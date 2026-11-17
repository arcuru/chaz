//! One resolution path for session controls, presentation and provider dispatch.
use super::BackendManager;
use crate::{
    config::{Backend, Config, ModelApi, ReasoningEffort},
    error::LlmError,
    security::SecretStore,
};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ReasoningState {
    /// Stable backend/model pair, independent of optional user-facing prefixes.
    pub key: String,
    pub effort: Option<ReasoningEffort>,
    pub source: &'static str,
    pub supported: Vec<ReasoningEffort>,
}

impl ReasoningState {
    pub fn label(&self) -> String {
        format!(
            "{} ({})",
            self.effort
                .map(ReasoningEffort::as_str)
                .unwrap_or("provider"),
            self.source
        )
    }
}

impl BackendManager {
    pub fn from_config(config: &Config, secrets: SecretStore) -> Self {
        Self {
            default_reasoning: config.default_reasoning,
            ..Self::new(&config.backends, secrets)
        }
    }

    /// A turn-local snapshot; never mutate the shared manager or other sessions.
    pub fn with_reasoning_overrides(
        &self,
        overrides: HashMap<String, Option<ReasoningEffort>>,
    ) -> Self {
        Self {
            reasoning_overrides: overrides,
            ..self.clone()
        }
    }

    pub fn reasoning_key(&self, model: Option<&str>) -> Option<String> {
        if self.backends.is_empty() {
            return None;
        }
        Some(
            serde_json::to_string(&(
                self.select_backend_for_model(model).get_name(),
                self.resolve_model_name(model),
            ))
            .expect("string pair"),
        )
    }

    pub fn reasoning_state(&self, model: Option<&str>) -> Result<ReasoningState, String> {
        let raw = self.resolve_model_name(model);
        if self.backends.is_empty() {
            return Ok(ReasoningState {
                key: String::new(),
                effort: None,
                source: "unavailable",
                supported: vec![],
            });
        }
        let backend = self.select_backend_for_model(model);
        let key = self.reasoning_key(model).expect("backend exists");
        let config = backend
            .models
            .as_ref()
            .and_then(|ms| ms.iter().find(|m| m.name == raw))
            .and_then(|m| m.reasoning.as_ref());
        // Preserve API/format conflicts even when the user selects provider default.
        backend.reasoning_control(&raw)?;
        let mut supported = config
            .map(|c| {
                c.supported_efforts
                    .clone()
                    .unwrap_or_else(|| c.effort.into_iter().collect())
            })
            .unwrap_or_default();
        match backend.api_for_model(&raw) {
            ModelApi::AnthropicMessages => supported.clear(), // no native mapping yet
            ModelApi::Responses => supported.retain(|e| *e != ReasoningEffort::Max),
            ModelApi::ChatCompletions => {}
        }
        // Canonical cycle order, independent of YAML order/duplicates.
        supported.sort();
        supported.dedup();
        let (effort, source) = if let Some(effort) = self.reasoning_overrides.get(&key) {
            (*effort, "session")
        } else if backend.api_for_model(&raw) == ModelApi::AnthropicMessages {
            (None, "provider default")
        } else if let Some(effort) = config.and_then(|c| c.effort) {
            (Some(effort), "model default")
        } else {
            // A global preference is clamped only within explicitly declared capabilities.
            // An unknown/non-reasoning model never receives an inferred control.
            let effort = self.default_reasoning.and_then(|requested| {
                supported
                    .iter()
                    .copied()
                    .find(|e| *e >= requested)
                    .or_else(|| supported.last().copied())
            });
            (
                effort,
                if effort.is_some() {
                    "global default"
                } else {
                    "provider default"
                },
            )
        };
        if let Some(effort) = effort
            && !supported.contains(&effort)
        {
            return Err(format!(
                "Reasoning '{}' is unsupported for {raw}; reset with /reasoning default or select a supported effort",
                effort.as_str()
            ));
        }
        Ok(ReasoningState {
            key,
            effort,
            source,
            supported,
        })
    }

    pub(super) fn request_backend(&self, model: Option<&str>) -> Result<Backend, LlmError> {
        let state = self
            .reasoning_state(model)
            .map_err(|message| LlmError::Configuration { message })?;
        let raw = self.resolve_model_name(model);
        let mut backend = self.select_backend_for_model(model).clone();
        if let Some(config) = backend
            .models
            .as_mut()
            .and_then(|ms| ms.iter_mut().find(|m| m.name == raw))
            .and_then(|m| m.reasoning.as_mut())
        {
            config.effort = state.effort;
        }
        Ok(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reasoning_defaults_are_capability_and_api_aware() {
        let config: Config = serde_yaml::from_str(
            r#"
default_reasoning: high
backends:
  - type: openaicompatible
    models:
      - name: unknown
      - name: declared
        reasoning: {supported_efforts: [none, low, medium]}
      - name: explicit
        reasoning: {effort: low, supported_efforts: [low, high]}
      - name: native
        api: responses
        reasoning: {supported_efforts: [low, xhigh, max]}
      - name: bad
        reasoning: {effort: medium, supported_efforts: []}
      - name: conflict
        api: responses
        reasoning: {supported_efforts: [low], format: openai}
  - type: anthropic
    name: claude
    models:
      - name: native
        reasoning: {effort: high, supported_efforts: [low, high]}
"#,
        )
        .unwrap();
        let manager =
            BackendManager::from_config(&config, crate::test_support::empty_secrets().await);
        assert_eq!(
            manager.reasoning_state(Some("unknown")).unwrap().effort,
            None
        );
        assert_eq!(
            manager.reasoning_state(Some("unlisted")).unwrap().effort,
            None
        );
        assert_eq!(
            manager.reasoning_state(Some("declared")).unwrap().effort,
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            manager.reasoning_state(Some("explicit")).unwrap().effort,
            Some(ReasoningEffort::Low)
        );
        let native = manager.reasoning_state(Some("native")).unwrap();
        assert_eq!(native.effort, Some(ReasoningEffort::XHigh));
        assert_eq!(
            native.supported,
            [ReasoningEffort::Low, ReasoningEffort::XHigh]
        );
        let claude = manager.reasoning_state(Some("claude:native")).unwrap();
        assert_eq!(claude.effort, None);
        assert!(claude.supported.is_empty());
        assert!(manager.reasoning_state(Some("bad")).is_err());
        assert!(
            manager
                .reasoning_state(Some("conflict"))
                .unwrap_err()
                .contains("format")
        );
        assert!(
            manager
                .with_reasoning_overrides([(native.key, Some(ReasoningEffort::Max))].into())
                .request_backend(Some("native"))
                .is_err()
        );
    }
}
