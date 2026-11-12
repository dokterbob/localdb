//! `POST /register` — OAuth2 Dynamic Client Registration (RFC 7591,
//! specs/05-surfaces.md §3.1).
//!
//! HTTP shape only (specs/01-architecture.md §1): every validation rule —
//! `redirect_uris`/`client_name` bounds and shape, `token_endpoint_auth_method`,
//! `grant_types`/`response_types` — lives in the single
//! `core::auth::validate_registration_request`, enforced by
//! `AuthService::register_client`. This module only parses/renders JSON and
//! maps `RegisterClientError` onto the matching RFC 7591 §3.2.2 error code.
//! Public client registration only — no `client_secret` is ever minted
//! (mirrors the built-in `localdb-cli` client's own policy);
//! `token_endpoint_auth_method` is always `"none"` in the response and, if
//! the caller sends a different value, the request is rejected.
//!
//! This route is deliberately **public** (no bearer token) — DCR is part of
//! the zero-config onboarding flow (401 → discovery → DCR → code+PKCE), the
//! same reasoning that keeps `/authorize`/`/token`/`/revoke` public.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use localdb_core::auth::{
    RegisterClientError, RegistrationError, RegistrationPolicy, SUPPORTED_GRANT_TYPES,
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
/// RFC 6749 §5.2's analogous shape). No `code`/`message`
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

/// A generic, detail-free `500` for a store-layer (internal
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

/// Seconds suggested in `Retry-After` when the registration cap
/// (`RegistrationPolicy::max_dynamic_clients`) is still reached after
/// pruning. Fixed, independent of `server.abuse_limits.unused_client_ttl_secs`
/// (which defaults to a full day): the TTL answers "when does an unused
/// registration become prunable", not "when is a slot likely to actually
/// free up" — a cap held by clients that *are* in active use won't free up
/// on any schedule this server can predict, so suggesting the TTL itself
/// would tell a caller to wait a day for no better reason than this number
/// being convenient to compute.
const REGISTRATION_CAP_RETRY_AFTER_SECS: u64 = 3600;

/// `429` (RFC 7591 has no error code for "capacity reached"; `temporarily_unavailable`
/// is RFC 6749 §4.1.2.1's closest-fitting registered code, reused here the
/// same way `oauth.rs` reuses OAuth2 codes for conditions the exact RFC
/// section never anticipated) plus `Retry-After`.
fn registration_cap_reached_error() -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(RegisterErrorBody {
            error: "temporarily_unavailable",
            error_description: Some(
                "the server is not accepting new client registrations right now; retry later"
                    .to_string(),
            ),
        }),
    )
        .into_response();
    if let Ok(value) =
        axum::http::HeaderValue::from_str(&REGISTRATION_CAP_RETRY_AFTER_SECS.to_string())
    {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    response
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

/// RFC 7591 §3.2.2 error code for a [`RegistrationError`] — `invalid_redirect_uri`
/// for a bad `redirect_uris` entry, `invalid_client_metadata` for every other
/// violation (unsupported auth method, emptiness, bounds, client_name
/// length, an unsupported `grant_types`/`response_types` entry).
fn registration_error_code(err: &RegistrationError) -> &'static str {
    match err {
        RegistrationError::InvalidRedirectUri(_) => "invalid_redirect_uri",
        RegistrationError::UnsupportedAuthMethod(_)
        | RegistrationError::EmptyRedirectUris
        | RegistrationError::TooManyOrOverlongRedirectUris
        | RegistrationError::ClientNameTooLong
        | RegistrationError::UnsupportedGrantType(_)
        | RegistrationError::UnsupportedResponseType(_) => "invalid_client_metadata",
    }
}

/// `POST /register` (public, RFC 7591): register a new public OAuth2 client.
///
/// Performs no validation of its own: every check — `token_endpoint_auth_method`
/// must be `"none"` if present, then `redirect_uris` non-empty, then within
/// the DCR size bounds, then `client_name` within its length bound, then
/// each `redirect_uris` entry an allowed shape, and finally (if present)
/// every `grant_types`/`response_types` entry supported — is
/// `AuthService::register_client`'s own `validate_registration_request`,
/// enforced in that exact precedence order; this handler only maps its
/// [`RegistrationError`] onto the matching RFC 7591 §3.2.2 code
/// (`registration_error_code`). Every rejection is RFC 7591 §3.2.2-shaped
/// (`register_error`) rather than the shared `{"code":...,"message":...}`
/// `ApiError` envelope used elsewhere.
///
/// Also enforces `server.abuse_limits`' registration cap and unused-client
/// TTL (`AuthService::register_client`'s `policy` argument) — see
/// [`registration_cap_reached_error`] for the `429` this produces once the
/// cap is still reached after pruning.
pub async fn post_register(
    State(state): State<AppState>,
    Json(req): Json<RegisterClientRequest>,
) -> Response {
    let limits = &state.yaml_config().server.abuse_limits;
    // `enable: false` is the master switch for every limit in this section
    // (its own doc comment) — registration's cap/TTL/eviction included, not
    // just the rate limiter.
    let policy = if limits.enable {
        RegistrationPolicy {
            max_dynamic_clients: limits.max_dynamic_clients,
            unused_client_ttl_secs: limits.unused_client_ttl_secs,
        }
    } else {
        RegistrationPolicy::unlimited()
    };
    let row = match state
        .auth()
        .register_client(
            req.redirect_uris,
            req.client_name,
            req.token_endpoint_auth_method.as_deref(),
            req.grant_types.as_deref(),
            req.response_types.as_deref(),
            &policy,
        )
        .await
    {
        Ok(row) => row,
        Err(RegisterClientError::Validation(e)) => {
            return register_error(registration_error_code(&e), e.to_string())
        }
        // A store-layer failure (write-lock contention, a dead connection) —
        // an internal-class fault (`is_internal_class_error`, shared with
        // `oauth.rs`'s token endpoint) must not be misreported as `400
        // invalid_client_metadata` with the raw storage error echoed back
        // via `e.to_string()`; it becomes a generic, detail-free `500`
        // instead.
        Err(RegisterClientError::Store(e)) if crate::auth::is_internal_class_error(&e) => {
            return internal_register_error()
        }
        Err(RegisterClientError::Store(e)) => {
            return register_error("invalid_client_metadata", e.to_string())
        }
        Err(RegisterClientError::CapReached) => return registration_cap_reached_error(),
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
mod tests;
