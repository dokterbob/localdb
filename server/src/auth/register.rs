//! `POST /register` — OAuth2 Dynamic Client Registration (RFC 7591, T7,
//! specs/05-surfaces.md §3.1).
//!
//! HTTP shape only (specs/01-architecture.md §1): validation and persistence
//! policy live in `localdb_core::auth::AuthService::register_client`
//! (redirect_uri policy: `core::auth::validate_registration_redirect_uri`).
//! This module parses/renders JSON and maps `core::Error` onto the shared
//! error taxonomy. Public client registration only — no `client_secret` is
//! ever minted (mirrors the built-in `localdb-cli` client's own policy);
//! `token_endpoint_auth_method` is always `"none"` in the response and, if
//! the caller sends a different value, the request is rejected.
//!
//! This route is deliberately **public** (no bearer token) — DCR is part of
//! the zero-config onboarding flow (401 → discovery → DCR → code+PKCE), the
//! same reasoning that keeps `/authorize`/`/token`/`/revoke` public (T4).

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use localdb_core::auth::{
    registration_client_name_within_bounds, registration_redirect_uris_within_bounds,
    validate_registration_redirect_uri, MAX_REGISTRATION_CLIENT_NAME_LEN,
    MAX_REGISTRATION_REDIRECT_URIS, MAX_REGISTRATION_REDIRECT_URI_LEN,
};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct RegisterClientRequest {
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
    #[serde(default)]
    pub response_types: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
}

/// `201` response (RFC 7591 §3.2.1): no `client_secret` — this is a public
/// client, matching the built-in `localdb-cli` client's own policy.
#[derive(Debug, Serialize)]
pub struct RegisterClientResponse {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth_method: &'static str,
}

/// RFC 7591 §3.2.2 client-registration error body: every `POST /register`
/// validation failure is `400` with an `error` member (mirrors `POST
/// /token`'s `TokenErrorBody`/`token_error` in `server/src/auth/oauth.rs`,
/// RFC 6749 §5.2's analogous shape) — finding #10. No `code`/`message`
/// `ApiError` envelope on this route.
#[derive(Debug, Serialize)]
struct RegisterErrorBody {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_description: Option<String>,
}

/// Every `POST /register` validation failure is `400` per RFC 7591 §3.2.2
/// (there is no registered-client identity yet to return a different status
/// for, unlike `/token`'s `401 invalid_client`).
fn register_error(code: &'static str, description: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(RegisterErrorBody {
            error: code,
            error_description: Some(description.into()),
        }),
    )
        .into_response()
}

fn default_grant_types() -> Vec<String> {
    vec![
        "authorization_code".to_string(),
        "refresh_token".to_string(),
    ]
}

fn default_response_types() -> Vec<String> {
    vec!["code".to_string()]
}

/// `POST /register` (public, RFC 7591): register a new public OAuth2 client.
///
/// Validates every `redirect_uris` entry (exact `https://` or loopback
/// `http://127.0.0.1[:port]/...` / `http://localhost[:port]/...` — see
/// `core::auth::validate_registration_redirect_uri`'s doc comment for why
/// custom URI schemes are rejected), rejects an explicit
/// `token_endpoint_auth_method` other than `"none"` (this server only ever
/// registers public clients — there is nowhere to store or verify a
/// `client_secret`), enforces the DCR size bounds below (finding #8), and
/// persists the result via `AuthService::register_client`. Every rejection
/// is RFC 7591 §3.2.2-shaped (finding #10, `register_error`) rather than the
/// shared `{"code":...,"message":...}` `ApiError` envelope used elsewhere.
///
/// **DCR bounds (finding #8):** this route is public and unauthenticated, so
/// a single request must not be able to grow `oauth_clients` metadata
/// without limit. At most [`MAX_REGISTRATION_REDIRECT_URIS`] `redirect_uris`
/// entries, each at most [`MAX_REGISTRATION_REDIRECT_URI_LEN`] characters;
/// `client_name`, if present, at most [`MAX_REGISTRATION_CLIENT_NAME_LEN`]
/// characters. These are per-request size caps, not a rate limiter — see the
/// `TODO` below.
pub async fn post_register(
    State(state): State<AppState>,
    Json(req): Json<RegisterClientRequest>,
) -> Response {
    if let Some(method) = req.token_endpoint_auth_method.as_deref() {
        if method != "none" {
            return register_error(
                "invalid_client_metadata",
                format!(
                    "token_endpoint_auth_method '{method}' is not supported; this server only \
                     registers public clients (token_endpoint_auth_method: 'none')"
                ),
            );
        }
    }
    if req.redirect_uris.is_empty() {
        return register_error(
            "invalid_client_metadata",
            "redirect_uris is required and must not be empty",
        );
    }

    // TODO(#8 follow-up): these are per-request payload-size caps, not a
    // rate limiter — a global registration-count cap / throttle on POST
    // /register (this route is public/unauthenticated) is a separate,
    // out-of-scope concern. Track it if T7's DCR endpoint sees abuse.
    if !registration_redirect_uris_within_bounds(&req.redirect_uris) {
        return register_error(
            "invalid_client_metadata",
            format!(
                "redirect_uris must contain at most {MAX_REGISTRATION_REDIRECT_URIS} entries, \
                 each at most {MAX_REGISTRATION_REDIRECT_URI_LEN} characters"
            ),
        );
    }
    if !registration_client_name_within_bounds(req.client_name.as_deref()) {
        return register_error(
            "invalid_client_metadata",
            format!("client_name must be at most {MAX_REGISTRATION_CLIENT_NAME_LEN} characters"),
        );
    }

    for uri in &req.redirect_uris {
        if !validate_registration_redirect_uri(uri) {
            return register_error(
                "invalid_redirect_uri",
                format!(
                    "redirect_uri '{uri}' is not allowed: must be an https:// URL or a loopback \
                     http://127.0.0.1[:port]/... or http://localhost[:port]/... URL"
                ),
            );
        }
    }

    let row = match state
        .auth()
        .register_client(req.redirect_uris, req.client_name)
        .await
    {
        Ok(row) => row,
        // Every check above mirrors `AuthService::register_client`'s own
        // validation, so reaching here means either a race (vanishingly
        // unlikely for pure, stateless checks) or a store-layer failure;
        // either way it is client-metadata-shaped from the caller's point of
        // view, not a distinct error class worth its own RFC 7591 code.
        Err(e) => return register_error("invalid_client_metadata", e.to_string()),
    };

    let grant_types = req.grant_types.unwrap_or_else(default_grant_types);
    let response_types = req.response_types.unwrap_or_else(default_response_types);

    (
        StatusCode::CREATED,
        Json(RegisterClientResponse {
            client_id: row.id,
            client_name: row.client_name,
            redirect_uris: row.redirect_uris,
            grant_types,
            response_types,
            token_endpoint_auth_method: "none",
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_grant_and_response_types_match_rfc7591_defaults() {
        assert_eq!(
            default_grant_types(),
            vec![
                "authorization_code".to_string(),
                "refresh_token".to_string()
            ]
        );
        assert_eq!(default_response_types(), vec!["code".to_string()]);
    }
}
