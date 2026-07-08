//! `require_auth`: the axum middleware enforcing bearer auth on every
//! protected route (specs/05-surfaces.md §3.1).
//!
//! Applied in `daemon::build_router` *after* the `/mcp` `nest_service`, so
//! it covers all of `/v1/*` and `/mcp`. None of the public routes from the
//! spec §3.1 table (`/.well-known/*`, `/authorize`, `/token`, ...) exist yet
//! (T4/T6/T7), so in T3 every registered route is protected.
//!
//! Behavior by mode:
//! - [`AuthMode::Open`]: inserts `Principal::local_trust()` into the request
//!   extensions and passes through — same trust boundary as daemonless use.
//! - [`AuthMode::Enforced`]: extracts `Authorization: Bearer <secret>`,
//!   resolves it via `AuthService::authenticate`, and inserts the resulting
//!   `Principal`. Missing/invalid credentials → 401 with the standard error
//!   envelope plus `WWW-Authenticate: Bearer` (D6, added by
//!   `ApiError::into_response`), upgraded here to carry
//!   `resource_metadata="<base>/.well-known/oauth-protected-resource"` (T7,
//!   RFC 9728 §5.1) — the trigger a stock MCP client uses to discover this
//!   daemon's AS and onboard with zero static config. `ApiError` itself has
//!   no access to the request's `Host` header or `AppState`, so this
//!   middleware post-processes the 401 response it already built rather than
//!   restructuring `ApiError` to carry a base URL — see
//!   `add_resource_metadata_challenge` below.
//!
//! T5 lifts the T3 interim policy that rejected every authenticated
//! `member` principal wholesale: this middleware now inserts *any*
//! authenticated principal — admin or member — and authorization is
//! enforced per-resource by the individual handlers instead (D7: members
//! read only the `shared` stores they hold a grant for via
//! `Principal::can_read_store`; admin-only management routes call
//! `Principal::require_admin`). See specs/05-surfaces.md §3.1 for the route
//! table.
//!
//! The inserted `Principal` is what `/v1/auth/me` reads back, and what rmcp
//! propagates into MCP tool handlers via `http::request::Parts` extensions
//! (see `mcp::handler::McpHandler`).

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use localdb_core::{auth::Principal, Error};

use crate::{error::ApiError, state::AppState};

use super::{base_url::resolve_base_url, AuthMode};

/// Extract the bearer secret from an `Authorization` header value.
///
/// Scheme matching is case-insensitive per RFC 7235; surrounding whitespace
/// on the token is trimmed. Returns `None` for a missing header, a
/// non-Bearer scheme, or an empty token.
fn bearer_secret(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// The auth middleware. See the module doc for the full behavior matrix.
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    match state.auth_mode() {
        AuthMode::Open => {
            req.extensions_mut().insert(Principal::local_trust());
            next.run(req).await
        }
        AuthMode::Enforced => {
            let host_header = req
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let Some(secret) = bearer_secret(req.headers()) else {
                let mut response = ApiError(Error::Unauthorized {
                    message: "missing bearer token".to_string(),
                })
                .into_response();
                add_resource_metadata_challenge(&mut response, &state, host_header.as_deref());
                return response;
            };
            let principal = match state.auth().authenticate(secret).await {
                Ok(p) => p,
                Err(e) => {
                    let mut response = ApiError(e).into_response();
                    add_resource_metadata_challenge(&mut response, &state, host_header.as_deref());
                    return response;
                }
            };
            // T5: no wholesale role gate here — every authenticated
            // principal passes; per-resource authorization (store-grant
            // scoping, admin-only management routes) is enforced by the
            // handlers themselves.
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
    }
}

/// Upgrade a 401 response's `WWW-Authenticate: Bearer` header (set by
/// `ApiError::into_response`, D6) to carry the RFC 9728 §5.1
/// `resource_metadata` parameter, iff a base URL can be resolved
/// (`server::auth::base_url::resolve_base_url` — prefers `server.public_url`,
/// falls back to a sanitized `Host` header). If no base URL can be resolved
/// (no `public_url` configured and a missing/hostile `Host` header), the
/// response is left with the plain `Bearer` challenge `ApiError` already
/// set — failing open on the *discovery hint* only, never on the 401 itself.
fn add_resource_metadata_challenge(
    response: &mut Response,
    state: &AppState,
    host_header: Option<&str>,
) {
    if response.status() != StatusCode::UNAUTHORIZED {
        return;
    }
    let Some(base) = resolve_base_url(state.public_url(), host_header) else {
        return;
    };
    let value = format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\"");
    if let Ok(header_value) = HeaderValue::from_str(&value) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, header_value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn bearer_secret_extracts_token() {
        let headers = headers_with_auth("Bearer ldb_abc123");
        assert_eq!(bearer_secret(&headers), Some("ldb_abc123"));
    }

    #[test]
    fn bearer_secret_scheme_is_case_insensitive() {
        let headers = headers_with_auth("bearer ldb_abc123");
        assert_eq!(bearer_secret(&headers), Some("ldb_abc123"));
    }

    #[test]
    fn bearer_secret_rejects_other_schemes() {
        let headers = headers_with_auth("Basic dXNlcjpwdw==");
        assert_eq!(bearer_secret(&headers), None);
    }

    #[test]
    fn bearer_secret_rejects_missing_header() {
        assert_eq!(bearer_secret(&HeaderMap::new()), None);
    }

    #[test]
    fn bearer_secret_rejects_empty_token() {
        let headers = headers_with_auth("Bearer ");
        assert_eq!(bearer_secret(&headers), None);
    }

    #[test]
    fn bearer_secret_trims_whitespace() {
        let headers = headers_with_auth("Bearer   ldb_abc123  ");
        assert_eq!(bearer_secret(&headers), Some("ldb_abc123"));
    }
}
