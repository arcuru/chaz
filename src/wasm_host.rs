//! WASM tool host — VM-enforced sandbox for third-party tools.
#![allow(dead_code)] // Available but WASM tool loading from config is future work
//!
//! WASM modules are compiled to WebAssembly and run inside a Wasmtime
//! sandbox with explicit capability imports. The module has no access
//! to filesystem, network, or processes except through host functions
//! that the engine provides — and those host functions route through
//! the chaz [`ToolHost`] for grant enforcement.
//!
//! # WASM Tool ABI
//!
//! Each WASM module exports:
//!
//! | Export | Signature | Description |
//! |--------|-----------|-------------|
//! | `descriptor() -> i64` | (ptr, len) of JSON | Tool name, description, schema |
//! | `execute(ptr, len) -> i64` | args in, (ptr, len) result out | Execute the tool |
//! | `alloc(len) -> ptr` | allocate linear memory | Returns data to host |
//!
//! And may import these host functions:
//!
//! | Import | Signature |
//! |--------|-----------|
//! | `host_shell(ptr, len) -> i64` | Shell command |
//! | `host_read_file(ptr, len) -> i64` | File read |
//! | `host_write_file(p,l,c,cl) -> i32` | File write |
//! | `host_http(url,l, m,ml, b,bl) -> i64` | HTTP request |
//!
//! All strings are UTF-8. Return encoding: lower 32 bits = ptr, upper 32 = len.
//! The host copies data out before returning; WASM module can reuse buffers.
//!
//! # Fuel limiting
//!
//! Each WASM call is fuel-limited to prevent infinite loops in untrusted code.

use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::info;
use wasmtime::AsContextMut;

/// Default fuel (instruction count) per WASM execution.
const DEFAULT_FUEL: u64 = 50_000_000;

fn pack_result(ptr: u32, len: u32) -> i64 {
    ((len as i64) << 32) | (ptr as i64)
}

fn unpack_result(packed: i64) -> (u32, u32) {
    ((packed & 0xFFFF_FFFF) as u32, ((packed >> 32) & 0xFFFF_FFFF) as u32)
}

// ── WasmEngine ───────────────────────────────────────────────────

/// Manages the Wasmtime runtime and caches compiled WASM modules.
///
/// Available for use; WASM tool loading from config will be added in a
/// follow-up. The engine is constructed at startup in main.rs.
pub struct WasmEngine {
    engine: wasmtime::Engine,
    modules: HashMap<String, Arc<wasmtime::Module>>,
    fuel: u64,
}

impl WasmEngine {
    pub fn new() -> Result<Self, String> {
        let mut config = wasmtime::Config::default();
        config.consume_fuel(true);

        let engine = wasmtime::Engine::new(&config)
            .map_err(|e| format!("Failed to create Wasmtime engine: {e}"))?;

        Ok(Self {
            engine,
            modules: HashMap::new(),
            fuel: DEFAULT_FUEL,
        })
    }

    pub fn load_module(&mut self, name: &str, wasm_bytes: &[u8]) -> Result<(), String> {
        let module = wasmtime::Module::new(&self.engine, wasm_bytes)
            .map_err(|e| format!("WASM compile error for '{name}': {e}"))?;

        // Validate required exports
        for exp in &["alloc", "descriptor", "execute"] {
            module.get_export(exp).ok_or_else(|| {
                format!("Module '{name}' missing required export '{exp}'")
            })?;
        }

        self.modules.insert(name.to_string(), Arc::new(module));
        info!(name, "WASM module loaded");
        Ok(())
    }

    pub fn get_module(&self, name: &str) -> Option<Arc<wasmtime::Module>> {
        self.modules.get(name).cloned()
    }

    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    pub fn fuel(&self) -> u64 {
        self.fuel
    }
}

// ── WasmTool ─────────────────────────────────────────────────────

/// A tool implemented as a WASM module.
///
/// Conforms to the [`Tool`] trait. Each execution instantiates a fresh
/// [`wasmtime::Store`] with the per-call [`ToolContext`] wired into
/// host function imports. Host functions delegate capability requests
/// through the chaz [`ToolHost`].
#[allow(dead_code)]
pub struct WasmTool {
    name: String,
    module: Arc<wasmtime::Module>,
    engine: Arc<wasmtime::Engine>,
    fuel: u64,
    descriptor: ToolDescriptor,
}

impl WasmTool {
    pub fn new(
        name: String,
        module: Arc<wasmtime::Module>,
        engine: Arc<wasmtime::Engine>,
        fuel: u64,
    ) -> Result<Self, String> {
        let descriptor = Self::load_descriptor(&name, &module, &engine, fuel)?;
        Ok(Self {
            name,
            module,
            engine,
            fuel,
            descriptor,
        })
    }

    fn load_descriptor(
        name: &str,
        module: &wasmtime::Module,
        engine: &wasmtime::Engine,
        fuel: u64,
    ) -> Result<ToolDescriptor, String> {
        let mut store = wasmtime::Store::new(engine, ());
        let instance = wasmtime::Instance::new(&mut store, module, &[])
            .map_err(|e| format!("Failed to instantiate '{name}' for descriptor: {e}"))?;

        store.set_fuel(fuel).ok();

        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| format!("'{name}' missing alloc: {e}"))?;

        let desc_fn = instance
            .get_typed_func::<(), i64>(&mut store, "descriptor")
            .map_err(|e| format!("'{name}' missing descriptor: {e}"))?;

        let packed = desc_fn
            .call(&mut store, ())
            .map_err(|e| format!("'{name}' descriptor() failed: {e}"))?;

        let (ptr, len) = unpack_result(packed);
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| format!("'{name}' missing memory export"))?;

        let mut buf = vec![0u8; len as usize];
        memory
            .read(&store, ptr as usize, &mut buf)
            .map_err(|e| format!("Failed to read descriptor from '{name}': {e}"))?;

        let json_str =
            String::from_utf8(buf).map_err(|e| format!("'{name}' descriptor not UTF-8: {e}"))?;

        let json: Value = serde_json::from_str(&json_str)
            .map_err(|e| format!("'{name}' descriptor not valid JSON: {e}"))?;

        let tool_name = json["name"].as_str().unwrap_or(name).to_string();
        let description = json["description"].as_str().unwrap_or("").to_string();
        let parameters = json.get("parameters").cloned().unwrap_or_default();

        let _ = alloc.call(&mut store, len as i32);

        Ok(ToolDescriptor {
            name: tool_name,
            description,
            parameters,
        })
    }
}

impl Tool for WasmTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    fn default_policy(&self) -> ToolPolicy {
        ToolPolicy {
            risk: crate::tool::RiskLevel::Medium,
            approval: crate::tool::ApprovalRequirement::UnlessAutoApproved,
            timeout: 60,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let args_json = serde_json::to_string(&arguments)
                .map_err(|e| ToolError::InvalidArgument(format!("args: {e}")))?;

            let host_state = WasmHostState { ctx: ctx.clone() };
            let mut store = wasmtime::Store::new(&self.engine, host_state);
            store.set_fuel(self.fuel).ok();

            let mut linker = wasmtime::Linker::new(&self.engine);
            add_host_functions(&mut linker)?;

            let instance = linker
                .instantiate(&mut store, &self.module)
                .map_err(|e| ToolError::Execution(format!("WASM instantiation: {e}")))?;

            let execute_fn = instance
                .get_typed_func::<(i32, i32), i64>(&mut store, "execute")
                .map_err(|_| ToolError::Execution("Missing execute export".into()))?;

            let alloc = instance
                .get_typed_func::<i32, i32>(&mut store, "alloc")
                .map_err(|_| ToolError::Execution("Missing alloc export".into()))?;

            let args_bytes = args_json.as_bytes();
            let args_ptr = alloc
                .call(&mut store, args_bytes.len() as i32)
                .map_err(|e| ToolError::Execution(format!("alloc: {e}")))?;

            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| ToolError::Execution("Missing memory export".into()))?;

            memory
                .write(&mut store, args_ptr as usize, args_bytes)
                .map_err(|e| ToolError::Execution(format!("memory write: {e}")))?;

            let packed = execute_fn
                .call(&mut store, (args_ptr, args_bytes.len() as i32))
                .map_err(|e| ToolError::Execution(format!("execute: {e}")))?;

            let (result_ptr, result_len) = unpack_result(packed);
            let mut result_buf = vec![0u8; result_len as usize];
            memory
                .read(&store, result_ptr as usize, &mut result_buf)
                .map_err(|e| ToolError::Execution(format!("result read: {e}")))?;

            String::from_utf8(result_buf)
                .map_err(|e| ToolError::Execution(format!("UTF-8: {e}")))
        })
    }
}

// ── Host functions ───────────────────────────────────────────────

#[allow(dead_code)]
struct WasmHostState {
    ctx: ToolContext,
}

#[allow(dead_code)]
fn add_host_functions(
    linker: &mut wasmtime::Linker<WasmHostState>,
) -> Result<(), ToolError> {
    // host_shell(cmd_ptr: i32, cmd_len: i32) -> i64
    linker
        .func_wrap_async(
            "env",
            "host_shell",
            |mut caller: wasmtime::Caller<'_, WasmHostState>,
             (cmd_ptr, cmd_len): (i32, i32)|
             -> Box<dyn Future<Output = i64> + Send + '_> {
                Box::new(async move {
                    let (ctx, memory) = match host_ctx_and_memory(&mut caller) {
                        Ok(v) => v,
                        Err(_) => return -1,
                    };
                    let cmd = match host_read_str(&caller, &memory, cmd_ptr, cmd_len) {
                        Some(s) => s,
                        None => return -1,
                    };
                    let result = ctx
                        .host()
                        .request(
                            &crate::tool_host::Capability::Shell {
                                command: cmd,
                                working_dir: None,
                            },
                            ctx.grants(),
                        )
                        .await;
                    host_write_result(caller, result)
                })
            },
        )
        .map_err(|e| ToolError::Execution(format!("host_shell: {e}")))?;

    // host_read_file(path_ptr: i32, path_len: i32) -> i64
    linker
        .func_wrap_async(
            "env",
            "host_read_file",
            |mut caller: wasmtime::Caller<'_, WasmHostState>,
             (path_ptr, path_len): (i32, i32)|
             -> Box<dyn Future<Output = i64> + Send + '_> {
                Box::new(async move {
                    let (ctx, memory) = match host_ctx_and_memory(&mut caller) {
                        Ok(v) => v,
                        Err(_) => return -1,
                    };
                    let path = match host_read_str(&caller, &memory, path_ptr, path_len) {
                        Some(s) => s,
                        None => return -1,
                    };
                    let result = ctx
                        .host()
                        .request(
                            &crate::tool_host::Capability::FileRead { path },
                            ctx.grants(),
                        )
                        .await;
                    host_write_result(caller, result)
                })
            },
        )
        .map_err(|e| ToolError::Execution(format!("host_read_file: {e}")))?;

    // host_write_file(path_p, path_l, content_p, content_l) -> i32
    linker
        .func_wrap_async(
            "env",
            "host_write_file",
            |mut caller: wasmtime::Caller<'_, WasmHostState>,
             (path_ptr, path_len, content_ptr, content_len): (i32, i32, i32, i32)|
             -> Box<dyn Future<Output = i32> + Send + '_> {
                Box::new(async move {
                    let (ctx, memory) = match host_ctx_and_memory(&mut caller) {
                        Ok(v) => v,
                        Err(_) => return -1,
                    };
                    let path = match host_read_str(&caller, &memory, path_ptr, path_len) {
                        Some(s) => s,
                        None => return -1,
                    };
                    let content =
                        match host_read_str(&caller, &memory, content_ptr, content_len) {
                            Some(s) => s,
                            None => return -1,
                        };
                    let result = ctx
                        .host()
                        .request(
                            &crate::tool_host::Capability::FileWrite { path, content },
                            ctx.grants(),
                        )
                        .await;
                    match result {
                        Ok(_) => 0,
                        Err(_) => -1,
                    }
                })
            },
        )
        .map_err(|e| ToolError::Execution(format!("host_write_file: {e}")))?;

    // host_http(url_p,l, method_p,l, body_p,l) -> i64
    linker
        .func_wrap_async(
            "env",
            "host_http",
            |mut caller: wasmtime::Caller<'_, WasmHostState>,
             (url_ptr, url_len, method_ptr, method_len, body_ptr, body_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )|
             -> Box<dyn Future<Output = i64> + Send + '_> {
                Box::new(async move {
                    let (ctx, memory) = match host_ctx_and_memory(&mut caller) {
                        Ok(v) => v,
                        Err(_) => return -1,
                    };
                    let url = match host_read_str(&caller, &memory, url_ptr, url_len) {
                        Some(s) => s,
                        None => return -1,
                    };
                    let method =
                        match host_read_str(&caller, &memory, method_ptr, method_len) {
                            Some(s) => s,
                            None => return -1,
                        };
                    let body = if body_ptr == 0 || body_len == 0 {
                        None
                    } else {
                        match host_read_str(&caller, &memory, body_ptr, body_len) {
                            Some(s) => Some(s),
                            None => return -1,
                        }
                    };
                    let result = ctx
                        .host()
                        .request(
                            &crate::tool_host::Capability::HttpRequest {
                                url,
                                method,
                                headers: HashMap::new(),
                                body,
                            },
                            ctx.grants(),
                        )
                        .await;
                    host_write_result(caller, result)
                })
            },
        )
        .map_err(|e| ToolError::Execution(format!("host_http: {e}")))?;

    Ok(())
}

#[allow(dead_code)]
fn host_ctx_and_memory(
    caller: &mut wasmtime::Caller<'_, WasmHostState>,
) -> Result<(ToolContext, wasmtime::Memory), ()> {
    let ctx = caller.data().ctx.clone();
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or(())?;
    Ok((ctx, memory))
}

#[allow(dead_code)]
fn host_read_str(
    store: &impl wasmtime::AsContext,
    memory: &wasmtime::Memory,
    ptr: i32,
    len: i32,
) -> Option<String> {
    if ptr < 0 || len <= 0 || len > 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    memory.read(store, ptr as usize, &mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Write a capability result to WASM memory as JSON, return packed (ptr, len).
#[allow(dead_code)]
fn host_write_result(
    mut caller: wasmtime::Caller<'_, WasmHostState>,
    result: Result<crate::tool_host::CapabilityResult, ToolError>,
) -> i64 {
    let json = match result {
        Ok(r) => match r {
            crate::tool_host::CapabilityResult::Shell(o) => serde_json::json!({
                "ok": true, "stdout": o.stdout, "stderr": o.stderr, "exit_code": o.exit_code
            }),
            crate::tool_host::CapabilityResult::FileRead(data) => serde_json::json!({
                "ok": true, "content": String::from_utf8_lossy(&data)
            }),
            crate::tool_host::CapabilityResult::FileWrite => serde_json::json!({"ok": true}),
            crate::tool_host::CapabilityResult::HttpResponse(r) => serde_json::json!({
                "ok": true, "status": r.status,
                "body": String::from_utf8_lossy(&r.body),
                "headers": r.headers
            }),
        },
        Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
    };

    let json_str = serde_json::to_string(&json).unwrap_or_default();
    let bytes = json_str.as_bytes();

    // Use caller to get export "alloc" and call it
    let alloc_ptr = {
        let alloc = match caller.get_export("alloc") {
            Some(e) => match e.into_func() {
                Some(f) => f,
                None => return -1,
            },
            None => return -1,
        };
        // Call alloc(bytes.len()) via the raw Val API since the typed func
        // API wraps store differently.
        let mut results = [wasmtime::Val::I32(0)];
        match alloc.call(
            caller.as_context_mut(),
            &[wasmtime::Val::I32(bytes.len() as i32)],
            &mut results,
        ) {
            Ok(_) => {}
            Err(_) => return -1,
        };
        results[0].i32().unwrap_or(-1)
    };

    if alloc_ptr < 0 {
        return -1;
    }

    let memory = match caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
    {
        Some(m) => m,
        None => return -1,
    };

    if memory
        .write(caller.as_context_mut(), alloc_ptr as usize, bytes)
        .is_err()
    {
        return -1;
    }

    pack_result(alloc_ptr as u32, bytes.len() as u32)
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal WAT module: descriptor returns {"n":"t","d":"","p":{}}, execute echoes args.
    const MINIMAL_TOOL_WAT: &str = r#"
(module
  (memory (export "memory") 1)

  ;; Bump allocator
  (global $alloc_ptr (mut i32) (i32.const 1024))
  (func (export "alloc") (param $len i32) (result i32)
    (global.get $alloc_ptr)
    (global.set $alloc_ptr (i32.add (global.get $alloc_ptr) (local.get $len)))
  )
  (func (export "dealloc") (param $ptr i32) (param $len i32))

  ;; descriptor() -> packed(ptr, len) of JSON: {"n":"t","d":"","p":{}}
  (func (export "descriptor") (result i64)
    (local $ptr i32)
    (call 0 (i32.const 23))  ;; alloc is function 0
    (local.set $ptr)
    ;; {"n":"t","d":"","p":{}} = 23 bytes
    (i32.store8 (local.get $ptr) (i32.const 123))
    (i32.store8 offset=1 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=2 (local.get $ptr) (i32.const 110))
    (i32.store8 offset=3 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=4 (local.get $ptr) (i32.const 58))
    (i32.store8 offset=5 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=6 (local.get $ptr) (i32.const 116))
    (i32.store8 offset=7 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=8 (local.get $ptr) (i32.const 44))
    (i32.store8 offset=9 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=10 (local.get $ptr) (i32.const 100))
    (i32.store8 offset=11 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=12 (local.get $ptr) (i32.const 58))
    (i32.store8 offset=13 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=14 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=15 (local.get $ptr) (i32.const 44))
    (i32.store8 offset=16 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=17 (local.get $ptr) (i32.const 112))
    (i32.store8 offset=18 (local.get $ptr) (i32.const 34))
    (i32.store8 offset=19 (local.get $ptr) (i32.const 58))
    (i32.store8 offset=20 (local.get $ptr) (i32.const 123))
    (i32.store8 offset=21 (local.get $ptr) (i32.const 125))
    (i32.store8 offset=22 (local.get $ptr) (i32.const 125))
    (i64.or
      (i64.extend_i32_u (local.get $ptr))
      (i64.shl (i64.const 23) (i64.const 32))
    )
  )

  ;; execute(args_ptr, args_len) -> packed(result_ptr, result_len)
  ;; Returns: "echo: <args>" string
  (func (export "execute") (param $args_ptr i32) (param $args_len i32) (result i64)
    (local $result_ptr i32)
    (local $i i32)

    ;; Allocate space for "echo: " + args
    (call 0 (i32.add (local.get $args_len) (i32.const 7)))  ;; alloc is function 0
    (local.set $result_ptr)

    ;; Write "echo: " prefix
    (i32.store8 (local.get $result_ptr) (i32.const 101))       ;; e
    (i32.store8 offset=1 (local.get $result_ptr) (i32.const 99)) ;; c
    (i32.store8 offset=2 (local.get $result_ptr) (i32.const 104)) ;; h
    (i32.store8 offset=3 (local.get $result_ptr) (i32.const 111)) ;; o
    (i32.store8 offset=4 (local.get $result_ptr) (i32.const 58))  ;; :
    (i32.store8 offset=5 (local.get $result_ptr) (i32.const 32))  ;; space

    ;; Copy args bytes
    (local.set $i (i32.const 0))
    (block $done
      (loop $copy
        (br_if $done (i32.ge_u (local.get $i) (local.get $args_len)))
        (i32.store8
          (i32.add (local.get $result_ptr) (i32.add (i32.const 6) (local.get $i)))
          (i32.load8_u (i32.add (local.get $args_ptr) (local.get $i)))
        )
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)
      )
    )

    (i64.or
      (i64.extend_i32_u (local.get $result_ptr))
      (i64.shl
        (i64.extend_i32_u (i32.add (local.get $args_len) (i32.const 6)))
        (i64.const 32)
      )
    )
  )
)
"#;

    #[test]
    fn test_compile_minimal_module() {
        let wasm_bytes = wat::parse_str(MINIMAL_TOOL_WAT).expect("Failed to parse WAT");
        let mut engine = WasmEngine::new().expect("Failed to create engine");
        engine
            .load_module("test_tool", &wasm_bytes)
            .expect("Failed to load module");

        let module = engine.get_module("test_tool").expect("Module not found");
        let desc = WasmTool::load_descriptor("test_tool", &module, engine.engine(), engine.fuel())
            .expect("Failed to load descriptor");

        // The WAT uses abbreviated JSON keys (n, d, p) to stay under 23 bytes.
        // The loader reads the "name" key which falls back to the module name.
        assert_eq!(desc.name, "test_tool");
        assert_eq!(desc.description, "");
    }

    #[test]
    fn test_missing_exports_rejected() {
        let bad_wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
)
"#;
        let wasm_bytes = wat::parse_str(bad_wat).expect("Failed to parse WAT");
        let mut engine = WasmEngine::new().expect("Failed to create engine");
        let err = engine
            .load_module("bad_tool", &wasm_bytes)
            .expect_err("Should reject");
        assert!(
            err.contains("missing required export"),
            "Error: {err}"
        );
    }

    #[test]
    fn test_pack_unpack_roundtrip() {
        let packed = pack_result(0x1234, 0x5678);
        let (ptr, len) = unpack_result(packed);
        assert_eq!(ptr, 0x1234);
        assert_eq!(len, 0x5678);
    }

    #[test]
    fn test_pack_unpack_zeros() {
        assert_eq!(unpack_result(pack_result(0, 0)), (0, 0));
    }

    #[test]
    fn test_pack_unpack_max() {
        assert_eq!(
            unpack_result(pack_result(0xFFFF_FFFF, 0xFFFF_FFFF)),
            (0xFFFF_FFFF, 0xFFFF_FFFF)
        );
    }
}
