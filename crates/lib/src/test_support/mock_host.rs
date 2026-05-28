//! Scripted, recording mock for `ToolHost`. Lets per-tool unit tests assert
//! both the call (what `Capability` the tool requested) and the formatting
//! (how the tool turns a `CapabilityResult` into its LLM-facing string).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::grants::Grants;
use crate::tool::ToolError;
use crate::tool_host::{Capability, CapabilityResult, ToolHost};

#[derive(Default)]
struct State {
    script: VecDeque<Result<CapabilityResult, ToolError>>,
    calls: Vec<Capability>,
}

/// `ToolHost` that returns scripted `CapabilityResult`s and records every
/// `Capability` it was asked to execute. Construct empty and `.push_*` the
/// expected results in order.
pub(crate) struct MockHost {
    state: Mutex<State>,
}

impl MockHost {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    pub fn push_shell(&self, stdout: &str, stderr: &str, exit_code: i32) {
        self.state
            .lock()
            .unwrap()
            .script
            .push_back(Ok(CapabilityResult::Shell(crate::tool_host::ShellOutput {
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
                exit_code,
            })));
    }

    pub fn push_file_read(&self, content: impl Into<Vec<u8>>) {
        self.state
            .lock()
            .unwrap()
            .script
            .push_back(Ok(CapabilityResult::FileRead(content.into())));
    }

    pub fn push_file_write(&self) {
        self.state
            .lock()
            .unwrap()
            .script
            .push_back(Ok(CapabilityResult::FileWrite));
    }

    pub fn push_http(&self, status: u16, body: impl Into<Vec<u8>>) {
        self.state
            .lock()
            .unwrap()
            .script
            .push_back(Ok(CapabilityResult::HttpResponse(
                crate::tool_host::HttpResponse {
                    status,
                    headers: Default::default(),
                    body: body.into(),
                },
            )));
    }

    pub fn push_err(&self, err: ToolError) {
        self.state.lock().unwrap().script.push_back(Err(err));
    }

    /// Snapshot of all capabilities the host has been asked to execute.
    pub fn recorded_calls(&self) -> Vec<Capability> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Convenience: get just the first recorded call.
    pub fn last_call(&self) -> Option<Capability> {
        self.state.lock().unwrap().calls.last().cloned()
    }
}

impl ToolHost for MockHost {
    fn request<'a>(
        &'a self,
        capability: &'a Capability,
        _grants: &'a Grants,
    ) -> Pin<Box<dyn Future<Output = Result<CapabilityResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.calls.push(capability.clone());
            state.script.pop_front().unwrap_or_else(|| {
                Err(ToolError::Execution(
                    "MockHost: script empty when host.request() called".into(),
                ))
            })
        })
    }

    fn name(&self) -> &str {
        "mock-host"
    }
}
