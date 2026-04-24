//! Structured error types for Chaz.
//!
//! Follows the same pattern as eidetica: a top-level `Error` enum with boxed
//! domain-specific sub-errors, each in their own module. Domain errors use
//! `#[non_exhaustive]` and provide `is_*()` semantic helpers.

use std::time::Duration;

/// Result type alias for use throughout Chaz.
///
/// Not yet used everywhere — will replace `Result<_, String>` and `anyhow::Result`
/// incrementally as domain error types are added.
#[allow(dead_code)]
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Top-level error type for Chaz.
///
/// Domain-specific variants are boxed to keep `Result<T, Error>` small on the
/// stack. The box allocation only occurs on the error (cold) path.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Structured LLM backend errors
    #[error(transparent)]
    Llm(Box<LlmError>),

    /// I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<LlmError> for Error {
    fn from(err: LlmError) -> Self {
        Error::Llm(Box::new(err))
    }
}

/// Errors from LLM backend calls (OpenAI-compatible APIs).
///
/// Classifies failures into retryable vs non-retryable categories, enabling
/// retry logic, circuit breakers, and structured escalation.
///
/// # Stability
///
/// New variants may be added in minor versions (enum is `#[non_exhaustive]`).
/// Use `is_*()` helpers for stable matching.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Rate limited (HTTP 429). Retryable after delay.
    #[error("Rate limited: {message}")]
    RateLimited {
        /// Delay before retrying, from `Retry-After` header if available
        retry_after_duration: Option<Duration>,
        /// Raw error message from the provider
        message: String,
    },

    /// Transient server error (HTTP 500, 502, 503, 504). Retryable.
    #[error("Server error (HTTP {status}): {message}")]
    ServerError {
        /// HTTP status code
        status: u16,
        /// Raw error message
        message: String,
    },

    /// Request timed out (connection or response). Retryable.
    #[error("Request timed out")]
    Timeout,

    /// Authentication or authorization failure (HTTP 401, 403). Not retryable.
    #[error("Auth failed (HTTP {status}): {message}")]
    AuthFailed {
        /// HTTP status code (401 or 403)
        status: u16,
        /// Error message
        message: String,
    },

    /// Insufficient credits or quota (HTTP 402). Not retryable.
    #[error("Insufficient credits: {message}")]
    InsufficientCredits {
        /// Error message
        message: String,
    },

    /// Invalid request (HTTP 400, bad JSON, missing params). Not retryable.
    #[error("Invalid request: {message}")]
    InvalidRequest {
        /// Error message
        message: String,
    },

    /// No response choices returned by the API. Not retryable.
    #[error("Empty response: {message}")]
    EmptyResponse {
        /// Description of what was missing
        message: String,
    },

    /// Client configuration error (missing API key, bad URL). Not retryable.
    #[error("Configuration error: {message}")]
    Configuration {
        /// Description of the configuration problem
        message: String,
    },

    /// Circuit breaker is open — backend is unhealthy. Not retryable (yet).
    #[error("Circuit breaker open: backend temporarily unavailable")]
    CircuitOpen,

    /// Network-level failure (DNS, connection refused, TLS). Retryable.
    #[error("Network error: {message}")]
    NetworkError {
        /// Error message
        message: String,
    },
}

impl LlmError {
    /// Whether this error is transient and the request should be retried.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmError::RateLimited { .. }
                | LlmError::ServerError { .. }
                | LlmError::Timeout
                | LlmError::NetworkError { .. }
        )
    }

    /// Whether this error indicates an authentication/authorization problem.
    pub fn is_auth_error(&self) -> bool {
        matches!(
            self,
            LlmError::AuthFailed { .. } | LlmError::InsufficientCredits { .. }
        )
    }

    /// Whether this error is a client-side problem (bad request, config).
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            LlmError::InvalidRequest { .. }
                | LlmError::Configuration { .. }
                | LlmError::EmptyResponse { .. }
        )
    }

    /// Get the suggested retry delay, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            LlmError::RateLimited {
                retry_after_duration,
                ..
            } => *retry_after_duration,
            _ => None,
        }
    }

    /// Get the HTTP status code, if available.
    pub fn status(&self) -> Option<u16> {
        match self {
            LlmError::RateLimited { .. } => Some(429),
            LlmError::ServerError { status, .. } => Some(*status),
            LlmError::AuthFailed { status, .. } => Some(*status),
            LlmError::InsufficientCredits { .. } => Some(402),
            LlmError::InvalidRequest { .. } => Some(400),
            _ => None,
        }
    }

    /// Classify an HTTP status code and message into the appropriate variant.
    pub fn from_http_status(status: u16, message: String) -> Self {
        match status {
            429 => LlmError::RateLimited {
                retry_after_duration: None,
                message,
            },
            401 | 403 => LlmError::AuthFailed { status, message },
            402 => LlmError::InsufficientCredits { message },
            400 => LlmError::InvalidRequest { message },
            408 => LlmError::Timeout,
            500..=599 => LlmError::ServerError { status, message },
            _ => LlmError::InvalidRequest { message },
        }
    }

    /// Classify an `async_openai` `OpenAIError` into a structured `LlmError`.
    ///
    /// The SDK's `OpenAIError` surfaces:
    /// - `Reqwest(_)` — transport errors (timeout, connect refused, TLS, etc.)
    /// - `ApiError(ApiError)` — the provider returned an error body. The HTTP
    ///   status code is NOT exposed directly; we classify on the `type`/`code`
    ///   strings OpenAI-compatible providers use.
    /// - `JSONDeserialize`/`InvalidArgument` — client-side, non-retryable.
    pub fn from_openai_error(err: async_openai::error::OpenAIError) -> Self {
        use async_openai::error::OpenAIError;
        match err {
            OpenAIError::Reqwest(ref reqwest_err) => {
                if reqwest_err.is_timeout() {
                    return LlmError::Timeout;
                }
                if reqwest_err.is_connect() {
                    return LlmError::NetworkError {
                        message: reqwest_err.to_string(),
                    };
                }
                if let Some(status) = reqwest_err.status() {
                    return LlmError::from_http_status(status.as_u16(), reqwest_err.to_string());
                }
                LlmError::NetworkError {
                    message: reqwest_err.to_string(),
                }
            }
            OpenAIError::ApiError(ref api_err) => {
                // async-openai doesn't expose the HTTP status on ApiError, so
                // classify on the provider's `type`/`code` strings. These are
                // the values used by OpenAI and honored by most compatible
                // providers (OpenRouter, DeepSeek, Anthropic-via-proxy, …).
                let kind = api_err.r#type.as_deref().unwrap_or("");
                let code = api_err.code.as_deref().unwrap_or("");
                let message = api_err.message.clone();
                match (kind, code) {
                    ("server_error", _) | (_, "server_error") => LlmError::ServerError {
                        status: 500,
                        message,
                    },
                    (_, "rate_limit_exceeded") => LlmError::RateLimited {
                        retry_after_duration: None,
                        message,
                    },
                    ("insufficient_quota", _) | (_, "insufficient_quota") => {
                        LlmError::InsufficientCredits { message }
                    }
                    ("authentication_error" | "permission_error", _)
                    | (_, "invalid_api_key" | "unauthorized") => LlmError::AuthFailed {
                        status: 401,
                        message,
                    },
                    _ => LlmError::InvalidRequest { message },
                }
            }
            OpenAIError::JSONDeserialize(ref e, ref raw_body) => {
                // async-openai's ApiError models `code` as `Option<String>`, but
                // OpenRouter's error bodies use an integer `code` (the HTTP status).
                // That trips async-openai's strict deserialize and we land here
                // with the raw body in hand — try a looser parse before giving up.
                if let Some(classified) = classify_error_body(raw_body) {
                    return classified;
                }
                LlmError::InvalidRequest {
                    message: format!("Response deserialize error: {e}"),
                }
            }
            OpenAIError::InvalidArgument(ref message) => LlmError::InvalidRequest {
                message: message.clone(),
            },
            OpenAIError::StreamError(ref e) => LlmError::NetworkError {
                message: e.to_string(),
            },
            OpenAIError::FileSaveError(ref m) | OpenAIError::FileReadError(ref m) => {
                LlmError::InvalidRequest { message: m.clone() }
            }
        }
    }
}

/// Best-effort classification of a raw error body when the SDK's strict
/// deserializer couldn't parse it.
///
/// Handles two common shapes:
///
/// * OpenRouter — `{"error": {"message", "code": <int>, "metadata": {"raw", "provider_name"}, ...}}`
///   (the `raw` metadata field often wraps the upstream provider's own error body)
/// * OpenAI-ish — `{"error": {"message", "type", "code": "..."}}`
///
/// Returns `None` if the body doesn't match either shape, so the caller can
/// fall back to a generic InvalidRequest.
fn classify_error_body(body: &str) -> Option<LlmError> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let err_obj = value.get("error")?;

    // Prefer the innermost upstream message when OpenRouter wraps it.
    let upstream_message = err_obj
        .get("metadata")
        .and_then(|m| m.get("raw"))
        .and_then(|r| r.as_str())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
        });

    let message = upstream_message
        .or_else(|| {
            err_obj
                .get("message")
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.to_string());

    // `code` can be an integer (OpenRouter: HTTP status) or a string (OpenAI: symbolic).
    let status_from_int = err_obj.get("code").and_then(|c| c.as_u64()).and_then(|n| {
        if (100..=599).contains(&n) {
            Some(n as u16)
        } else {
            None
        }
    });

    if let Some(status) = status_from_int {
        return Some(LlmError::from_http_status(status, message));
    }

    let code = err_obj
        .get("code")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let kind = err_obj
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();

    Some(match (kind, code) {
        (_, "rate_limit_exceeded") => LlmError::RateLimited {
            retry_after_duration: None,
            message,
        },
        ("server_error", _) => LlmError::ServerError {
            status: 500,
            message,
        },
        ("authentication_error" | "permission_error", _)
        | (_, "invalid_api_key" | "unauthorized") => LlmError::AuthFailed {
            status: 401,
            message,
        },
        ("insufficient_quota", _) | (_, "insufficient_quota") => {
            LlmError::InsufficientCredits { message }
        }
        _ => LlmError::InvalidRequest { message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_http_status_retryable() {
        let err = LlmError::from_http_status(429, "Too Many Requests".into());
        assert!(err.is_retryable());
        assert_eq!(err.status(), Some(429));
        assert!(matches!(err, LlmError::RateLimited { .. }));

        let err = LlmError::from_http_status(502, "Bad Gateway".into());
        assert!(err.is_retryable());
        assert_eq!(err.status(), Some(502));

        let err = LlmError::from_http_status(503, "Service Unavailable".into());
        assert!(err.is_retryable());

        let err = LlmError::from_http_status(408, "Request Timeout".into());
        assert!(err.is_retryable());
        assert!(matches!(err, LlmError::Timeout));
    }

    #[test]
    fn test_from_http_status_non_retryable() {
        let err = LlmError::from_http_status(401, "Unauthorized".into());
        assert!(!err.is_retryable());
        assert!(err.is_auth_error());

        let err = LlmError::from_http_status(403, "Forbidden".into());
        assert!(!err.is_retryable());
        assert!(err.is_auth_error());

        let err = LlmError::from_http_status(402, "Payment Required".into());
        assert!(!err.is_retryable());
        assert!(err.is_auth_error());

        let err = LlmError::from_http_status(400, "Bad Request".into());
        assert!(!err.is_retryable());
        assert!(err.is_client_error());
    }

    #[test]
    fn test_retry_after() {
        let err = LlmError::RateLimited {
            retry_after_duration: Some(Duration::from_secs(5)),
            message: "slow down".into(),
        };
        assert_eq!(err.retry_after(), Some(Duration::from_secs(5)));

        let err = LlmError::ServerError {
            status: 502,
            message: "bad gateway".into(),
        };
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_is_helpers_exhaustive() {
        // CircuitOpen is none of the categories (it's a special state)
        let err = LlmError::CircuitOpen;
        assert!(!err.is_retryable());
        assert!(!err.is_auth_error());
        assert!(!err.is_client_error());
    }

    #[test]
    fn test_display_formatting() {
        let err = LlmError::RateLimited {
            retry_after_duration: Some(Duration::from_secs(30)),
            message: "slow down".into(),
        };
        assert_eq!(err.to_string(), "Rate limited: slow down");

        let err = LlmError::RateLimited {
            retry_after_duration: None,
            message: "slow down".into(),
        };
        assert_eq!(err.to_string(), "Rate limited: slow down");

        let err = LlmError::ServerError {
            status: 502,
            message: "Bad Gateway".into(),
        };
        assert_eq!(err.to_string(), "Server error (HTTP 502): Bad Gateway");
    }

    #[test]
    fn test_from_openai_error_rate_limit_code() {
        let api_err = async_openai::error::OpenAIError::ApiError(async_openai::error::ApiError {
            message: "Too Many Requests".into(),
            r#type: None,
            param: None,
            code: Some("rate_limit_exceeded".into()),
        });
        let err = LlmError::from_openai_error(api_err);
        assert!(err.is_retryable());
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[test]
    fn test_from_openai_error_server_error_type() {
        let api_err = async_openai::error::OpenAIError::ApiError(async_openai::error::ApiError {
            message: "upstream failure".into(),
            r#type: Some("server_error".into()),
            param: None,
            code: None,
        });
        let err = LlmError::from_openai_error(api_err);
        assert!(err.is_retryable());
        assert!(matches!(err, LlmError::ServerError { .. }));
    }

    #[test]
    fn test_from_openai_error_invalid_request_fallback() {
        let api_err = async_openai::error::OpenAIError::ApiError(async_openai::error::ApiError {
            message: "something went wrong".into(),
            r#type: Some("invalid_request_error".into()),
            param: None,
            code: None,
        });
        let err = LlmError::from_openai_error(api_err);
        assert!(err.is_client_error());
    }

    #[test]
    fn test_from_openai_error_invalid_argument() {
        let api_err = async_openai::error::OpenAIError::InvalidArgument("bad".into());
        let err = LlmError::from_openai_error(api_err);
        assert!(err.is_client_error());
    }

    #[test]
    fn test_classify_openrouter_error_body_integer_code() {
        // OpenRouter returns `code` as an integer HTTP status — the SDK's
        // strict deserializer can't handle that, so we classify from the raw
        // body. The upstream provider's message is preferred when present.
        let body = r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"error\":{\"message\":\"The `reasoning_content` in the thinking mode must be passed back to the API.\",\"type\":\"invalid_request_error\"}}","provider_name":"DeepSeek","is_byok":false}},"user_id":"u"}"#;
        let err = classify_error_body(body).expect("should parse OpenRouter body");
        assert!(err.is_client_error());
        assert_eq!(err.status(), Some(400));
        let msg = err.to_string();
        assert!(
            msg.contains("reasoning_content"),
            "expected upstream message, got: {msg}"
        );
    }

    #[test]
    fn test_classify_openrouter_error_body_integer_code_server() {
        let body = r#"{"error":{"message":"Upstream unavailable","code":502}}"#;
        let err = classify_error_body(body).expect("should parse");
        assert!(err.is_retryable());
        assert_eq!(err.status(), Some(502));
    }

    #[test]
    fn test_classify_error_body_rate_limit_code() {
        let body =
            r#"{"error":{"message":"slow down","type":"rate_limit","code":"rate_limit_exceeded"}}"#;
        let err = classify_error_body(body).expect("should parse");
        assert!(err.is_retryable());
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[test]
    fn test_classify_error_body_garbage_returns_none() {
        assert!(classify_error_body("not json").is_none());
        // Valid JSON but no `error` key is also None.
        assert!(classify_error_body(r#"{"foo": "bar"}"#).is_none());
    }

    #[test]
    fn test_error_size() {
        // Ensure boxed variant keeps the top-level Error small
        assert!(
            std::mem::size_of::<Error>() <= 24,
            "Error enum is {} bytes, should be ≤ 24 (pointer + discriminant)",
            std::mem::size_of::<Error>()
        );
    }
}
