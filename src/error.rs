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

    /// Classify an `openai_api_rs` APIError into a structured LlmError.
    ///
    /// The SDK's `APIError` has two variants:
    /// - `ReqwestError(reqwest::Error)` — network/HTTP-level failures
    /// - `CustomError { message }` — API-level errors, formatted as "{status}: {body}"
    pub fn from_api_error(err: openai_api_rs::v1::error::APIError) -> Self {
        match err {
            openai_api_rs::v1::error::APIError::ReqwestError(ref reqwest_err) => {
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
                // Generic network error
                LlmError::NetworkError {
                    message: reqwest_err.to_string(),
                }
            }
            openai_api_rs::v1::error::APIError::CustomError { ref message } => {
                // The SDK formats errors as "{status_code}: {body}"
                // Try to extract the status code
                if let Some((status_str, rest)) = message
                    .strip_prefix("APIError: ")
                    .unwrap_or(message)
                    .split_once(": ")
                {
                    if let Ok(status) = status_str.trim().parse::<u16>() {
                        return LlmError::from_http_status(status, rest.to_string());
                    }
                }
                // Couldn't parse status — treat as invalid request
                LlmError::InvalidRequest {
                    message: message.clone(),
                }
            }
        }
    }
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
    fn test_from_api_error_custom_with_status() {
        // Simulate the SDK's "{status}: {body}" format
        let api_err = openai_api_rs::v1::error::APIError::CustomError {
            message: "429: Too Many Requests".into(),
        };
        let err = LlmError::from_api_error(api_err);
        assert!(err.is_retryable());
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[test]
    fn test_from_api_error_custom_without_status() {
        let api_err = openai_api_rs::v1::error::APIError::CustomError {
            message: "something went wrong".into(),
        };
        let err = LlmError::from_api_error(api_err);
        assert!(err.is_client_error());
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
