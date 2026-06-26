//! Shared error taxonomy for all localdb surfaces.
//!
//! One enum; every surface maps it mechanically:
//! - HTTP status codes (server crate)
//! - CLI exit codes + stderr (cli crate)
//! - MCP tool errors (mcp crate)
//!
//! Error codes are stable API.

use thiserror::Error;

/// The shared error type for all localdb operations.
///
/// Every surface maps this enum to its own representation.
/// See specs/05-surfaces.md §5 for the full mapping table.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum Error {
    /// Unknown store entity.
    #[error("store not found: {id}")]
    StoreNotFound { id: String },

    /// Unknown source entity.
    #[error("source not found: {id}")]
    SourceNotFound { id: String },

    /// Unknown document entity.
    #[error("document not found: {id}")]
    DocumentNotFound { id: String },

    /// Unknown job entity.
    #[error("job not found: {id}")]
    JobNotFound { id: String },

    /// The runtime-state database write lock could not be acquired within the
    /// busy timeout (5 s). Another writer held the lock longer than expected.
    /// Try again shortly.
    ///
    /// CLI exit code: 4
    #[error(
        "runtime-state database write lock could not be acquired within the busy timeout; \
         try again shortly"
    )]
    RuntimeStateLocked,

    /// A daemon is already running when one is not expected.
    ///
    /// CLI exit code: 4
    #[error("daemon is already running")]
    DaemonRunning,

    /// The daemon is not reachable when one is required.
    ///
    /// CLI exit code: 5
    #[error("daemon is unreachable")]
    DaemonUnreachable,

    /// Attempted API write to a YAML-owned object.
    ///
    /// See specs/03-config.md §3 for ownership model.
    #[error("this object is owned by the config file and cannot be mutated via the API")]
    ConfigReadonly,

    /// Config failed validation; message contains path-precise error.
    #[error("invalid config: {message}")]
    InvalidConfig { message: String },

    /// Bad arguments or request body.
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    /// Extraction can't handle the file type; informational in job stats.
    #[error("unsupported format: {format}")]
    UnsupportedFormat { format: String },

    /// A recognized, supported format whose contents could not be extracted
    /// (e.g. a corrupt or truncated DOCX/PDF). Distinct from `UnsupportedFormat`
    /// (format not handled) and `Internal` (a bug in our code).
    #[error("extraction failed for {format}: {reason}")]
    ExtractionFailed { format: String, reason: String },

    /// External embedding endpoint is down or misconfigured.
    ///
    /// CLI exit code: 5
    #[error("provider unavailable: {message}")]
    ProviderUnavailable { message: String },

    /// Local model not yet downloaded.
    ///
    /// Message includes the fix (e.g. run `localdb init`).
    /// CLI exit code: 5
    #[error("model missing: {message}")]
    ModelMissing { message: String },

    /// A conflicting index job is already running for this scope.
    ///
    /// CLI exit code: 4
    #[error("index already in progress for this scope")]
    IndexInProgress,

    /// Internal bug; includes correlation id, logged with backtrace.
    ///
    /// CLI exit code: 1
    #[error("internal error (correlation_id={correlation_id}): {message}")]
    Internal {
        message: String,
        correlation_id: String,
    },
}

impl Error {
    /// Returns the stable string code used in JSON error responses.
    pub fn code(&self) -> &'static str {
        match self {
            Error::StoreNotFound { .. } => "store_not_found",
            Error::SourceNotFound { .. } => "source_not_found",
            Error::DocumentNotFound { .. } => "document_not_found",
            Error::JobNotFound { .. } => "job_not_found",
            Error::RuntimeStateLocked => "runtime_state_locked",
            Error::DaemonRunning => "daemon_running",
            Error::DaemonUnreachable => "daemon_unreachable",
            Error::ConfigReadonly => "config_readonly",
            Error::InvalidConfig { .. } => "invalid_config",
            Error::InvalidRequest { .. } => "invalid_request",
            Error::UnsupportedFormat { .. } => "unsupported_format",
            Error::ExtractionFailed { .. } => "extraction_failed",
            Error::ProviderUnavailable { .. } => "provider_unavailable",
            Error::ModelMissing { .. } => "model_missing",
            Error::IndexInProgress => "index_in_progress",
            Error::Internal { .. } => "internal",
        }
    }

    /// Returns the suggested CLI exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Internal { .. } => 1,
            Error::InvalidConfig { .. } | Error::InvalidRequest { .. } => 2,
            Error::StoreNotFound { .. }
            | Error::SourceNotFound { .. }
            | Error::DocumentNotFound { .. }
            | Error::JobNotFound { .. } => 3,
            Error::RuntimeStateLocked
            | Error::DaemonRunning
            | Error::ConfigReadonly
            | Error::IndexInProgress => 4,
            Error::DaemonUnreachable
            | Error::ProviderUnavailable { .. }
            | Error::ModelMissing { .. } => 5,
            Error::UnsupportedFormat { .. } | Error::ExtractionFailed { .. } => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable() {
        // Verify every variant has a known stable code
        let cases: &[(Error, &str, i32)] = &[
            (
                Error::StoreNotFound { id: "x".into() },
                "store_not_found",
                3,
            ),
            (
                Error::SourceNotFound { id: "x".into() },
                "source_not_found",
                3,
            ),
            (
                Error::DocumentNotFound { id: "x".into() },
                "document_not_found",
                3,
            ),
            (Error::JobNotFound { id: "x".into() }, "job_not_found", 3),
            (Error::RuntimeStateLocked, "runtime_state_locked", 4),
            (Error::DaemonRunning, "daemon_running", 4),
            (Error::DaemonUnreachable, "daemon_unreachable", 5),
            (Error::ConfigReadonly, "config_readonly", 4),
            (
                Error::InvalidConfig {
                    message: "m".into(),
                },
                "invalid_config",
                2,
            ),
            (
                Error::InvalidRequest {
                    message: "m".into(),
                },
                "invalid_request",
                2,
            ),
            (
                Error::UnsupportedFormat {
                    format: "pdf".into(),
                },
                "unsupported_format",
                2,
            ),
            (
                Error::ExtractionFailed {
                    format: "office/docx".into(),
                    reason: "zip error".into(),
                },
                "extraction_failed",
                2,
            ),
            (
                Error::ProviderUnavailable {
                    message: "m".into(),
                },
                "provider_unavailable",
                5,
            ),
            (
                Error::ModelMissing {
                    message: "m".into(),
                },
                "model_missing",
                5,
            ),
            (Error::IndexInProgress, "index_in_progress", 4),
            (
                Error::Internal {
                    message: "bug".into(),
                    correlation_id: "abc123".into(),
                },
                "internal",
                1,
            ),
        ];

        for (err, expected_code, expected_exit) in cases {
            assert_eq!(err.code(), *expected_code, "code mismatch for {:?}", err);
            assert_eq!(
                err.exit_code(),
                *expected_exit,
                "exit_code mismatch for {:?}",
                err
            );
        }
    }

    #[test]
    fn error_display_contains_context() {
        let err = Error::StoreNotFound {
            id: "my-store".into(),
        };
        assert!(err.to_string().contains("my-store"));

        let err = Error::Internal {
            message: "something broke".into(),
            correlation_id: "corr-1".into(),
        };
        assert!(err.to_string().contains("corr-1"));
        assert!(err.to_string().contains("something broke"));
    }

    #[test]
    fn all_not_found_variants_exit_3() {
        assert_eq!(Error::StoreNotFound { id: "s".into() }.exit_code(), 3);
        assert_eq!(Error::SourceNotFound { id: "s".into() }.exit_code(), 3);
        assert_eq!(Error::DocumentNotFound { id: "s".into() }.exit_code(), 3);
        assert_eq!(Error::JobNotFound { id: "s".into() }.exit_code(), 3);
    }

    #[test]
    fn conflict_errors_exit_4() {
        assert_eq!(Error::RuntimeStateLocked.exit_code(), 4);
        assert_eq!(Error::DaemonRunning.exit_code(), 4);
        assert_eq!(Error::ConfigReadonly.exit_code(), 4);
        assert_eq!(Error::IndexInProgress.exit_code(), 4);
    }

    #[test]
    fn unavailable_errors_exit_5() {
        assert_eq!(Error::DaemonUnreachable.exit_code(), 5);
        assert_eq!(
            Error::ProviderUnavailable {
                message: "m".into()
            }
            .exit_code(),
            5
        );
        assert_eq!(
            Error::ModelMissing {
                message: "m".into()
            }
            .exit_code(),
            5
        );
    }
}
