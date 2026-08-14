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

    /// Unknown document/resource entity.
    #[error("resource not found: {id}")]
    ResourceNotFound { id: String },

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

    /// Upstream rate limit exceeded; retries exhausted.
    ///
    /// CLI exit code: 5
    #[error("rate limited: {message}")]
    RateLimited { message: String },
}

impl Error {
    /// Returns the stable string code used in JSON error responses.
    pub fn code(&self) -> &'static str {
        match self {
            Error::StoreNotFound { .. } => "store_not_found",
            Error::SourceNotFound { .. } => "source_not_found",
            Error::ResourceNotFound { .. } => "resource_not_found",
            Error::JobNotFound { .. } => "job_not_found",
            Error::RuntimeStateLocked => "runtime_state_locked",
            Error::DaemonRunning => "daemon_running",
            Error::DaemonUnreachable => "daemon_unreachable",
            Error::InvalidConfig { .. } => "invalid_config",
            Error::InvalidRequest { .. } => "invalid_request",
            Error::UnsupportedFormat { .. } => "unsupported_format",
            Error::ExtractionFailed { .. } => "extraction_failed",
            Error::ProviderUnavailable { .. } => "provider_unavailable",
            Error::ModelMissing { .. } => "model_missing",
            Error::IndexInProgress => "index_in_progress",
            Error::Internal { .. } => "internal",
            Error::RateLimited { .. } => "rate_limited",
        }
    }

    /// Reconstruct a typed `Error` from a stable `code()` string plus a
    /// message, the inverse of [`Error::code`].
    ///
    /// Every surface that receives an error as a `{code, message}` pair
    /// across a boundary — a daemon HTTP error body
    /// (`cli::daemon_client::decode_daemon_error`) or a failed `IndexJob`'s
    /// `error_code`/`error` fields (`cli::job_attach::finish_job`) —
    /// reconstructs the original variant through this one mapping, so the
    /// code taxonomy only has to be kept in sync in one place. `message` is
    /// reused verbatim for every variant's message-shaped field (`id`,
    /// `message`, ...) — the original field *name* isn't recoverable, but
    /// every consumer only ever displays the string, never inspects it
    /// structurally.
    ///
    /// Returns `None` for a code this binary doesn't recognize (e.g. a newer
    /// code string from a daemon build ahead of this CLI, or a variant like
    /// `Error::Internal`/`Error::UnsupportedFormat`/`Error::ExtractionFailed`
    /// whose fields don't fit a single `message` string) — callers supply
    /// their own fallback (typically `Error::Internal`) with whatever extra
    /// context (HTTP status, a wrapping label, ...) is available to them.
    pub fn from_code(code: &str, message: String) -> Option<Error> {
        Some(match code {
            "store_not_found" => Error::StoreNotFound { id: message },
            "source_not_found" => Error::SourceNotFound { id: message },
            "resource_not_found" => Error::ResourceNotFound { id: message },
            // Legacy code string from a stale daemon predating the
            // resource_not_found rename (specs/05-surfaces.md §5).
            "document_not_found" => Error::ResourceNotFound { id: message },
            "job_not_found" => Error::JobNotFound { id: message },
            "runtime_state_locked" => Error::RuntimeStateLocked,
            "daemon_running" => Error::DaemonRunning,
            "daemon_unreachable" => Error::DaemonUnreachable,
            "invalid_config" => Error::InvalidConfig { message },
            "invalid_request" => Error::InvalidRequest { message },
            "index_in_progress" => Error::IndexInProgress,
            "provider_unavailable" => Error::ProviderUnavailable { message },
            "model_missing" => Error::ModelMissing { message },
            "rate_limited" => Error::RateLimited { message },
            _ => return None,
        })
    }

    /// Returns the bare message field `from_code` would reconstruct this
    /// variant from, without the `Display` prefix (e.g. `"invalid config: "`)
    /// that `{self}` / `to_string()` adds.
    ///
    /// `Some` exactly for the 9 variants `from_code` maps back into via a
    /// single `message` string: the four `id`-carrying not-found variants,
    /// and
    /// `InvalidConfig`/`InvalidRequest`/`ProviderUnavailable`/`ModelMissing`/
    /// `RateLimited`'s `message`. `None` for every other variant, including
    /// ones `from_code` *decodes* to (`RuntimeStateLocked`, `DaemonRunning`,
    /// `DaemonUnreachable`, `IndexInProgress` carry no message at all) and
    /// ones it can't round-trip at all (`Internal`, `UnsupportedFormat`,
    /// `ExtractionFailed`).
    ///
    /// A producer that will later hand this error to a `{code, message}`
    /// boundary — a failed `IndexJob`'s `error` field
    /// (`ingestion::fail_index_job_with_error`), or an HTTP error body's
    /// `message` field (`server::error::ApiError::into_response`) — must
    /// store `raw_message().unwrap_or_else(|| self.to_string())` instead of
    /// `to_string()`: the consumer's `Error::from_code(code, message)`
    /// reconstructs the variant and re-adds the prefix through `Display`, so
    /// storing the already-prefixed string doubles it.
    pub fn raw_message(&self) -> Option<&str> {
        match self {
            Error::StoreNotFound { id }
            | Error::SourceNotFound { id }
            | Error::ResourceNotFound { id }
            | Error::JobNotFound { id } => Some(id),
            Error::InvalidConfig { message }
            | Error::InvalidRequest { message }
            | Error::ProviderUnavailable { message }
            | Error::ModelMissing { message }
            | Error::RateLimited { message } => Some(message),
            Error::RuntimeStateLocked
            | Error::DaemonRunning
            | Error::DaemonUnreachable
            | Error::UnsupportedFormat { .. }
            | Error::ExtractionFailed { .. }
            | Error::IndexInProgress
            | Error::Internal { .. } => None,
        }
    }

    /// Returns the suggested CLI exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Internal { .. } => 1,
            Error::InvalidConfig { .. } | Error::InvalidRequest { .. } => 2,
            Error::StoreNotFound { .. }
            | Error::SourceNotFound { .. }
            | Error::ResourceNotFound { .. }
            | Error::JobNotFound { .. } => 3,
            Error::RuntimeStateLocked | Error::DaemonRunning | Error::IndexInProgress => 4,
            Error::DaemonUnreachable
            | Error::ProviderUnavailable { .. }
            | Error::ModelMissing { .. }
            | Error::RateLimited { .. } => 5,
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
                Error::ResourceNotFound { id: "x".into() },
                "resource_not_found",
                3,
            ),
            (Error::JobNotFound { id: "x".into() }, "job_not_found", 3),
            (Error::RuntimeStateLocked, "runtime_state_locked", 4),
            (Error::DaemonRunning, "daemon_running", 4),
            (Error::DaemonUnreachable, "daemon_unreachable", 5),
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
            (
                Error::RateLimited {
                    message: "m".into(),
                },
                "rate_limited",
                5,
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
        assert_eq!(Error::ResourceNotFound { id: "s".into() }.exit_code(), 3);
        assert_eq!(Error::JobNotFound { id: "s".into() }.exit_code(), 3);
    }

    // -- from_code: round trip with code(), and the two documented gaps -----

    #[test]
    fn from_code_round_trips_every_code_with_a_message_field() {
        // Every variant whose `code()` output `from_code` claims to
        // recognize must decode back to an equal value when fed its own
        // `code()` + a representative message — this is what makes it safe
        // for `finish_job`/`decode_daemon_error` to reconstruct the typed
        // error a job or an HTTP error body only carries as a string pair.
        let cases: &[Error] = &[
            Error::StoreNotFound { id: "x".into() },
            Error::SourceNotFound { id: "x".into() },
            Error::ResourceNotFound { id: "x".into() },
            Error::JobNotFound { id: "x".into() },
            Error::RuntimeStateLocked,
            Error::DaemonRunning,
            Error::DaemonUnreachable,
            Error::InvalidConfig {
                message: "x".into(),
            },
            Error::InvalidRequest {
                message: "x".into(),
            },
            Error::IndexInProgress,
            Error::ProviderUnavailable {
                message: "x".into(),
            },
            Error::ModelMissing {
                message: "x".into(),
            },
            Error::RateLimited {
                message: "x".into(),
            },
        ];
        for err in cases {
            let decoded = Error::from_code(err.code(), "x".to_string());
            assert_eq!(decoded.as_ref(), Some(err), "round trip failed for {err:?}");
        }
    }

    #[test]
    fn from_code_accepts_the_legacy_document_not_found_alias() {
        assert_eq!(
            Error::from_code("document_not_found", "doc-1".to_string()),
            Some(Error::ResourceNotFound {
                id: "doc-1".to_string()
            })
        );
    }

    #[test]
    fn from_code_returns_none_for_an_unrecognized_or_unmappable_code() {
        // An unknown code (e.g. a newer daemon build) and every code whose
        // variant doesn't fit a single `message` field (internal,
        // unsupported_format, extraction_failed) all return `None` so the
        // caller applies its own fallback.
        assert_eq!(Error::from_code("something_new", "x".to_string()), None);
        assert_eq!(Error::from_code("internal", "x".to_string()), None);
        assert_eq!(
            Error::from_code("unsupported_format", "x".to_string()),
            None
        );
        assert_eq!(Error::from_code("extraction_failed", "x".to_string()), None);
    }

    // -- raw_message: the 9 reconstructible variants, and a few that aren't -

    #[test]
    fn raw_message_returns_the_bare_field_for_reconstructible_variants() {
        // Every variant `from_code` can rebuild from a single `message`
        // string must hand back exactly that field, unprefixed — this is
        // what lets a producer avoid double-prefixing when a consumer later
        // runs the string back through `from_code` + `Display`.
        assert_eq!(
            Error::StoreNotFound { id: "x".into() }.raw_message(),
            Some("x")
        );
        assert_eq!(
            Error::SourceNotFound { id: "x".into() }.raw_message(),
            Some("x")
        );
        assert_eq!(
            Error::ResourceNotFound { id: "x".into() }.raw_message(),
            Some("x")
        );
        assert_eq!(
            Error::JobNotFound { id: "x".into() }.raw_message(),
            Some("x")
        );
        assert_eq!(
            Error::InvalidConfig {
                message: "unconfigured embedder provider".into(),
            }
            .raw_message(),
            Some("unconfigured embedder provider")
        );
        assert_eq!(
            Error::InvalidRequest {
                message: "x".into()
            }
            .raw_message(),
            Some("x")
        );
        assert_eq!(
            Error::ProviderUnavailable {
                message: "x".into()
            }
            .raw_message(),
            Some("x")
        );
        assert_eq!(
            Error::ModelMissing {
                message: "x".into()
            }
            .raw_message(),
            Some("x")
        );
        assert_eq!(
            Error::RateLimited {
                message: "x".into()
            }
            .raw_message(),
            Some("x")
        );
    }

    #[test]
    fn raw_message_is_none_for_non_reconstructible_variants() {
        // Variants `from_code` never decodes (a fixed-message variant like
        // `RuntimeStateLocked`, or one whose fields don't fit a single
        // `message` string) have no bare message to hand back — callers must
        // fall back to `to_string()`.
        assert_eq!(Error::RuntimeStateLocked.raw_message(), None);
        assert_eq!(Error::DaemonRunning.raw_message(), None);
        assert_eq!(Error::DaemonUnreachable.raw_message(), None);
        assert_eq!(Error::IndexInProgress.raw_message(), None);
        assert_eq!(
            Error::UnsupportedFormat {
                format: "pdf".into()
            }
            .raw_message(),
            None
        );
        assert_eq!(
            Error::ExtractionFailed {
                format: "office/docx".into(),
                reason: "zip error".into(),
            }
            .raw_message(),
            None
        );
        assert_eq!(
            Error::Internal {
                message: "bug".into(),
                correlation_id: "abc".into(),
            }
            .raw_message(),
            None
        );
    }

    #[test]
    fn conflict_errors_exit_4() {
        assert_eq!(Error::RuntimeStateLocked.exit_code(), 4);
        assert_eq!(Error::DaemonRunning.exit_code(), 4);
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
        assert_eq!(
            Error::RateLimited {
                message: "m".into()
            }
            .exit_code(),
            5
        );
    }
}
