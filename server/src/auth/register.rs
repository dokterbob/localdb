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
    MAX_REGISTRATION_REDIRECT_URIS, MAX_REGISTRATION_REDIRECT_URI_LEN, SUPPORTED_GRANT_TYPES,
    SUPPORTED_RESPONSE_TYPES,
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

/// Finding #4: a generic, detail-free `500` for a store-layer (internal
/// -class) `register_client` failure — RFC 7591 has nothing to say about
/// server-side faults, so this is RFC-neutral (mirrors `oauth.rs`'s
/// `internal_token_error()`, RFC 6749 §5.2's analogous `server_error` shape).
/// No `e.to_string()` here: the whole point is that raw storage detail must
/// never reach this public, unauthenticated route.
fn internal_register_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(RegisterErrorBody {
            error: "server_error",
            error_description: Some("internal error".to_string()),
        }),
    )
        .into_response()
}

fn default_grant_types() -> Vec<String> {
    SUPPORTED_GRANT_TYPES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_response_types() -> Vec<String> {
    SUPPORTED_RESPONSE_TYPES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Finding #3: a `grant_types`/`response_types` entry this AS doesn't
/// actually support (e.g. `client_credentials`, `token`) must not be echoed
/// back in a `201` — that would advertise a flow `/token`/`/authorize` can't
/// service. `supported` is [`SUPPORTED_GRANT_TYPES`] or
/// [`SUPPORTED_RESPONSE_TYPES`] — the same list `discovery::
/// oauth_authorization_server` advertises as `grant_types_supported`/
/// `response_types_supported`, so DCR and discovery can never disagree.
fn unsupported_entry<'a>(requested: &'a [String], supported: &[&str]) -> Option<&'a str> {
    requested
        .iter()
        .find(|entry| !supported.contains(&entry.as_str()))
        .map(|s| s.as_str())
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

    // Finding #3: a registrant asking for a `grant_types`/`response_types`
    // entry this AS doesn't actually support (e.g. `client_credentials`,
    // `token`) must be rejected rather than echoed back in the `201` — that
    // response is otherwise the only place a client learns which flows are
    // supported, and an unsupported entry there is a lie `/token`/`/authorize`
    // can't back up. `SUPPORTED_GRANT_TYPES`/`SUPPORTED_RESPONSE_TYPES` are
    // the same lists `discovery::oauth_authorization_server` advertises as
    // `grant_types_supported`/`response_types_supported` (single source of
    // truth in `core::auth::client`). Absent fields keep the current
    // defaults, applied below.
    if let Some(grant_types) = &req.grant_types {
        if let Some(bad) = unsupported_entry(grant_types, SUPPORTED_GRANT_TYPES) {
            return register_error(
                "invalid_client_metadata",
                format!(
                    "grant_types entry '{bad}' is not supported; this server supports \
                     {SUPPORTED_GRANT_TYPES:?}"
                ),
            );
        }
    }
    if let Some(response_types) = &req.response_types {
        if let Some(bad) = unsupported_entry(response_types, SUPPORTED_RESPONSE_TYPES) {
            return register_error(
                "invalid_client_metadata",
                format!(
                    "response_types entry '{bad}' is not supported; this server supports \
                     {SUPPORTED_RESPONSE_TYPES:?}"
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
        // Finding #4: every check above mirrors `AuthService::register_client`'s
        // own pure, stateless validation, so reaching here with a client-input
        // -shaped error is vanishingly unlikely (at most a race). What's
        // actually likely is a store-layer failure (write-lock contention, a
        // dead connection) — an internal-class fault (`is_internal_class_error`,
        // shared with `oauth.rs`'s token endpoint) must not be misreported as
        // `400 invalid_client_metadata` with the raw storage error echoed back
        // via `e.to_string()`; it becomes a generic, detail-free `500`
        // instead. A genuine (non-internal) failure keeps the prior behavior:
        // 400 invalid_client_metadata with its message, which carries no
        // sensitive detail since it can only come from the validators above.
        Err(e) if crate::auth::is_internal_class_error(&e) => return internal_register_error(),
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

    // -----------------------------------------------------------------
    // Finding #3: unsupported grant_types/response_types entries.
    // -----------------------------------------------------------------

    #[test]
    fn unsupported_entry_flags_a_disallowed_grant_type() {
        let requested = vec!["client_credentials".to_string()];
        assert_eq!(
            unsupported_entry(&requested, localdb_core::auth::SUPPORTED_GRANT_TYPES),
            Some("client_credentials")
        );
    }

    #[test]
    fn unsupported_entry_allows_every_supported_grant_type() {
        let requested = vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ];
        assert_eq!(
            unsupported_entry(&requested, localdb_core::auth::SUPPORTED_GRANT_TYPES),
            None
        );
    }

    #[test]
    fn unsupported_entry_flags_a_disallowed_response_type() {
        let requested = vec!["token".to_string()];
        assert_eq!(
            unsupported_entry(&requested, localdb_core::auth::SUPPORTED_RESPONSE_TYPES),
            Some("token")
        );
    }

    // -----------------------------------------------------------------
    // Finding #4: a store-layer (internal-class) `register_client` failure
    // must become a generic, detail-free 500 (`internal_register_error`),
    // never a `400 invalid_client_metadata` echoing `e.to_string()`.
    // `LibsqlAuthStore` has no seam to make `create_oauth_client` itself
    // return a store-layer error, so — mirroring the same limitation noted
    // for finding #1 in `oauth.rs` — this is covered at the
    // response-shaping level rather than end-to-end through a real store
    // fault; the shared classification predicate itself
    // (`crate::auth::is_internal_class_error`) is exercised by `oauth.rs`'s
    // own `internal_class_errors_are_classified_internal`/
    // `client_input_errors_are_not_classified_internal` tests, which cover
    // this call site too since the predicate is the same one.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn internal_register_error_is_a_generic_500_with_no_leaked_detail() {
        let resp = internal_register_error();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "server_error");
        assert_eq!(body["error_description"], "internal error");
    }
}
