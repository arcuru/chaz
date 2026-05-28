/// Manage all the backends for chaz.
///
/// This module is responsible for handling dispatch, validation, and general management for all the different backends
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::debug;

use crate::{
    config::Backend,
    error::LlmError,
    openai::OpenAI,
    runtime::{LLMResponse, RuntimeMessage},
    security::SecretStore,
    tool::ToolDefinition,
};

/// Role of a message in a legacy `ChatContext`. Mirrors the OpenAI chat
/// completions roles for System/User/Assistant conversations. The tool and
/// function roles aren't used by the legacy no-tools path; the ReAct loop
/// uses `RuntimeMessage` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

impl MessageRole {
    /// Wire string used by OpenAI-compatible APIs.
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        }
    }
}

/// LLM backend trait.
///
/// Uses native `async fn` (not `impl Future`) for ergonomic implementor code.
/// Not dyn-compatible by design — `BackendDispatch` below is the dyn-safe
/// dispatch shim used by `BackendManager::with_mock`.
#[allow(async_fn_in_trait)]
pub trait LLMBackend {
    fn list_models(&self) -> Vec<String>;
    fn default_model(&self) -> Option<String>;
    /// Execute a simple chat request (no tools). Used by /compact and Matrix commands.
    async fn execute(&self, context: &ChatContext) -> Result<String, LlmError>;

    /// Whether this backend supports tool/function calling
    fn supports_tools(&self) -> bool {
        false
    }

    /// Execute a single LLM call with tool definitions (ReAct loop step).
    /// Returns structured response with text or tool calls.
    async fn chat_with_tools(
        &self,
        _messages: &[RuntimeMessage],
        _tools: &[ToolDefinition],
        _model: &str,
    ) -> Result<LLMResponse, LlmError> {
        Err(LlmError::Configuration {
            message: "Tool calling not supported by this backend".to_string(),
        })
    }
}

/// Dyn-compatible dispatch trait used by `BackendManager::with_mock` to route
/// LLM calls through an arbitrary implementation. `LLMBackend` itself is not
/// dyn-compatible (native `async fn` in trait), so the integration-test mock
/// implements this narrower interface — which is exactly what `BackendManager`
/// needs for its ReAct-loop call sites.
pub trait BackendDispatch: Send + Sync {
    fn supports_tools(&self) -> bool;
    fn chat_with_tools<'a>(
        &'a self,
        messages: &'a [RuntimeMessage],
        tools: &'a [ToolDefinition],
        model: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LLMResponse, LlmError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct BackendManager {
    backends: Vec<Backend>,
    secrets: SecretStore,
    /// Optional override used by integration tests to bypass the OpenAI dispatch
    /// path. When set, `chat_with_tools_for_model` and `supports_tools_for_model`
    /// route through this trait object instead of constructing an `OpenAI` from
    /// the backend config. Production code never sets this; constructed only
    /// via `BackendManager::with_mock`.
    mock: Option<Arc<dyn BackendDispatch>>,
}

/// A generic Message
#[derive(Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let role = match self.role {
            MessageRole::User => "USER",
            MessageRole::Assistant => "ASSISTANT",
            MessageRole::System => "SYSTEM",
        };
        write!(f, "{}: {}", role, self.content)
    }
}

impl Message {
    /// Create a new message
    pub fn new<S: Into<String>>(role: MessageRole, content: S) -> Message {
        Message {
            role,
            content: content.into(),
        }
    }
}

/// The ChatContext is an internal representation of a ChatCompletion request.
///
/// The frontend converts to this format, and the backend converts this to the backend-specific APIs.
pub struct ChatContext {
    pub messages: Vec<Message>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
}

impl ChatContext {
    /// Convert messages into a single string.
    pub fn string_prompt(&self) -> String {
        let mut prompt = String::new();
        for message in self.messages.iter() {
            prompt.push_str(&format!("{}\n", message))
        }
        prompt.push_str("ASSISTANT: ");
        prompt
    }
}

impl BackendManager {
    /// Create a new backend manager
    pub fn new(backends: &Option<Vec<Backend>>, secrets: SecretStore) -> Self {
        Self {
            backends: backends.as_ref().cloned().unwrap_or_default(),
            secrets,
            mock: None,
        }
    }

    /// Construct a BackendManager that dispatches all LLM calls to `mock`.
    /// Bypasses the OpenAI construction path; used by integration tests to
    /// drive the runtime without a real LLM. The `secrets` argument is held
    /// for type compatibility but unused on the mock path.
    pub fn with_mock(mock: Arc<dyn BackendDispatch>, secrets: SecretStore) -> Self {
        Self {
            backends: Vec::new(),
            secrets,
            mock: Some(mock),
        }
    }

    /// Lists all known backends
    pub fn list_known_backends(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.get_name().clone()).collect()
    }

    /// Lists all known models
    ///
    /// Models may be valid even if they aren't listed
    pub fn list_known_models(&self) -> Vec<String> {
        if self.backends.len() == 1 {
            OpenAI::new(&self.backends[0], &self.secrets).list_models()
        } else {
            self.backends
                .iter()
                .flat_map(|backend| {
                    let prefix = backend.get_name();
                    OpenAI::new(backend, &self.secrets)
                        .list_models()
                        .into_iter()
                        .map(move |model| format!("{}:{}", prefix, model))
                })
                .collect()
        }
    }

    /// Returns true if the model is known
    pub fn is_known_model(&self, model: &str) -> bool {
        self.list_known_models().contains(&model.to_string())
    }

    /// Validate that the model name is valid
    pub fn validate_model(&self, model: &str) -> Result<(), String> {
        if self.is_known_model(model) || self.backends.len() <= 1 {
            return Ok(());
        }
        // Multiple backends: name must be prefixed by backend name
        for backend in &self.backends {
            if model.starts_with(&format!("{}:", backend.name.as_deref().unwrap_or(""))) {
                return Ok(());
            }
        }
        Err("Multiple backends exist, please specify the model name with the backend prepended, e.g. openrouter:model-name".to_string())
    }

    /// Get the default model
    pub fn default_model(&self) -> Option<String> {
        let backend = self.backends.first()?;
        let model = OpenAI::new(backend, &self.secrets).default_model()?;
        if self.backends.len() == 1 {
            Some(model)
        } else {
            Some(format!("{}:{}", backend.get_name(), model))
        }
    }

    /// Select the backend based on a model name.
    /// Multi-backend setups use "backend_name:model" prefixed names.
    fn select_backend_for_model(&self, model: Option<&str>) -> &Backend {
        if let Some(model) = model {
            self.backends
                .iter()
                .find(|backend| {
                    backend.name.as_deref() == Some(model.split(":").next().unwrap_or(""))
                })
                .unwrap_or(&self.backends[0])
        } else {
            &self.backends[0]
        }
    }

    /// Select the backend based on the model name in a ChatContext.
    /// Used by legacy code paths (Matrix commands, /compact).
    fn select_backend(&self, context: &ChatContext) -> &Backend {
        self.select_backend_for_model(context.model.as_deref())
    }

    /// Execute a ChatContext (simple, no tools).
    /// Used by Matrix commands and /compact — not by the runtime.
    pub async fn execute(&self, context: &ChatContext) -> Result<String, LlmError> {
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".to_string(),
            });
        }
        let backend = self.select_backend(context);
        OpenAI::new(backend, &self.secrets).execute(context).await
    }

    /// Whether the backend for the given model supports tool/function calling.
    pub fn supports_tools_for_model(&self, model: Option<&str>) -> bool {
        if let Some(mock) = &self.mock {
            return mock.supports_tools();
        }
        if self.backends.is_empty() {
            return false;
        }
        let backend = self.select_backend_for_model(model);
        OpenAI::new(backend, &self.secrets).supports_tools()
    }

    /// Resolve a model name: strip backend prefix, fall back to default.
    pub fn resolve_model_name(&self, model: Option<&str>) -> String {
        if self.backends.is_empty() {
            return String::new();
        }
        let backend = self.select_backend_for_model(model);
        let model_prefix = backend.name.clone().unwrap_or_else(|| "openai".to_string());
        let mut resolved = model.unwrap_or("").to_string();
        resolved = resolved
            .trim_start_matches(&format!("{model_prefix}:"))
            .to_string();
        if resolved.is_empty() {
            resolved = OpenAI::new(backend, &self.secrets)
                .default_model()
                .unwrap_or_default();
        }
        debug!(
            requested = ?model,
            resolved = %resolved,
            backend = %backend.get_name(),
            "Model resolved"
        );
        resolved
    }

    /// Maximum retry attempts for transient errors on the backend for the given model.
    pub fn max_retries_for_model(&self, model: Option<&str>) -> u32 {
        if self.backends.is_empty() {
            return 3;
        }
        self.select_backend_for_model(model).max_retries()
    }

    /// Execute a single LLM call with tool definitions (for ReAct loop).
    pub async fn chat_with_tools_for_model(
        &self,
        model: Option<&str>,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        resolved_model: &str,
    ) -> Result<LLMResponse, LlmError> {
        if let Some(mock) = &self.mock {
            return mock.chat_with_tools(messages, tools, resolved_model).await;
        }
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".to_string(),
            });
        }
        let backend = self.select_backend_for_model(model);
        OpenAI::new(backend, &self.secrets)
            .chat_with_tools(messages, tools, resolved_model)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, BackendType, Model};
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    async fn empty_secrets() -> SecretStore {
        let (_instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("t"))
                .await
                .unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = eidetica::crdt::Doc::new();
        s.set("name", "central");
        let db = user.create_database(s, &key).await.unwrap();
        SecretStore::new(db).await
    }

    fn backend(name: &str, models: &[&str]) -> Backend {
        let mut b = Backend::new(BackendType::OpenAICompatible);
        b.name = Some(name.to_string());
        b.models = Some(
            models
                .iter()
                .map(|m| Model {
                    name: m.to_string(),
                })
                .collect(),
        );
        b
    }

    // ================================================================
    // Message
    // ================================================================

    #[test]
    fn message_display_formats_role_uppercase() {
        let m = Message::new(MessageRole::User, "hello");
        assert_eq!(m.to_string(), "USER: hello");
        let m = Message::new(MessageRole::Assistant, "hi");
        assert_eq!(m.to_string(), "ASSISTANT: hi");
        let m = Message::new(MessageRole::System, "ok");
        assert_eq!(m.to_string(), "SYSTEM: ok");
    }

    // ================================================================
    // ChatContext::string_prompt
    // ================================================================

    #[test]
    fn string_prompt_concatenates_with_trailing_assistant() {
        let ctx = ChatContext {
            messages: vec![
                Message::new(MessageRole::System, "be helpful"),
                Message::new(MessageRole::User, "hi"),
            ],
            model: None,
            system_prompt: None,
        };
        let out = ctx.string_prompt();
        assert!(out.starts_with("SYSTEM: be helpful"));
        assert!(out.contains("\nUSER: hi\n"));
        assert!(out.ends_with("ASSISTANT: "));
    }

    // ================================================================
    // BackendManager construction + listing
    // ================================================================

    #[tokio::test]
    async fn empty_backend_manager_reports_nothing() {
        let secrets = empty_secrets().await;
        let mgr = BackendManager::new(&None, secrets);
        assert!(mgr.list_known_backends().is_empty());
        assert!(mgr.list_known_models().is_empty());
        assert!(mgr.default_model().is_none());
        assert!(!mgr.is_known_model("gpt-4"));
    }

    #[tokio::test]
    async fn single_backend_lists_unprefixed_models() {
        let secrets = empty_secrets().await;
        let backends = Some(vec![backend("openai", &["gpt-4", "gpt-3.5"])]);
        let mgr = BackendManager::new(&backends, secrets);
        assert_eq!(mgr.list_known_backends(), vec!["openai"]);
        // Single backend: no prefix on models.
        let models = mgr.list_known_models();
        assert!(models.contains(&"gpt-4".to_string()));
        assert!(models.contains(&"gpt-3.5".to_string()));
        assert!(mgr.is_known_model("gpt-4"));
    }

    #[tokio::test]
    async fn multi_backend_prefixes_models() {
        let secrets = empty_secrets().await;
        let backends = Some(vec![
            backend("openai", &["gpt-4"]),
            backend("anthropic", &["claude-3"]),
        ]);
        let mgr = BackendManager::new(&backends, secrets);
        let models = mgr.list_known_models();
        assert!(models.contains(&"openai:gpt-4".to_string()));
        assert!(models.contains(&"anthropic:claude-3".to_string()));
        // Raw model name without prefix is NOT known in multi-backend mode.
        assert!(!mgr.is_known_model("gpt-4"));
        assert!(mgr.is_known_model("openai:gpt-4"));
    }

    // ================================================================
    // validate_model
    // ================================================================

    #[tokio::test]
    async fn validate_model_accepts_known_single_backend() {
        let secrets = empty_secrets().await;
        let backends = Some(vec![backend("openai", &["gpt-4"])]);
        let mgr = BackendManager::new(&backends, secrets);
        assert!(mgr.validate_model("gpt-4").is_ok());
        // Single backend is permissive: unknown model names pass too.
        assert!(mgr.validate_model("mystery-model").is_ok());
    }

    #[tokio::test]
    async fn validate_model_requires_prefix_when_multi() {
        let secrets = empty_secrets().await;
        let backends = Some(vec![
            backend("openai", &["gpt-4"]),
            backend("anthropic", &["claude-3"]),
        ]);
        let mgr = BackendManager::new(&backends, secrets);
        // Known prefixed model passes.
        assert!(mgr.validate_model("openai:gpt-4").is_ok());
        // Unknown-but-prefixed passes (allows caller to use new models).
        assert!(mgr.validate_model("openai:new-model").is_ok());
        // Unprefixed unknown model fails.
        assert!(mgr.validate_model("random-model").is_err());
    }

    // ================================================================
    // resolve_model_name
    // ================================================================

    #[tokio::test]
    async fn resolve_model_name_strips_backend_prefix() {
        let secrets = empty_secrets().await;
        let backends = Some(vec![
            backend("openai", &["gpt-4"]),
            backend("anthropic", &["claude-3"]),
        ]);
        let mgr = BackendManager::new(&backends, secrets);
        assert_eq!(
            mgr.resolve_model_name(Some("anthropic:claude-3")),
            "claude-3"
        );
        assert_eq!(mgr.resolve_model_name(Some("openai:gpt-4")), "gpt-4");
    }

    #[tokio::test]
    async fn resolve_model_name_falls_back_to_default() {
        let secrets = empty_secrets().await;
        let backends = Some(vec![backend("openai", &["gpt-4", "gpt-3.5"])]);
        let mgr = BackendManager::new(&backends, secrets);
        // None → default model of the first backend.
        let default = mgr.resolve_model_name(None);
        assert!(!default.is_empty(), "expected a default, got empty");
    }

    #[tokio::test]
    async fn resolve_model_name_empty_when_no_backends() {
        let secrets = empty_secrets().await;
        let mgr = BackendManager::new(&None, secrets);
        assert_eq!(mgr.resolve_model_name(Some("any-model")), "");
    }

    // ================================================================
    // max_retries_for_model
    // ================================================================

    #[tokio::test]
    async fn max_retries_uses_backend_config() {
        let secrets = empty_secrets().await;
        let mut b = backend("openai", &["gpt-4"]);
        b.max_retries = Some(7);
        let backends = Some(vec![b]);
        let mgr = BackendManager::new(&backends, secrets);
        assert_eq!(mgr.max_retries_for_model(Some("gpt-4")), 7);
    }

    #[tokio::test]
    async fn max_retries_default_when_no_backends() {
        let secrets = empty_secrets().await;
        let mgr = BackendManager::new(&None, secrets);
        // Falls back to 3 even without a backend so the runtime retry loop
        // stays well-defined.
        assert_eq!(mgr.max_retries_for_model(None), 3);
    }

    // ================================================================
    // execute / chat_with_tools_for_model without backends
    // ================================================================

    #[tokio::test]
    async fn execute_without_backends_is_configuration_error() {
        let secrets = empty_secrets().await;
        let mgr = BackendManager::new(&None, secrets);
        let ctx = ChatContext {
            messages: vec![Message::new(MessageRole::User, "hi")],
            model: None,
            system_prompt: None,
        };
        let err = mgr.execute(&ctx).await.unwrap_err();
        assert!(matches!(err, LlmError::Configuration { .. }));
    }

    #[tokio::test]
    async fn chat_with_tools_without_backends_is_configuration_error() {
        let secrets = empty_secrets().await;
        let mgr = BackendManager::new(&None, secrets);
        let result = mgr
            .chat_with_tools_for_model(Some("any"), &[], &[], "any")
            .await;
        match result {
            Err(LlmError::Configuration { .. }) => {}
            Err(other) => panic!("expected Configuration, got {other}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }
}
