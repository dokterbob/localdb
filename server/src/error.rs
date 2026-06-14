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
            message: self.0.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

/// Map a `CoreError` to an HTTP status code per specs/05-surfaces.md §5.
pub fn http_status_for(err: &CoreError) -> StatusCode {
    match err {
        CoreError::StoreNotFound { .. }
        | CoreError::SourceNotFound { .. }
        | CoreError::DocumentNotFound { .. }
        | CoreError::JobNotFound { .. } => StatusCode::NOT_FOUND,

        CoreError::StoreLocked
        | CoreError::DaemonRunning
        | CoreError::ConfigReadonly
        | CoreError::IndexInProgress => StatusCode::CONFLICT,

        CoreError::DaemonUnreachable | CoreError::ProviderUnavailable { .. } => {
            StatusCode::BAD_GATEWAY
        }

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
            http_status_for(&Error::DocumentNotFound { id: "x".into() }),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            http_status_for(&Error::JobNotFound { id: "x".into() }),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn conflict_errors_map_to_409() {
        assert_eq!(http_status_for(&Error::StoreLocked), StatusCode::CONFLICT);
        assert_eq!(http_status_for(&Error::DaemonRunning), StatusCode::CONFLICT);
        assert_eq!(
            http_status_for(&Error::ConfigReadonly),
            StatusCode::CONFLICT
        );
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
