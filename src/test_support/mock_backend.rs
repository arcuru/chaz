//! Scripted, recording mock that plugs into `BackendManager::with_mock`.
//!
//! Tests push expected `LLMResponse`s (or errors) onto the script queue, then
//! drive the runtime; the mock consumes them in FIFO order and records every
//! call's inputs for assertion. If the script is empty when the runtime calls,
//! the mock returns a `Configuration` error so the test fails loudly rather
//! than hanging.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::backends::BackendDispatch;
use crate::error::LlmError;
use crate::runtime::{LLMResponse, ResponseMetadata, RuntimeMessage, ToolCallRequest};
use crate::tool::ToolDefinition;

/// Inputs observed by `MockBackend::chat_with_tools` on a single call.
#[derive(Clone, Debug)]
pub(crate) struct RecordedCall {
    pub messages: Vec<RuntimeMessage>,
    pub tools: Vec<ToolDefinition>,
    pub model: String,
}

#[derive(Default)]
struct State {
    script: std::collections::VecDeque<Result<LLMResponse, LlmError>>,
    calls: Vec<RecordedCall>,
}

/// Recording mock LLM backend with a FIFO response queue.
pub(crate) struct MockBackend {
    state: Mutex<State>,
    default_model: String,
    supports_tools: bool,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            default_model: "mock-model".to_string(),
            supports_tools: true,
        }
    }

    /// Queue a final-text response. Convenience for the common case.
    pub fn push_text(&self, content: impl Into<String>) {
        let model = self.default_model.clone();
        self.state
            .lock()
            .unwrap()
            .script
            .push_back(Ok(LLMResponse::Text {
                content: content.into(),
                metadata: Some(ResponseMetadata {
                    model,
                    ..Default::default()
                }),
            }));
    }

    /// Queue a tool-calls response. Each `(id, name, args_json)` tuple becomes
    /// one `ToolCallRequest`; the runtime will dispatch them in order.
    pub fn push_tool_calls<I>(&self, calls: I)
    where
        I: IntoIterator<Item = (String, String, String)>,
    {
        let tool_calls: Vec<ToolCallRequest> = calls
            .into_iter()
            .map(|(id, name, arguments)| ToolCallRequest {
                id,
                name,
                arguments,
            })
            .collect();
        let model = self.default_model.clone();
        self.state
            .lock()
            .unwrap()
            .script
            .push_back(Ok(LLMResponse::ToolCalls {
                content: None,
                tool_calls,
                provider_extra: serde_json::Map::new(),
                metadata: Some(ResponseMetadata {
                    model,
                    ..Default::default()
                }),
            }));
    }

    /// Queue an error response for the next call.
    pub fn push_err(&self, err: LlmError) {
        self.state.lock().unwrap().script.push_back(Err(err));
    }

    /// Snapshot of all calls recorded so far.
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Number of responses still queued.
    pub fn pending(&self) -> usize {
        self.state.lock().unwrap().script.len()
    }
}

impl BackendDispatch for MockBackend {
    fn supports_tools(&self) -> bool {
        self.supports_tools
    }

    fn chat_with_tools<'a>(
        &'a self,
        messages: &'a [RuntimeMessage],
        tools: &'a [ToolDefinition],
        model: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LLMResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.calls.push(RecordedCall {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                model: model.to_string(),
            });
            state.script.pop_front().unwrap_or_else(|| {
                Err(LlmError::Configuration {
                    message: "MockBackend: chat_with_tools called with empty script".into(),
                })
            })
        })
    }
}
