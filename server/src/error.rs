//! HTTP error mapping for the server.
//!
//! Maps `localdb_core::Error` to HTTP status codes per specs/05-surfaces.md §5.

use axum::{
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

/// Map a `CoreError` to an HTTP status code per specs/05-surfaces.md §5.
pub fn http_status_for(err: &CoreError) -> StatusCode {
    match err {
        CoreError::StoreNotFound { .. }
        | CoreError::SourceNotFound { .. }
        | CoreError::ResourceNotFound { .. }
        | CoreError::JobNotFound { .. } => StatusCode::NOT_FOUND,

        CoreError::RuntimeStateLocked | CoreError::DaemonRunning | CoreError::IndexInProgress => {
            StatusCode::CONFLICT
        }

        CoreError::DaemonUnreachable
        | CoreError::ProviderUnavailable { .. }
        | CoreError::RateLimited { .. } => StatusCode::BAD_GATEWAY,

        CoreError::InvalidConfig { .. }
        | CoreError::UnsupportedFormat { .. }
        | CoreError::ExtractionFailed { .. } => StatusCode::UNPROCESSABLE_ENTITY,

        CoreError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,

        CoreError::ModelMissing { .. } => StatusCode::SERVICE_UNAVAILABLE,

        CoreError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use localdb_core::Error;

    #[test]
    fn not_found_errors_map_to_404() {
        assert_eq!(
            http_status_for(&Error::StoreNotFound { id: "x".into() }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            http_status_for(&Error::SourceNotFound { id: "x".into() }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            http_status_for(&Error::ResourceNotFound { id: "x".into() }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            http_status_for(&Error::JobNotFound { id: "x".into() }),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn conflict_errors_map_to_409() {
        assert_eq!(
            http_status_for(&Error::RuntimeStateLocked),
            StatusCode::CONFLICT
        );
        assert_eq!(http_status_for(&Error::DaemonRunning), StatusCode::CONFLICT);
        assert_eq!(
            http_status_for(&Error::IndexInProgress),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn bad_gateway_errors_map_to_502() {
        assert_eq!(
            http_status_for(&Error::DaemonUnreachable),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            http_status_for(&Error::ProviderUnavailable {
                message: "m".into()
            }),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            http_status_for(&Error::RateLimited {
                message: "m".into()
            }),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn invalid_config_maps_to_422() {
        assert_eq!(
            http_status_for(&Error::InvalidConfig {
                message: "m".into()
            }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn unsupported_format_maps_to_422() {
        assert_eq!(
            http_status_for(&Error::UnsupportedFormat {
                format: "application/octet-stream".into()
            }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn extraction_failed_maps_to_422() {
        assert_eq!(
            http_status_for(&Error::ExtractionFailed {
                format: "office/docx".into(),
                reason: "zip error".into(),
            }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn invalid_request_maps_to_400() {
        assert_eq!(
            http_status_for(&Error::InvalidRequest {
                message: "m".into()
            }),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn model_missing_maps_to_503() {
        assert_eq!(
            http_status_for(&Error::ModelMissing {
                message: "m".into()
            }),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // -- error_response_message: bare message, no doubled Display prefix ----

    #[test]
    fn error_response_message_is_bare_for_reconstructible_variants() {
        // `decode_daemon_error` re-adds the "invalid config: " prefix via
        // `Error::from_code` + `Display`; the JSON body's `message` field
        // must NOT already carry it, or the CLI's rendered error doubles it.
        assert_eq!(
            error_response_message(&Error::InvalidConfig {
                message: "unconfigured embedder provider".into(),
            }),
            "unconfigured embedder provider"
        );
        assert_eq!(
            error_response_message(&Error::StoreNotFound { id: "s1".into() }),
            "s1"
        );
    }

    #[test]
    fn error_response_message_falls_back_to_display_for_non_reconstructible_variants() {
        // `Internal` has no single bare field `from_code` could rebuild from
        // (it needs both `message` and `correlation_id`), so the JSON body
        // must fall back to the full `Display` string.
        let err = Error::Internal {
            message: "bug".into(),
            correlation_id: "corr-1".into(),
        };
        assert_eq!(error_response_message(&err), err.to_string());
    }

    #[tokio::test]
    async fn into_response_json_body_carries_the_bare_message_not_the_prefixed_display() {
        // End-to-end through `IntoResponse`: the JSON body actually sent to
        // an HTTP client must have the bare message, not
        // `Error::to_string()`'s "invalid config: "-prefixed form — this is
        // what `cli::daemon_client::decode_daemon_error` reads before
        // re-adding the prefix itself via `Error::from_code`.
        let err = Error::InvalidConfig {
            message: "unconfigured embedder provider".into(),
        };
        let response = ApiError(err).into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["code"], "invalid_config");
        assert_eq!(body["message"], "unconfigured embedder provider");
    }

    #[test]
    fn internal_maps_to_500() {
        assert_eq!(
            http_status_for(&Error::Internal {
                message: "bug".into(),
                correlation_id: "abc".into(),
            }),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
