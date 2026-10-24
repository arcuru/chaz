/// Manage all the backends for chaz.
///
/// This module is responsible for handling dispatch, validation, and general management for all the different backends
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use tracing::debug;

use crate::{
    anthropic::Anthropic,
    config::{Backend, BackendType, ModelApi},
    error::LlmError,
    openai::OpenAI,
    responses::Responses,
    runtime::{LLMResponse, RuntimeMessage},
    security::SecretStore,
    tool::ToolDefinition,
};

/// LLM client selected by the model's effective API. [`LLMBackend`] is not
/// dyn-compatible (native `async fn` in trait), so dispatch is an explicit
/// enum rather than a boxed trait object; each arm forwards to the concrete
/// backend. Constructed fresh per call from a `&Backend`; each client clones
/// the config and secret-store handle.
enum BackendClient {
    OpenAI(OpenAI),
    Anthropic(Anthropic),
    Responses(Responses),
}

// Keep concrete dispatch exhaustive in one place without boxing async calls.
// Responses' cache call needs a different expression to carry the caller identity.
macro_rules! with_backend {
    ($client:expr, $backend:ident => $call:expr) => {
        with_backend!($client, $backend => $call, responses => $call)
    };
    ($client:expr, $backend:ident => $call:expr, responses => $responses:expr) => {
        match $client {
            BackendClient::OpenAI($backend) => $call,
            BackendClient::Anthropic($backend) => $call,
            BackendClient::Responses($backend) => $responses,
        }
    };
}

impl BackendClient {
    /// Catalog discovery belongs to the provider family, not a model's API.
    fn new(backend: &Backend, secrets: &SecretStore) -> Self {
        match backend.backend_type {
            BackendType::OpenAICompatible => BackendClient::OpenAI(OpenAI::new(backend, secrets)),
            BackendType::Anthropic => BackendClient::Anthropic(Anthropic::new(backend, secrets)),
        }
    }

    fn for_model(backend: &Backend, secrets: &SecretStore, model: &str) -> Result<Self, LlmError> {
        let api = backend.api_for_model(model);
        let client = match (&backend.backend_type, api) {
            (BackendType::OpenAICompatible, ModelApi::ChatCompletions) => {
                Self::OpenAI(OpenAI::new(backend, secrets))
            }
            (BackendType::Anthropic, ModelApi::AnthropicMessages) => {
                Self::Anthropic(Anthropic::new(backend, secrets))
            }
            (BackendType::OpenAICompatible, ModelApi::Responses) => {
                Self::Responses(Responses::new(backend, secrets))
            }
            _ => {
                return Err(LlmError::Configuration {
                    message: format!(
                        "API '{}' is incompatible with backend type {:?}",
                        api.as_str(),
                        backend.backend_type
                    ),
                });
            }
        };
        // A Chat-only reasoning format on a native API fails before any request,
        // including on Anthropic Messages, which otherwise ignores reasoning.
        backend
            .reasoning_control(model)
            .map_err(|message| LlmError::Configuration { message })?;
        Ok(client)
    }

    fn list_models(&self) -> Vec<String> {
        with_backend!(self, b => b.list_models())
    }

    fn list_models_with_info(&self) -> Vec<ModelInfo> {
        with_backend!(self, b => b.list_models_with_info())
    }

    async fn fetch_models_from_api(&self) -> Result<Vec<ModelInfo>, LlmError> {
        with_backend!(self, b => b.fetch_models_from_api().await)
    }

    fn default_model(&self) -> Option<String> {
        with_backend!(self, b => b.default_model())
    }

    async fn execute(&self, context: &ChatContext) -> Result<String, LlmError> {
        with_backend!(self, b => b.execute(context).await)
    }

    fn supports_tools(&self) -> bool {
        with_backend!(self, b => b.supports_tools())
    }

    async fn chat_with_cache(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &str,
        cache: &crate::cache::CacheOptions,
        prompt_cache_key: Option<&str>,
    ) -> Result<LLMResponse, LlmError> {
        with_backend!(self,
            b => b.chat_with_cache(messages, tools, model, cache).await,
            responses => b.chat_with_tools(messages, tools, model, prompt_cache_key).await
        )
    }
}

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

    async fn chat_with_cache(
        &self,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        model: &str,
        _cache: &crate::cache::CacheOptions,
    ) -> Result<LLMResponse, LlmError> {
        self.chat_with_tools(messages, tools, model).await
    }

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
    fn execute<'a>(
        &'a self,
        context: &'a ChatContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, LlmError>> + Send + 'a>>;
    fn supports_tools(&self) -> bool;
    fn chat_with_cache<'a>(
        &'a self,
        messages: &'a [RuntimeMessage],
        tools: &'a [ToolDefinition],
        model: &'a str,
        _cache: &'a crate::cache::CacheOptions,
    ) -> Pin<Box<dyn Future<Output = Result<LLMResponse, LlmError>> + Send + 'a>> {
        self.chat_with_tools(messages, tools, model)
    }
    /// Test seam for a bounded, tool-free auxiliary completion.
    fn complete_bounded<'a>(
        &'a self,
        messages: &'a [RuntimeMessage],
        model: &'a str,
        _max_output_tokens: u32,
    ) -> Pin<Box<dyn Future<Output = Result<LLMResponse, LlmError>> + Send + 'a>> {
        self.chat_with_tools(messages, &[], model)
    }
    fn chat_with_tools<'a>(
        &'a self,
        messages: &'a [RuntimeMessage],
        tools: &'a [ToolDefinition],
        model: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LLMResponse, LlmError>> + Send + 'a>>;
}

/// Display-oriented info about a single known model, surfaced to the
/// TUI model picker. `id` is the full name the backend expects (e.g.
/// `anthropic/claude-opus-4-7` for OpenRouter, or `openrouter:gpt-5-mini`
/// for multi-backend setups). Prices are USD per million tokens.
/// Modalities are the raw OpenRouter `architecture.input_modalities` /
/// `output_modalities` strings (`text`, `image`, `audio`, `video`, …) —
/// the picker derives capability badges from them.
///
/// Derives `Serialize`/`Deserialize` so it doubles as the persisted shape in
/// `model_info_store`. Every optional field pairs `#[serde(default)]` (older
/// entries / providers that omit a field load cleanly) with
/// `skip_serializing_if` (absent values aren't written at all). The latter
/// matters because the store lives in an append-only eidetica DB: keeping the
/// on-disk JSON to just the fields a model actually has holds down the bytes
/// that accumulate there forever. See
/// `docs/src/design/model_info_store.md`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    /// Maximum context window in tokens, when known. Sourced from the live
    /// `/models` catalog (OpenRouter-style providers report it) or declared
    /// in YAML for providers whose catalog omits it. `None` means unknown —
    /// callers fall back to the configured `max_context_tokens` budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

#[derive(Clone)]
pub struct BackendManager {
    backends: Vec<Backend>,
    secrets: SecretStore,
    session_scope: Option<(String, String)>,
    /// Optional override used by integration tests to bypass the OpenAI dispatch
    /// path. When set, `chat_with_tools_for_model` and `supports_tools_for_model`
    /// route through this trait object instead of constructing an `OpenAI` from
    /// the backend config. Production code never sets this; constructed only
    /// via `BackendManager::with_mock`.
    mock: Option<Arc<dyn BackendDispatch>>,
    /// Context windows for in-use models, sourced from the persisted
    /// `model_info_store`, keyed by the same model id space as
    /// `list_known_models_with_info`. Consulted by `context_window` as a
    /// fallback when the YAML-declared model carries no window — this is what
    /// makes window-aware budgeting work with zero config. Shared via `Arc`
    /// so per-session worker backends (cloned from the server's default
    /// backend) observe the same overlay; `RwLock` so the startup warm — or a
    /// background fetch when a not-yet-seen model is first used — can refresh
    /// it in place.
    model_windows: Arc<RwLock<HashMap<String, u32>>>,
    model_info: Arc<RwLock<HashMap<String, ModelInfo>>>,
}

/// A generic Message
#[derive(Clone, Debug)]
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
            session_scope: None,
            mock: None,
            model_windows: Arc::new(RwLock::new(HashMap::new())),
            model_info: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub(crate) fn for_session(&self, session_id: &str, caller: &str) -> Self {
        Self {
            session_scope: Some((session_id.into(), caller.into())),
            ..self.clone()
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
            session_scope: None,
            mock: Some(mock),
            model_windows: Arc::new(RwLock::new(HashMap::new())),
            model_info: Arc::new(RwLock::new(HashMap::new())),
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
            BackendClient::new(&self.backends[0], &self.secrets).list_models()
        } else {
            self.backends
                .iter()
                .flat_map(|backend| {
                    let prefix = backend.get_name();
                    BackendClient::new(backend, &self.secrets)
                        .list_models()
                        .into_iter()
                        .map(move |model| format!("{}:{}", prefix, model))
                })
                .collect()
        }
    }

    /// Like `list_known_models`, but yields each model's pricing alongside
    /// its id so the TUI picker can render a price column. Pricing is `None`
    /// when the backend config doesn't declare it.
    pub fn list_known_models_with_info(&self) -> Vec<ModelInfo> {
        if self.backends.len() == 1 {
            BackendClient::new(&self.backends[0], &self.secrets).list_models_with_info()
        } else {
            self.backends
                .iter()
                .flat_map(|backend| {
                    let prefix = backend.get_name();
                    BackendClient::new(backend, &self.secrets)
                        .list_models_with_info()
                        .into_iter()
                        .map(move |info| ModelInfo {
                            id: format!("{}:{}", prefix, info.id),
                            ..info
                        })
                })
                .collect()
        }
    }

    /// Live-fetch the full model catalog from each backend's `/models`
    /// endpoint. Multi-backend setups prefix ids with the backend name to
    /// match `list_known_models`. A failure in any backend is propagated;
    /// callers decide whether to fall back to the YAML-configured list.
    pub async fn fetch_models_with_info(&self) -> Result<Vec<ModelInfo>, crate::error::LlmError> {
        if self.backends.len() == 1 {
            return BackendClient::new(&self.backends[0], &self.secrets)
                .fetch_models_from_api()
                .await;
        }
        let mut out = Vec::new();
        for backend in &self.backends {
            let prefix = backend.get_name();
            let models = BackendClient::new(backend, &self.secrets)
                .fetch_models_from_api()
                .await?;
            out.extend(models.into_iter().map(|info| ModelInfo {
                id: format!("{prefix}:{}", info.id),
                ..info
            }));
        }
        Ok(out)
    }

    /// Returns true if the model is known
    pub fn is_known_model(&self, model: &str) -> bool {
        self.list_known_models().contains(&model.to_string())
    }

    /// The context window (in tokens) for `model`, if known.
    ///
    /// Resolution order:
    /// 1. The YAML-declared catalog (`list_known_models_with_info`) — an
    ///    explicit `context_window:` in config is operator intent and wins.
    /// 2. The in-use-model overlay loaded from `model_info_store` via
    ///    [`set_model_windows`] / [`insert_model_window`] — the zero-config
    ///    path, so a model whose window chaz learned from a `/models` fetch
    ///    budgets correctly without anyone hand-editing config.
    ///
    /// Both use the same id space, so multi-backend prefixed ids
    /// (`backend:model`) match in either tier. `None` when neither source
    /// knows the model's window — callers fall back to the configured
    /// `max_context_tokens`.
    pub fn context_window(&self, model: &str) -> Option<usize> {
        if let Some(w) = self
            .list_known_models_with_info()
            .into_iter()
            .find(|info| info.id == model)
            .and_then(|info| info.context_window)
        {
            return Some(w as usize);
        }
        self.model_windows
            .read()
            .unwrap()
            .get(model)
            .map(|&w| w as usize)
    }

    /// Replace the whole in-use-model window overlay consulted by
    /// [`context_window`]. Called once at startup (server warm) from the
    /// persisted `model_info_store`. Because the overlay lives behind a shared
    /// `Arc`, updates are visible to every clone of this manager — including
    /// the per-session worker backends cloned at `register_session`.
    pub fn set_model_windows(&self, windows: HashMap<String, u32>) {
        *self.model_windows.write().unwrap() = windows;
    }

    /// Insert (or update) a single model's window into the overlay, leaving
    /// the rest intact. Used by the background "first use of a not-yet-seen
    /// model" path so the very next turn budgets window-aware without waiting
    /// for a restart. Shares the same `Arc`, so all clones see it.
    pub fn insert_model_window(&self, model: String, window: u32) {
        self.model_windows.write().unwrap().insert(model, window);
    }

    /// Refresh cached in-use prices and windows without a network request.
    pub fn insert_model_info(&self, info: ModelInfo) {
        if let Some(window) = info.context_window {
            self.insert_model_window(info.id.clone(), window);
        }
        self.model_info
            .write()
            .unwrap()
            .insert(info.id.clone(), info);
    }

    /// Configured fields win over cached fields. Lookup remains backend-scoped;
    /// an alias's price is not silently applied to a different returned model.
    pub(crate) fn model_info_for_call(
        &self,
        requested: Option<&str>,
        wire: Option<&str>,
    ) -> Option<ModelInfo> {
        let backend = self
            .backends
            .first()
            .map(|_| self.select_backend_for_model(requested))?;
        let model = wire
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.resolve_model_name(requested));
        let id = if self.backends.len() > 1 {
            format!("{}:{model}", backend.get_name())
        } else {
            model
        };
        let configured = self
            .list_known_models_with_info()
            .into_iter()
            .find(|m| m.id == id);
        let cached = self.model_info.read().unwrap().get(&id).cloned();
        match (configured, cached) {
            (Some(mut info), Some(cache)) => {
                info.price_input = info.price_input.or(cache.price_input);
                info.price_output = info.price_output.or(cache.price_output);
                info.price_cache_read = info.price_cache_read.or(cache.price_cache_read);
                Some(info)
            }
            (info, cache) => info.or(cache),
        }
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
        let model = BackendClient::new(backend, &self.secrets).default_model()?;
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
        if let Some(mock) = &self.mock {
            return mock.execute(context).await;
        }
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".to_string(),
            });
        }
        let backend = self.select_backend(context);
        let model = self.resolve_model_name(context.model.as_deref());
        BackendClient::for_model(backend, &self.secrets, &model)?
            .execute(context)
            .await
    }

    /// Whether the backend for the given model supports tool/function calling.
    /// Invalid API selections report no support; execution preserves the error.
    pub fn supports_tools_for_model(&self, model: Option<&str>) -> bool {
        self.try_supports_tools_for_model(model).unwrap_or(false)
    }

    pub(crate) fn try_supports_tools_for_model(
        &self,
        model: Option<&str>,
    ) -> Result<bool, LlmError> {
        if let Some(mock) = &self.mock {
            return Ok(mock.supports_tools());
        }
        if self.backends.is_empty() {
            return Ok(false);
        }
        let backend = self.select_backend_for_model(model);
        let resolved = self.resolve_model_name(model);
        Ok(BackendClient::for_model(backend, &self.secrets, &resolved)?.supports_tools())
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
            resolved = BackendClient::new(backend, &self.secrets)
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

    /// Native pending continuations are not completed history: never reinterpret
    /// their in-progress exchange, even when visible conversion would be possible.
    pub(crate) fn validate_pending_replay(
        &self,
        model: Option<&str>,
        resolved_model: &str,
        messages: &[RuntimeMessage],
    ) -> Result<(), LlmError> {
        for message in messages {
            let (RuntimeMessage::AssistantWithExtra { provider_extra, .. }
            | RuntimeMessage::AssistantToolCalls { provider_extra, .. }) = message
            else {
                continue;
            };
            if !provider_extra.contains_key(crate::responses::OUTPUT_ITEMS) {
                continue;
            }
            let compatible = !self.backends.is_empty() && self.mock.is_none() && {
                let backend = self.select_backend_for_model(model);
                crate::responses::ReplayOrigin::new(
                    backend,
                    &self.secrets,
                    resolved_model,
                    backend.api_for_model(resolved_model),
                )
                .matches(provider_extra)
            };
            if !compatible {
                return Err(LlmError::InvalidRequest {
                    message: "Pending Responses continuation has an incompatible or unknown origin; restore the original API, requested model and connection before resuming, or explicitly interrupt the turn. No wait or tool effect was run".into(),
                });
            }
        }
        Ok(())
    }

    /// Validate the accepted request's native replay before host incorporation
    /// and presentation telemetry. Dispatch revalidates against current credentials.
    pub(crate) fn validate_request_replay(
        &self,
        model: Option<&str>,
        resolved_model: &str,
        messages: &[RuntimeMessage],
    ) -> Result<(), LlmError> {
        if self.mock.is_some() {
            return Ok(());
        }
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".into(),
            });
        }
        let backend = self.select_backend_for_model(model);
        if backend.api_for_model(resolved_model) == ModelApi::Responses {
            crate::responses::Responses::new(backend, &self.secrets)
                .validate_request(messages, resolved_model)?;
        } else {
            crate::responses::validate_foreign_history(messages)?;
        }
        Ok(())
    }

    /// Maximum retry attempts for transient errors on the backend for the given model.
    pub fn max_retries_for_model(&self, model: Option<&str>) -> u32 {
        if self.backends.is_empty() {
            return 3;
        }
        self.select_backend_for_model(model).max_retries()
    }

    /// One tool-free auxiliary call through the configured model route.
    /// No runtime hooks, tools, retries, or Anthropic output-budget raises.
    pub(crate) async fn complete_bounded(
        &self,
        model: Option<&str>,
        messages: &[RuntimeMessage],
        max_output_tokens: u32,
    ) -> Result<LLMResponse, LlmError> {
        if let Some(model) = model {
            self.validate_model(model)
                .map_err(|message| LlmError::Configuration { message })?;
        }
        if let Some(mock) = &self.mock {
            return mock
                .complete_bounded(messages, model.unwrap_or(""), max_output_tokens)
                .await;
        }
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".into(),
            });
        }
        let resolved = self.resolve_model_name(model);
        let backend = self.select_backend_for_model(model);
        let client = BackendClient::for_model(backend, &self.secrets, &resolved)?;
        let cache = crate::extensions::context::default_cache_options();
        let limit = Some(max_output_tokens);
        with_backend!(client,
            b => b.chat_with_cache_impl(messages, &[], &resolved, &cache, limit).await,
            responses => b.chat_with_tools_bounded(messages, &[], &resolved, None, limit).await
        )
    }

    /// Execute a single LLM call with tool definitions (for ReAct loop).
    pub async fn chat_with_tools_for_model(
        &self,
        model: Option<&str>,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        resolved_model: &str,
    ) -> Result<LLMResponse, LlmError> {
        self.chat_with_cache_for_model(
            model,
            messages,
            tools,
            resolved_model,
            &crate::extensions::context::default_cache_options(),
        )
        .await
    }
    pub async fn chat_with_cache_for_model(
        &self,
        model: Option<&str>,
        messages: &[RuntimeMessage],
        tools: &[ToolDefinition],
        resolved_model: &str,
        cache: &crate::cache::CacheOptions,
    ) -> Result<LLMResponse, LlmError> {
        if let Some(mock) = &self.mock {
            return mock
                .chat_with_cache(messages, tools, resolved_model, cache)
                .await;
        }
        if self.backends.is_empty() {
            return Err(LlmError::Configuration {
                message: "No backends configured".to_string(),
            });
        }
        let backend = self.select_backend_for_model(model);
        let key = self
            .session_scope
            .as_ref()
            .map(|(session, caller)| self.secrets.session_cache_key(session, caller, backend));
        let client = BackendClient::for_model(backend, &self.secrets, resolved_model)?;
        if backend.api_for_model(resolved_model) != ModelApi::Responses {
            crate::responses::validate_foreign_history(messages)?;
        }
        client
            .chat_with_cache(messages, tools, resolved_model, cache, key.as_deref())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, BackendType, Model};
    use crate::test_support::empty_secrets;

    fn backend(name: &str, models: &[&str]) -> Backend {
        let mut b = Backend::new(BackendType::OpenAICompatible);
        b.name = Some(name.to_string());
        b.models = Some(
            models
                .iter()
                .map(|m| Model {
                    name: m.to_string(),
                    api: None,
                    reasoning: None,
                    price_input: None,
                    price_output: None,
                    price_cache_read: None,
                    context_window: None,
                })
                .collect(),
        );
        b
    }

    #[tokio::test]
    async fn client_facade_forwards_catalog_and_defaults_for_each_api() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "discovered"}]
            })))
            .expect(3)
            .mount(&server)
            .await;
        let secrets = empty_secrets().await;
        for (backend_type, api) in [
            (BackendType::OpenAICompatible, ModelApi::ChatCompletions),
            (BackendType::Anthropic, ModelApi::AnthropicMessages),
            (BackendType::OpenAICompatible, ModelApi::Responses),
        ] {
            let mut b = backend("provider", &["configured", "second"]);
            b.backend_type = backend_type;
            b.api = Some(api);
            b.api_base = Some(server.uri());
            b.api_key = Some("test-key".into());
            b.models.as_mut().unwrap()[0].price_input = Some(2.0);
            let client = BackendClient::for_model(&b, &secrets, "configured").unwrap();
            assert_eq!(client.list_models(), ["configured", "second"]);
            assert_eq!(client.default_model().as_deref(), Some("configured"));
            assert!(client.supports_tools());
            assert_eq!(
                client.list_models_with_info(),
                vec![
                    ModelInfo {
                        id: "configured".into(),
                        price_input: Some(2.0),
                        ..Default::default()
                    },
                    ModelInfo {
                        id: "second".into(),
                        ..Default::default()
                    },
                ]
            );
            assert_eq!(
                client.fetch_models_from_api().await.unwrap(),
                vec![ModelInfo {
                    id: "discovered".into(),
                    ..Default::default()
                }]
            );
        }
    }

    #[tokio::test]
    async fn telemetry_prices_use_config_cached_info_and_exact_backend_wire_identity() {
        let manager = BackendManager::new(
            &Some(vec![backend("one", &["alias"]), backend("two", &["wire"])]),
            empty_secrets().await,
        );
        manager.insert_model_info(ModelInfo {
            id: "one:alias".into(),
            price_input: Some(2.0),
            ..Default::default()
        });
        manager.insert_model_info(ModelInfo {
            id: "two:wire".into(),
            price_input: Some(9.0),
            ..Default::default()
        });
        assert_eq!(
            manager
                .model_info_for_call(Some("one:alias"), None)
                .unwrap()
                .price_input,
            Some(2.0)
        );
        assert!(
            manager
                .model_info_for_call(Some("one:alias"), Some("wire"))
                .is_none(),
            "other backend's matching wire slug is not price data"
        );
        manager.insert_model_info(ModelInfo {
            id: "one:wire".into(),
            price_input: Some(3.0),
            context_window: Some(8000),
            ..Default::default()
        });
        assert_eq!(
            manager
                .model_info_for_call(Some("one:alias"), Some("wire"))
                .unwrap()
                .price_input,
            Some(3.0)
        );
        assert_eq!(manager.context_window("one:wire"), Some(8000));
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
    async fn context_window_resolves_from_configured_model() {
        let secrets = empty_secrets().await;
        let mut b = backend("openai", &["gpt-4", "gpt-3.5"]);
        b.models.as_mut().unwrap()[0].context_window = Some(64_000);
        let mgr = BackendManager::new(&Some(vec![b]), secrets);
        assert_eq!(mgr.context_window("gpt-4"), Some(64_000));
        // Model exists but declares no window, and an unknown model: both None.
        assert_eq!(mgr.context_window("gpt-3.5"), None);
        assert_eq!(mgr.context_window("nope"), None);
    }

    #[tokio::test]
    async fn context_window_falls_back_to_catalog_overlay() {
        let secrets = empty_secrets().await;
        let mut b = backend("openai", &["gpt-4", "gpt-3.5"]);
        // gpt-4 declares a window in YAML; gpt-3.5 does not.
        b.models.as_mut().unwrap()[0].context_window = Some(64_000);
        let mgr = BackendManager::new(&Some(vec![b]), secrets);

        // Overlay supplies windows the catalog learned at runtime, including
        // one that also has a YAML value.
        let mut overlay = HashMap::new();
        overlay.insert("gpt-4".to_string(), 999_000);
        overlay.insert("gpt-3.5".to_string(), 128_000);
        mgr.set_model_windows(overlay);

        // YAML wins over the overlay (explicit operator intent).
        assert_eq!(mgr.context_window("gpt-4"), Some(64_000));
        // No YAML window -> overlay fills it in (the zero-config path).
        assert_eq!(mgr.context_window("gpt-3.5"), Some(128_000));
        // Still None for a model neither source knows.
        assert_eq!(mgr.context_window("nope"), None);
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
        mgr.validate_model("gpt-4")
            .expect("model should be accepted with this backend configuration");
        // Single backend is permissive: unknown model names pass too.
        mgr.validate_model("mystery-model")
            .expect("model should be accepted with this backend configuration");
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
        mgr.validate_model("openai:gpt-4")
            .expect("model should be accepted with this backend configuration");
        // Unknown-but-prefixed passes (allows caller to use new models).
        mgr.validate_model("openai:new-model")
            .expect("model should be accepted with this backend configuration");
        // Unprefixed unknown model fails.
        assert_eq!(
            mgr.validate_model("random-model"),
            Err("Multiple backends exist, please specify the model name with the backend prepended, e.g. openrouter:model-name"
                .into())
        );
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
    async fn bounded_completion_sends_native_limits_without_tools_or_policy_override() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        for api in [
            ModelApi::ChatCompletions,
            ModelApi::AnthropicMessages,
            ModelApi::Responses,
        ] {
            let kind = if api == ModelApi::AnthropicMessages {
                BackendType::Anthropic
            } else {
                BackendType::OpenAICompatible
            };
            let server = MockServer::start().await;
            let response = match api {
                ModelApi::ChatCompletions => {
                    serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "Title"}, "finish_reason": "stop"}], "model": "test-model", "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}})
                }
                ModelApi::AnthropicMessages => {
                    serde_json::json!({"content": [{"type": "text", "text": "Title"}], "model": "test-model", "stop_reason": "end_turn", "usage": {"input_tokens": 10, "output_tokens": 2}})
                }
                ModelApi::Responses => {
                    serde_json::json!({"status":"completed", "model":"test-model", "output":[{"type":"message", "content":[{"type":"output_text", "text":"Title"}]}], "usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}})
                }
            };
            let endpoint = match api {
                ModelApi::AnthropicMessages => "/v1/messages",
                ModelApi::ChatCompletions => "/v1/chat/completions",
                ModelApi::Responses => "/v1/responses",
            };
            Mock::given(method("POST"))
                .and(path(endpoint))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .expect(2)
                .mount(&server)
                .await;
            let mut config = Backend::new(kind.clone());
            config.name = Some("route".into());
            config.api_base = Some(format!("{}/v1", server.uri()));
            config.api_key = Some("test-key".into());
            config.models = Some(vec![Model {
                name: "test-model".into(),
                api: Some(api),
                reasoning: Some(crate::config::ReasoningConfig {
                    effort: crate::config::ReasoningEffort::High,
                    format: None,
                }),
                price_input: None,
                price_output: None,
                price_cache_read: None,
                context_window: None,
            }]);
            let manager = BackendManager::new(&Some(vec![config]), empty_secrets().await);
            let messages = vec![RuntimeMessage::User("context".into())];
            let response = manager
                .complete_bounded(Some("route:test-model"), &messages, 128)
                .await
                .unwrap();
            assert!(
                matches!(response, LLMResponse::Text { ref content, .. } if content == "Title")
            );
            manager
                .chat_with_tools_for_model(Some("route:test-model"), &messages, &[], "test-model")
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            let normal: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
            assert_eq!(body["model"], "test-model");
            assert!(body.get("tools").is_none());
            if api == ModelApi::AnthropicMessages {
                assert_eq!(body["max_tokens"], 128);
                assert_eq!(normal["max_tokens"], 8192);
            } else if api == ModelApi::Responses {
                assert_eq!(body["max_output_tokens"], 128);
                assert_eq!(body["reasoning"]["effort"], "high");
                assert!(normal.get("max_output_tokens").is_none());
            } else {
                assert_eq!(body["max_completion_tokens"], 128);
                assert_eq!(body["reasoning"]["effort"], "high");
                assert!(normal.get("max_completion_tokens").is_none());
            }
        }
    }

    #[tokio::test]
    async fn bounded_anthropic_completion_never_raises_budget_or_retries_tool_use() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/messages")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"stop_reason": "max_tokens", "content": [{"type": "tool_use", "id": "call", "name": "tool", "input": {}}]}))).expect(1).mount(&server).await;
        let mut config = Backend::new(BackendType::Anthropic);
        config.api_base = Some(server.uri());
        config.api_key = Some("test-key".into());
        let manager = BackendManager::new(&Some(vec![config]), empty_secrets().await);
        assert!(matches!(
            manager
                .complete_bounded(
                    Some("test-model"),
                    &[RuntimeMessage::User("context".into())],
                    128
                )
                .await,
            Err(LlmError::TruncatedOutput { .. })
        ));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "no model-catalog lookup or output-budget raise"
        );
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

    #[tokio::test]
    async fn attached_turn_preserves_api_configuration_errors() {
        use crate::test_support::{fresh_session, permissive_security, tool_context};
        use crate::tool::{ToolPolicyRegistry, ToolRegistry};

        let (_instance, session) = fresh_session().await;
        let mut ctx = tool_context(session, Arc::new(ToolRegistry::new()));
        ctx.allow_no_reply = true;
        let secrets = empty_secrets().await;
        let mut b = backend("proxy", &["model"]);
        b.models.as_mut().unwrap()[0].api = Some(ModelApi::AnthropicMessages);
        let mgr = BackendManager::new(&Some(vec![b]), secrets);
        let expected = mgr
            .chat_with_tools_for_model(Some("model"), &[], &[], "model")
            .await
            .err()
            .expect("API selection must fail")
            .to_string();
        let result = crate::runtime::execute(
            Some("model"),
            vec![RuntimeMessage::User("hi".into())],
            &mgr,
            &permissive_security(),
            &ctx,
            &ToolPolicyRegistry::empty(),
            None,
            None,
        )
        .await;
        assert_eq!(result.expect_err("turn must fail"), expected);
    }

    #[tokio::test]
    async fn incompatible_api_fails_on_legacy_and_runtime_paths() {
        let secrets = empty_secrets().await;
        for (family, api) in [
            (BackendType::OpenAICompatible, ModelApi::AnthropicMessages),
            (BackendType::Anthropic, ModelApi::ChatCompletions),
            (BackendType::Anthropic, ModelApi::Responses),
        ] {
            for model_override in [false, true] {
                let mut b = backend("proxy", &["model"]);
                b.backend_type = family.clone();
                if model_override {
                    b.models.as_mut().unwrap()[0].api = Some(api);
                } else {
                    b.api = Some(api);
                }
                let mgr = BackendManager::new(&Some(vec![b]), secrets.clone());
                let ctx = ChatContext {
                    messages: vec![],
                    model: None,
                    system_prompt: None,
                };
                let legacy = mgr.execute(&ctx).await.unwrap_err();
                let runtime = mgr
                    .chat_with_tools_for_model(None, &[], &[], "model")
                    .await
                    .err()
                    .expect("incompatible API must fail");
                for error in [legacy, runtime] {
                    assert!(matches!(error, LlmError::Configuration { message }
                        if message.contains("is incompatible with backend type")
                        && message.contains(api.as_str())));
                }
            }
        }
    }

    #[tokio::test]
    async fn responses_api_routes_legacy_and_runtime_without_chat_fallback() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status":"completed", "output":[{"type":"message", "content":[{"type":"output_text", "text":"native"}]}]
            }))).expect(6).mount(&server).await;
        let secrets = empty_secrets().await;
        let mut b = backend("provider", &["response-model"]);
        b.api_key = Some("test-token".into());
        b.api_base = Some(server.uri());
        b.models.as_mut().unwrap()[0].api = Some(ModelApi::Responses);
        let mgr = BackendManager::new(&Some(vec![b]), secrets);
        assert!(mgr.supports_tools_for_model(Some("response-model")));
        for model in [None, Some("provider:response-model")] {
            let resolved = mgr.resolve_model_name(model);
            let ctx = ChatContext {
                messages: vec![],
                model: model.map(str::to_owned),
                system_prompt: None,
            };
            assert_eq!(mgr.execute(&ctx).await.unwrap(), "native");
            assert!(
                matches!(mgr.chat_with_tools_for_model(model, &[], &[], &resolved).await.unwrap(), LLMResponse::Text { content, .. } if content == "native")
            );
            assert!(
                matches!(mgr.chat_with_cache_for_model(model, &[], &[], &resolved, &crate::cache::CacheOptions::default()).await.unwrap(), LLMResponse::Text { content, .. } if content == "native")
            );
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests.iter().all(|r| r.url.path() == "/responses"));
        assert!(requests.iter().all(|r| {
            r.body_json::<serde_json::Value>()
                .unwrap()
                .get("prompt_cache_key")
                .is_none()
        }));
    }

    #[tokio::test]
    async fn live_discovery_inherits_api_without_changing_catalog_or_naming() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("Authorization", "Bearer catalog-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "configured", "api": "chat-completions"},
                    {"id": "new-model", "api": "guessed"}
                ]
            })))
            .expect(3)
            .mount(&server)
            .await;
        let secrets = empty_secrets().await;
        let mut b = backend("proxy", &["configured", "not-discovered"]);
        b.api_base = Some(format!("{}/v1", server.uri()));
        b.api_key = Some("catalog-key".into());
        b.api = Some(ModelApi::Responses);
        b.models.as_mut().unwrap()[0].api = Some(ModelApi::ChatCompletions);
        let mgr = BackendManager::new(&Some(vec![b.clone()]), secrets.clone());
        let ids: Vec<_> = mgr
            .fetch_models_with_info()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["configured", "new-model"]);
        assert_eq!(mgr.list_known_models(), ["configured", "not-discovered"]);
        // Discovery doesn't overwrite local settings or become an allowlist.
        assert!(matches!(
            BackendClient::for_model(&b, &secrets, &ids[0]),
            Ok(BackendClient::OpenAI(_))
        ));
        for id in [&ids[1], "unknown", "not-discovered"] {
            mgr.validate_model(id)
                .expect("model should be accepted with this backend configuration");
            assert!(
                matches!(
                    BackendClient::for_model(&b, &secrets, id),
                    Ok(BackendClient::Responses(_))
                ),
                "unknown IDs inherit the backend API"
            );
        }

        let mut other = b.clone();
        other.name = Some("other".into());
        other.api = None;
        let multi = BackendManager::new(&Some(vec![b, other]), secrets);
        let ids: Vec<_> = multi
            .fetch_models_with_info()
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            ids,
            [
                "proxy:configured",
                "proxy:new-model",
                "other:configured",
                "other:new-model"
            ]
        );
        assert_eq!(multi.default_model().as_deref(), Some("proxy:configured"));
        multi
            .validate_model("proxy:unknown")
            .expect("model should be accepted with this backend configuration");
        assert_eq!(
            multi.validate_model("unknown"),
            Err("Multiple backends exist, please specify the model name with the backend prepended, e.g. openrouter:model-name"
                .into())
        );
        assert_eq!(
            multi.resolve_model_name(Some("proxy:new-model")),
            "new-model"
        );
        assert!(multi.supports_tools_for_model(Some("proxy:new-model")));
        assert!(multi.supports_tools_for_model(Some("other:new-model")));
    }

    #[tokio::test]
    async fn anthropic_default_and_explicit_api_preserve_native_requests_and_cache() {
        use crate::cache::{CacheAnchor, CacheOptions, CacheTtl};
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "native-key"))
            .and(body_partial_json(
                serde_json::json!({"model": "claude-test"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "native"}],
                "stop_reason": "end_turn"
            })))
            .expect(4)
            .mount(&server)
            .await;
        let secrets = empty_secrets().await;
        secrets
            .insert("backend:native".into(), "native-key".into())
            .await;
        let mut b = backend("native", &["claude-test"]);
        b.backend_type = BackendType::Anthropic;
        b.api_base = Some(format!("{}/v1", server.uri()));
        b.api_key_ref = Some("backend:native".into());
        for api in [None, Some(ModelApi::AnthropicMessages)] {
            b.api = api;
            let mgr = BackendManager::new(&Some(vec![b.clone()]), secrets.clone());
            assert_eq!(mgr.default_model().as_deref(), Some("claude-test"));
            assert!(mgr.supports_tools_for_model(None));
            assert_eq!(mgr.list_known_models(), ["claude-test"]);
            let ctx = ChatContext {
                messages: vec![Message::new(MessageRole::User, "hi")],
                model: None,
                system_prompt: Some("be helpful".into()),
            };
            assert_eq!(mgr.execute(&ctx).await.unwrap(), "native");
            let response = mgr
                .chat_with_cache_for_model(
                    None,
                    &[
                        RuntimeMessage::System("be helpful".into()),
                        RuntimeMessage::User("hi".into()),
                    ],
                    &[],
                    &mgr.resolve_model_name(None),
                    &CacheOptions {
                        anchors: vec![CacheAnchor::System],
                        ttl: CacheTtl::OneHour,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(response, LLMResponse::Text { content, .. } if content == "native"));
        }
        let requests = server.received_requests().await.unwrap();
        for request in [&requests[1], &requests[3]] {
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        }
    }

    #[tokio::test]
    async fn chat_override_uses_same_connection_and_credentials_on_both_paths() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("Authorization", "Bearer stored-key"))
            .and(body_partial_json(
                serde_json::json!({"model": "chat-model"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "hello"}}]
            })))
            .expect(2)
            .mount(&server)
            .await;
        let secrets = empty_secrets().await;
        secrets
            .insert("backend:proxy".into(), "stored-key".into())
            .await;
        let mut b = backend("proxy", &["chat-model"]);
        b.api = Some(ModelApi::Responses);
        b.models.as_mut().unwrap()[0].api = Some(ModelApi::ChatCompletions);
        b.api_base = Some(format!("{}/v1", server.uri()));
        b.api_key_ref = Some("backend:proxy".into());
        let mgr = BackendManager::new(&Some(vec![b, backend("other", &["other-model"])]), secrets);
        let selected = Some("proxy:chat-model");
        assert!(mgr.supports_tools_for_model(selected));
        let ctx = ChatContext {
            messages: vec![Message::new(MessageRole::User, "hi")],
            model: selected.map(str::to_owned),
            system_prompt: None,
        };
        assert_eq!(mgr.execute(&ctx).await.unwrap(), "hello");
        let response = mgr
            .chat_with_tools_for_model(selected, &[], &[], &mgr.resolve_model_name(selected))
            .await
            .unwrap();
        assert!(matches!(response, LLMResponse::Text { content, .. } if content == "hello"));
    }
}
