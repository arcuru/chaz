# Tool System

The tool system manages tool registration, policy enforcement, and per-agent tool visibility.

## Tool Trait

Every tool implements the `Tool` trait:

<!-- Code block ignored: trait definition for illustration -->

```rust,ignore
trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    fn execute(&self, arguments: Value, ctx: &ToolContext)
        -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send>>;
    fn default_policy(&self) -> ToolPolicy { /* defaults */ }
}
```

`ToolDescriptor` provides the tool's name, description, and a JSON Schema for its parameters. The LLM sees these as function definitions.

## ToolError

Tool execution failures are classified by a typed `ToolError` enum so the runtime can make retry/re-prompt decisions:

<!-- Code block ignored: enum definition for illustration -->

```rust,ignore
enum ToolError {
    Timeout { secs: u64 },       // retryable; runtime-enforced timeout fired
    ApprovalDenied,              // user/gate rejected the call
    Network(String),             // retryable; transport-level failure
    InvalidArgument(String),     // LLM supplied bad input
    Execution(String),           // generic operation failure
}
```

`From<String>` and `From<&str>` are implemented so tools that produce untyped errors (e.g. via `?` on helpers returning `Result<_, String>`) auto-convert to `Execution`. `ToolError::is_retryable()` returns true for `Timeout` and `Network`; the runtime retries `Network` errors once with a 500ms backoff (`Timeout` is deliberately NOT retried because the partial work may have succeeded). The built-in MCP wrapper classifies transport-origin errors (HTTP connection failures, subprocess pipe breakage) as `Network`; `web_fetch` does the same for reqwest send/body failures.

## Tool Policy

Each tool has a policy controlling its risk level, approval requirements, and execution timeout:

<!-- Code block ignored: struct definition for illustration -->

```rust,ignore
struct ToolPolicy {
    risk: RiskLevel,              // Low, Medium, High
    approval: ApprovalRequirement, // Never, UnlessAutoApproved, Always
    timeout: u64,                  // seconds
    sensitive_params: Vec<String>, // redacted in approval display
    rate_limit: Option<u32>,      // max calls per minute (None = unlimited)
}
```

Tools provide a `default_policy()`. Config-level overrides in `security.tool_policies` take precedence. The `ToolPolicyRegistry` resolves the effective policy per tool.

## ScopedTools and Narrowing

`ScopedTools` provides a filtered view of the tool registry:

```mermaid
graph TD
    REG[ToolRegistry<br/>9 tools] --> S1[ScopedTools: default<br/>all 9 tools]
    S1 -->|"narrow(['web_fetch', 'calculate', ...])"| S2[ScopedTools: researcher<br/>5 tools]
    S1 -->|"narrow(['shell', 'read_file', ...])"| S3[ScopedTools: coder<br/>4 tools]
```

When agent A spawns agent B, B's tools are computed as the intersection of A's current scope and B's `allowed_tools`. This is transitive -- tools can only decrease down the spawn tree.

<!-- Code block ignored: struct definition for illustration -->

```rust,ignore
struct ScopedTools {
    registry: Arc<ToolRegistry>,
    allowed: Option<Vec<String>>,  // None = all tools; supports globs like "namespace.*"
}

impl ScopedTools {
    fn narrow(&self, child_allowed: Option<&[String]>) -> ScopedTools {
        // Intersects parent's allowed with child's allowed
        // Glob patterns (e.g., "filesystem.*") are expanded against the registry
    }
}
```

Allowlist entries support glob patterns: `"filesystem.*"` matches all tools with that namespace prefix (requires a dot after the prefix). This is useful for MCP tool namespaces. Glob patterns work across all ScopedTools operations: `definitions()`, `get()`, `is_empty()`, and `narrow()`.

## ToolHost

The `ToolHost` trait is the sandboxed capability boundary between tools and the operating system. Tools request capabilities (shell commands, file I/O, HTTP requests) through the host rather than calling OS APIs directly:

```rust,ignore
trait ToolHost: Send + Sync {
    fn request(&self, capability: &Capability, grants: &Grants)
        -> Pin<Box<dyn Future<Output = Result<CapabilityResult, ToolError>>>>;
    fn name(&self) -> &str;
}

enum Capability {
    Shell { command: String, working_dir: Option<String> },
    FileRead { path: String },
    FileWrite { path: String, content: String },
    HttpRequest { url: String, method: String, headers: HashMap<String, String>, body: Option<String> },
}
```

The host enforces grants at the capability boundary — the tool says what it wants to do, the host decides whether to allow it and how to execute it. Two `ToolHost` implementations live in-tree:

| Host                 | Tier       | Isolation                                                                | Status                                                                                                       |
| -------------------- | ---------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| `NativeToolHost`     | Native     | In-process, grant enforcement only                                       | The only host wired in today — used for every built-in tool.                                                 |
| `BubblewrapToolHost` | OS sandbox | Wraps `Shell` capability invocations in `bwrap`; other caps pass through | Implemented in `crates/lib/src/bubblewrap_host.rs` but not yet config-selectable (`#[allow(dead_code)]`).    |

Tools (shell, web, file) call `ctx.host().request()` against the trait, so swapping host implementations needs no tool-code changes; what's missing is the config plumbing to choose anything other than `NativeToolHost`.

A separate WASM-tools path (`crates/lib/src/wasm_host.rs`: `WasmEngine` + `WasmTool`) lets a tool itself live in a Wasmtime sandbox while still routing its capability requests through whichever `ToolHost` chaz is using. The engine is wired but the config-driven loader is future work (the module is `#[allow(dead_code)]` today).

## ToolContext

The `ToolContext` is passed to every tool execution:

```rust,ignore
struct ToolContext {
    agent_name: String,                          // current agent
    call_depth: usize,                           // spawn nesting level
    max_call_depth: usize,                       // from agent config
    tools: ScopedTools,                          // narrowed tool set
    profile: ToolProfile,                        // how tool defs are presented
    session: Arc<Mutex<Session>>,                // for tools that write entries
    active_extensions: HashSet<String>,          // per-session active-set filter
    grants: Grants,                              // resolved per-call grants
    agent_grants: HashMap<String, Grants>,       // per-tool overlays from agent config
    host: Arc<dyn ToolHost>,                     // sandboxed capability boundary
}
```

Tools use `ctx.host()` for system access (shell, file, network) and `ctx.grants()` only for introspection (e.g., `describe_tool` listing available capabilities). `active_extensions` is built by `Server::active_extensions_for` and is what `ScopedTools` consults to hide tools from extensions disabled in this session.

## Adding a New Tool

Tools are published by [extensions](extensions.md). The `Tool` impl is the
tool's _behavior_; the extension is what wires it into the runtime registry.

1. Create `crates/lib/src/tools/my_tool.rs` implementing `Tool`.
2. Add `mod my_tool;` + `pub use` to `crates/lib/src/tools/mod.rs`.
3. Have an extension's `ExtensionInstance::tools()` return your `Tool`. Either
   add it to an existing built-in (e.g. `extensions/core.rs` for general
   tools) or create a new extension in `crates/lib/src/extensions/my_ext.rs` and
   register it in `extensions::all_builtins`.

At install time `install_all` drains every `Global` instance's `tools()`,
the hub publishes them through `tools_for_registry()`, and `main.rs` builds
the runtime `ToolRegistry` from that list. Per-session active-set filtering
hides tools whose owning extension is disabled for the session; policy
overrides in `security.tool_policies` apply on top of the tool's
`default_policy()`.
