//! HTTP error mapping for the server.
//!
//! Maps `localdb_core::Error` to HTTP status codes per specs/05-surfaces.md §5.

use axum::{
    extract::{rejection::JsonRejection, FromRequest, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use localdb_core::Error as CoreError;

/// JSON error response body.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    /// Stable error code (snake_case).
    pub code: String,
    /// Human-readable error message.
    pub message: String,
}

/// Wraps a `CoreError` so it can be returned from axum handlers.
#[derive(Debug)]
pub struct ApiError(pub CoreError);

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = http_status_for(&self.0);
        let body = ErrorResponse {
            code: self.0.code().to_string(),
            message: error_response_message(&self.0),
        };
        (status, Json(body)).into_response()
    }
}

/// The `message` field of a JSON error response body.
///
/// Bare (`raw_message()`), not the full `Display` string
/// (`to_string()`): a daemon HTTP client (`cli::daemon_client::decode_daemon_error`)
/// reconstructs the typed error via `Error::from_code(code, message)`, which
/// re-adds the `Display` prefix (e.g. "invalid config: "). Storing the
/// already-prefixed string here would double it (issue #187 review, finding
/// F4). Variants `raw_message()` can't reconstruct fall back to the full
/// `Display` string, since there's no bare field to store instead.
fn error_response_message(err: &CoreError) -> String {
    err.raw_message()
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string())
}

/// A JSON request body extractor whose deserialization failures round-trip
/// through the same `ApiError` shape every other request-validation failure
/// on this daemon does, rather than axum's own default `JsonRejection`
/// response — plain text, and (for a data/type error, as opposed to a
/// syntax error) `422` rather than `400`.
///
/// Every malformed field on a request body is `invalid_request`, `400` —
/// the same status a semantically-invalid-but-well-typed value already gets
/// (e.g. `CreateJobRequest.deletion_policy: "obliterate"`, rejected by
/// `parse_deletion_policy`). Without this wrapper, a *type*-mismatched field
/// (e.g. `refetch: "yes"` where a bool is expected) would be rejected by
/// axum's `Json<T>` extractor itself, before the handler ever runs, and
/// never see that treatment — specs/05-surfaces.md's `POST /v1/jobs` body
/// documents `refetch`'s non-bool case as `invalid_request`/`400`
/// explicitly, so this wrapper is what makes that true.
///
/// Only *body-content* failures are enveloped. Transport-level rejections —
/// a missing/wrong `Content-Type` (`415`) or a body over the size limit
/// (`413`) — pass through as axum's stock responses, exactly as every
/// plain-`Json` route on this daemon returns them: the spec's error
/// taxonomy (specs/05-surfaces.md §5) has no codes for those conditions,
/// and collapsing them into a `400` would misreport what the client did
/// wrong.
pub struct ApiJson<T>(pub T);

/// [`ApiJson`]'s rejection: the `invalid_request` envelope for a malformed
/// body, or axum's own stock response for everything else (see
/// [`ApiJson`]'s doc comment).
pub enum ApiJsonRejection {
    Invalid(ApiError),
    Passthrough(Response),
}

impl IntoResponse for ApiJsonRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Invalid(e) => e.into_response(),
            Self::Passthrough(r) => r,
        }
    }
}

impl<S, T> FromRequest<S> for ApiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiJsonRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(
                rejection @ (JsonRejection::JsonDataError(_) | JsonRejection::JsonSyntaxError(_)),
            ) => Err(ApiJsonRejection::Invalid(ApiError(
                CoreError::InvalidRequest {
                    message: format!("invalid request body: {rejection}"),
                },
            ))),
            Err(other) => Err(ApiJsonRejection::Passthrough(other.into_response())),
        }
    }
}

/// Map a `CoreError` to an HTTP status code per specs/05-surfaces.md §5.
pub fn http_status_for(err: &CoreError) -> StatusCode {
    match err {
        CoreError::StoreNotFound { .. }
        | CoreError::SourceNotFound { .. }
        | CoreError::ResourceNotFound { .. }
        | CoreError::JobNotFound { .. } => StatusCode::NOT_FOUND,

        CoreError::RuntimeStateLocked
        | CoreError::DaemonRunning
        | CoreError::IndexInProgress
        | CoreError::JobCancelled
        | CoreError::JobAlreadyTerminal => StatusCode::CONFLICT,

        CoreError::DaemonUnreachable
        | CoreError::ProviderUnavailable { .. }
        | CoreError::RateLimited { .. } => StatusCode::BAD_GATEWAY,

        CoreError::InvalidConfig { .. }
        | CoreError::UnsupportedFormat { .. }
        | CoreError::ExtractionFailed { .. } => StatusCode::UNPROCESSABLE_ENTITY,

        CoreError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,

        // Raised client-side by the CLI about the daemon it is talking to,
        // never by the daemon about itself; mapped for exhaustiveness, and
        // 503 is the honest reading either way.
        CoreError::ModelMissing { .. } | CoreError::DaemonCapabilityUnavailable { .. } => {
            StatusCode::SERVICE_UNAVAILABLE
        }

        CoreError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests;
