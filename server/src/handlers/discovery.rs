//! `GET /.well-known/oauth-protected-resource` (RFC 9728) and
//! `GET /.well-known/oauth-authorization-server` (RFC 8414) — T7,
//! specs/05-surfaces.md §3.1.
//!
//! Both routes are public (no bearer token — they *are* the discovery step
//! of the zero-config onboarding flow: 401 → protected-resource metadata →
//! authorization-server metadata → DCR → code+PKCE). HTTP shape only; the
//! base-URL resolution (public_url vs sanitized Host header) lives in
//! `server::auth::base_url::resolve_base_url`.

use axum::{
    extract::State,
    http::{header, HeaderMap},
    Json,
};

use localdb_core::auth::{SUPPORTED_GRANT_TYPES, SUPPORTED_RESPONSE_TYPES};
use localdb_core::Error as CoreError;

use crate::auth::base_url::resolve_base_url;
use crate::error::ApiError;
use crate::state::AppState;

/// Resolve the request's base URL, mapping an unresolvable Host header (no
/// `server.public_url` configured, and a missing/hostile `Host` header) to
/// `400 invalid_request` rather than ever echoing the raw header back.
fn resolve_base(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let host_header = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    resolve_base_url(state.public_url(), host_header).ok_or_else(|| {
        ApiError(CoreError::InvalidRequest {
            message: "cannot determine this server's base URL: no server.public_url is \
                      configured and the request's Host header is missing or invalid"
                .to_string(),
        })
    })
}

/// `GET /.well-known/oauth-protected-resource` (RFC 9728 §3.1).
pub async fn oauth_protected_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = resolve_base(&state, &headers)?;
    Ok(Json(serde_json::json!({
        "resource": base,
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
    })))
}

/// `GET /.well-known/oauth-authorization-server` (RFC 8414 §3.2).
pub async fn oauth_authorization_server(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = resolve_base(&state, &headers)?;
    Ok(Json(serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "revocation_endpoint": format!("{base}/revoke"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": SUPPORTED_RESPONSE_TYPES,
        "grant_types_supported": SUPPORTED_GRANT_TYPES,
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
    })))
}
