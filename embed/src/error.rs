//! Embed-crate-specific error types.

use thiserror::Error;

/// Errors that can occur during embedding operations.
#[derive(Debug, Error)]
pub enum EmbedError {
    /// Local model is not in the cache and downloads are disabled.
    #[error("model missing: {0}\nHint: run `localdb init` to download the default model, or set `LOCALDB_ALLOW_MODEL_DOWNLOAD=1`.")]
    ModelMissing(String),

    /// The model checksum does not match the expected value.
    #[error("model checksum mismatch for {model}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        model: String,
        expected: String,
        actual: String,
    },

    /// A hosted provider returned an error or is unreachable.
    #[error("provider error ({provider}): {message}")]
    ProviderError { provider: String, message: String },

    /// All retry attempts exhausted.
    #[error("provider {provider} unavailable after {attempts} attempts: {last_error}")]
    RetriesExhausted {
        provider: String,
        attempts: u32,
        last_error: String,
    },

    /// Request timed out.
    #[error("embedding request to {provider} timed out after {timeout_secs}s")]
    Timeout { provider: String, timeout_secs: u64 },

    /// I/O error during model download or cache access.
    #[error("model cache I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP error.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON parsing error.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// Generic internal error.
    #[error("internal embed error: {0}")]
    Internal(String),
}

impl From<EmbedError> for localdb_core::Error {
    fn from(e: EmbedError) -> localdb_core::Error {
        match e {
            EmbedError::ModelMissing(msg) => localdb_core::Error::ModelMissing { message: msg },
            EmbedError::ChecksumMismatch {
                model,
                expected,
                actual,
            } => localdb_core::Error::Internal {
                message: format!(
                    "model checksum mismatch for {model}: expected {expected}, got {actual}"
                ),
                correlation_id: "checksum".to_string(),
            },
            EmbedError::ProviderError { provider, message } => {
                localdb_core::Error::ProviderUnavailable {
                    message: format!("{provider}: {message}"),
                }
            }
            EmbedError::RetriesExhausted {
                provider,
                attempts,
                last_error,
            } => localdb_core::Error::ProviderUnavailable {
                message: format!("{provider} unavailable after {attempts} attempts: {last_error}"),
            },
            EmbedError::Timeout {
                provider,
                timeout_secs,
            } => localdb_core::Error::ProviderUnavailable {
                message: format!("{provider} timed out after {timeout_secs}s"),
            },
            EmbedError::Io(e) => localdb_core::Error::Internal {
                message: format!("I/O error: {e}"),
                correlation_id: "io".to_string(),
            },
            EmbedError::Http(e) => localdb_core::Error::ProviderUnavailable {
                message: format!("HTTP error: {e}"),
            },
            EmbedError::Json(e) => localdb_core::Error::Internal {
                message: format!("JSON error: {e}"),
                correlation_id: "json".to_string(),
            },
            EmbedError::Internal(msg) => localdb_core::Error::Internal {
                message: msg,
                correlation_id: "embed".to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::Error as CoreError;

    #[test]
    fn model_missing_maps_to_core_error() {
        let e = EmbedError::ModelMissing("bge-small-en-v1.5 not found".to_string());
        let core: CoreError = e.into();
        assert_eq!(core.code(), "model_missing");
    }

    #[test]
    fn provider_error_maps_to_core_error() {
        let e = EmbedError::ProviderError {
            provider: "openai".to_string(),
            message: "401 Unauthorized".to_string(),
        };
        let core: CoreError = e.into();
        assert_eq!(core.code(), "provider_unavailable");
    }

    #[test]
    fn retries_exhausted_maps_to_provider_unavailable() {
        let e = EmbedError::RetriesExhausted {
            provider: "perplexity".to_string(),
            attempts: 3,
            last_error: "connection refused".to_string(),
        };
        let core: CoreError = e.into();
        assert_eq!(core.code(), "provider_unavailable");
    }

    #[test]
    fn timeout_maps_to_provider_unavailable() {
        let e = EmbedError::Timeout {
            provider: "voyage".to_string(),
            timeout_secs: 30,
        };
        let core: CoreError = e.into();
        assert_eq!(core.code(), "provider_unavailable");
    }

    #[test]
    fn model_missing_display_has_hint() {
        let e = EmbedError::ModelMissing("model not found".to_string());
        let msg = e.to_string();
        assert!(
            msg.contains("localdb init") || msg.contains("LOCALDB_ALLOW_MODEL_DOWNLOAD"),
            "model_missing error should have actionable hint: {msg}"
        );
    }
}
