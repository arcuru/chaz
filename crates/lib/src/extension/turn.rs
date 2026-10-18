//! Read-only successful-turn observation and session-scoped host operations.
//! This is not a context transform: the main request and persisted transcript
//! are never changed. In-process extensions remain trusted native code.

use super::caps::CapFuture;
use crate::backends::BackendManager;
use crate::runtime::{LLMResponse, ResponseMetadata, RuntimeMessage};
use crate::security::SecurityContext;
use crate::session::{SessionRegistry, read_session_name_from_db};
use crate::tool::ToolDefinition;
use eidetica::Database;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// An instance endpoint fired once when a runtime turn succeeds, before the
/// runtime returns. Errors, panics and a 15-second deadline are fail-open.
pub trait TurnComplete: Send + Sync {
    fn on_turn_complete<'a>(&'a self, ctx: &'a TurnCompleteContext<'_>) -> CapFuture<'a, ()>;
}

pub struct Completion {
    pub text: String,
    pub metadata: Option<ResponseMetadata>,
}

/// Only the calling turn's context and operations. No registry, backend,
/// session DB handle, other-session identifier or provider reasoning escapes
/// through this API. Host construction and fields carrying authority are private.
pub struct TurnCompleteContext<'a> {
    pub agent_name: &'a str,
    pub model: Option<&'a str>,
    pub call_depth: usize,
    /// Final accepted request messages plus the public reply. Opaque provider
    /// extras are stripped; projection, summaries and tool-result wrappers
    /// match what the main model saw, including this turn's tool exchanges.
    pub messages: &'a [RuntimeMessage],
    /// Definitions available to the main model, not executable by this endpoint.
    pub tools: &'a [ToolDefinition],
    pub(crate) extension: &'a str,
    pub(crate) db: &'a Database,
    pub(crate) registry: &'a SessionRegistry,
    pub(crate) backend: &'a BackendManager,
    pub(crate) security: &'a SecurityContext,
    pub(crate) completion_used: AtomicBool,
}

impl TurnCompleteContext<'_> {
    pub async fn name(&self) -> anyhow::Result<Option<String>> {
        read_session_name_from_db(self.db).await
    }

    /// Never replaces a name, even if the user renamed during inference.
    /// Names are single-line, nonempty, at most 80 characters, and leak-scanned.
    pub async fn set_name_if_unnamed(&self, name: &str) -> anyhow::Result<bool> {
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty()
                && name.chars().count() <= 80
                && !name
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')),
            "session name must be 1–80 characters on one line"
        );
        let scanned = self
            .security
            .leak_detector
            .scan(name)
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(scanned == name, "session name contained a secret");
        self.registry
            .set_session_name_if_unnamed(&self.db.root_id().to_string(), scanned)
            .await
    }

    pub async fn settings(&self) -> anyhow::Result<Value> {
        super::read_settings_checked(self.db, self.extension).await
    }

    /// Merge a patch into this extension's current settings.
    pub async fn update_settings(&self, patch: Value) -> anyhow::Result<()> {
        let mut settings = self.settings().await?;
        let object = settings
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("extension settings must be an object"))?;
        object.extend(
            patch
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("settings patch must be an object"))?
                .clone(),
        );
        super::write_settings(self.db, self.extension, settings).await
    }

    /// One tool-free model call per endpoint invocation, limited to 10 seconds
    /// and 1–512 output tokens (including reasoning on OpenAI). Uses normal
    /// backend routing and model reasoning configuration, never the ReAct loop.
    /// The whole context is sent, or the call is refused if it won't fit a
    /// known model window; no last-message excerpt or silent truncation.
    pub async fn complete(
        &self,
        instruction: &str,
        model: Option<&str>,
        max_output_tokens: u32,
    ) -> anyhow::Result<Completion> {
        anyhow::ensure!(
            (1..=512).contains(&max_output_tokens),
            "auxiliary output budget must be 1–512 tokens"
        );
        anyhow::ensure!(
            !self.completion_used.swap(true, Ordering::AcqRel),
            "only one auxiliary completion is allowed per endpoint invocation"
        );
        let default_model = self.backend.default_model();
        let model = model.or(self.model).or(default_model.as_deref());
        let mut messages = self.messages.to_vec();
        // Definitions are contextual data, not an executable tools parameter.
        let tools = serde_json::to_string(self.tools)?;
        messages.push(RuntimeMessage::User(format!("{instruction}\n\nTools available in the preceding conversation (reference only):\n{tools}")));
        if let Some(window) = model.and_then(|m| self.backend.context_window(m)) {
            let cost = crate::context::estimate_request_tokens(&messages, &[]);
            anyhow::ensure!(
                cost + max_output_tokens as usize <= window,
                "full turn context exceeds auxiliary model window"
            );
        }
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            self.backend
                .complete_bounded(model, &messages, max_output_tokens),
        )
        .await??;
        match response {
            LLMResponse::Text {
                content,
                mut metadata,
                ..
            } => {
                if let Some(meta) = &mut metadata {
                    meta.extra.clear();
                }
                Ok(Completion {
                    text: content,
                    metadata,
                })
            }
            LLMResponse::ToolCalls { .. } => {
                anyhow::bail!("auxiliary model requested tools; none were executed")
            }
        }
    }
}

/// Copy the model-facing projection without exposing hidden provider fields.
pub(crate) fn public_messages(messages: &[RuntimeMessage], reply: &str) -> Vec<RuntimeMessage> {
    let mut out = messages.to_vec();
    for message in &mut out {
        match message {
            RuntimeMessage::AssistantToolCalls {
                provider_extra,
                exchange_id,
                ..
            } => {
                provider_extra.clear();
                *exchange_id = None;
            }
            RuntimeMessage::AssistantWithExtra { content, .. } => {
                *message = RuntimeMessage::Assistant(content.take().unwrap_or_default());
            }
            _ => {}
        }
    }
    out.push(RuntimeMessage::Assistant(reply.to_owned()));
    out
}
