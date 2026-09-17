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

    /// The running daemon is healthy but predates a capability the request
    /// needs, and would silently answer as though the request had not asked
    /// for it (`SearchRequest` ignores unknown fields). Distinct from
    /// [`Error::DaemonUnreachable`]: the daemon answers, it just answers
    /// wrongly. Message includes the fix (restart the daemon).
    ///
    /// CLI exit code: 5
    #[error("daemon capability unavailable: {message}")]
    DaemonCapabilityUnavailable { message: String },

    /// A conflicting index job is already running for this scope.
    ///
    /// CLI exit code: 4
    #[error("index already in progress for this scope")]
    IndexInProgress,

    /// A job was cancelled via `DELETE /v1/jobs/{id}` (issue #218) before it
    /// reached a normal terminal state. Recorded as the job's `Failed`
    /// state with `error_code: "job_cancelled"` — never a fifth
    /// `IndexJobState` variant — so `cli::job_attach::finish_job`
    /// reconstructs exactly this variant via `Error::from_code` when a
    /// daemon-attached CLI (e.g. `localdb index`) observes a job it didn't
    /// itself cancel end this way.
    ///
    /// CLI exit code: 4
    #[error("job was cancelled")]
    JobCancelled,

    /// `DELETE /v1/jobs/{id}` was requested for a job that has already
    /// reached a terminal state (`done` or `failed`) — cancellation must
    /// never overwrite a recorded outcome, so this is reported instead of
    /// silently no-oping or retroactively rewriting the job's history.
    ///
    /// CLI exit code: 4
    #[error("job already reached a terminal state; cannot cancel")]
    JobAlreadyTerminal,

    /// Internal bug; includes correlation id, logged with backtrace.
    ///
    /// CLI exit code: 1
    #[error("internal error (correlation_id={correlation_id}): {message}")]
    Internal {
        message: String,
        correlation_id: String,
    },

    /// Missing or invalid credentials on a request that requires authentication.
    ///
    /// HTTP 401. CLI exit code: 6.
    #[error("unauthorized: {message}")]
    Unauthorized { message: String },

    /// Valid credentials, but the principal lacks permission for the requested
    /// operation (e.g. a member attempting an admin-only action, or attempting
    /// to access a store they hold no grant for).
    ///
    /// HTTP 403. CLI exit code: 6.
    #[error("forbidden: {message}")]
    Forbidden { message: String },
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
            Error::DaemonCapabilityUnavailable { .. } => "daemon_capability_unavailable",
            Error::ModelMissing { .. } => "model_missing",
            Error::IndexInProgress => "index_in_progress",
            Error::JobCancelled => "job_cancelled",
            Error::JobAlreadyTerminal => "job_already_terminal",
            Error::Internal { .. } => "internal",
            Error::Unauthorized { .. } => "unauthorized",
            Error::Forbidden { .. } => "forbidden",
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
            "job_cancelled" => Error::JobCancelled,
            "job_already_terminal" => Error::JobAlreadyTerminal,
            "provider_unavailable" => Error::ProviderUnavailable { message },
            "daemon_capability_unavailable" => Error::DaemonCapabilityUnavailable { message },
            "model_missing" => Error::ModelMissing { message },
            "rate_limited" => Error::RateLimited { message },
            "unauthorized" => Error::Unauthorized { message },
            "forbidden" => Error::Forbidden { message },
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
            | Error::DaemonCapabilityUnavailable { message }
            | Error::ModelMissing { message }
            | Error::RateLimited { message }
            | Error::Unauthorized { message }
            | Error::Forbidden { message } => Some(message),
            Error::RuntimeStateLocked
            | Error::DaemonRunning
            | Error::DaemonUnreachable
            | Error::UnsupportedFormat { .. }
            | Error::ExtractionFailed { .. }
            | Error::IndexInProgress
            | Error::JobCancelled
            | Error::JobAlreadyTerminal
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
            Error::RuntimeStateLocked
            | Error::DaemonRunning
            | Error::IndexInProgress
            | Error::JobCancelled
            | Error::JobAlreadyTerminal => 4,
            Error::DaemonUnreachable
            | Error::DaemonCapabilityUnavailable { .. }
            | Error::ProviderUnavailable { .. }
            | Error::ModelMissing { .. }
            | Error::RateLimited { .. } => 5,
            Error::UnsupportedFormat { .. } | Error::ExtractionFailed { .. } => 2,
            Error::Unauthorized { .. } | Error::Forbidden { .. } => 6,
        }
    }
}

#[cfg(test)]
mod tests;
