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

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use localdb_core::Error as CoreError;

use crate::error::ApiError;
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
/// `client_secret`), and persists the result via
/// `AuthService::register_client`.
pub async fn post_register(
    State(state): State<AppState>,
    Json(req): Json<RegisterClientRequest>,
) -> Result<axum::response::Response, ApiError> {
    if let Some(method) = req.token_endpoint_auth_method.as_deref() {
        if method != "none" {
            return Err(ApiError(CoreError::InvalidRequest {
                message: format!(
                    "token_endpoint_auth_method '{method}' is not supported; this server only \
                     registers public clients (token_endpoint_auth_method: 'none')"
                ),
            }));
        }
    }
    if req.redirect_uris.is_empty() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: "redirect_uris is required and must not be empty".to_string(),
        }));
    }

    let row = state
        .auth()
        .register_client(req.redirect_uris, req.client_name)
        .await?;

    let grant_types = req.grant_types.unwrap_or_else(default_grant_types);
    let response_types = req.response_types.unwrap_or_else(default_response_types);

    Ok((
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
        .into_response())
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
