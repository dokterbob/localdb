//! Per-client-IP token-bucket rate limiting for the daemon's public,
//! unauthenticated auth routes (specs/05-surfaces.md §3.1): `/authorize`,
//! `/join`, `/token`, `/revoke`, `/register`, `/v1/invites/redeem`, and
//! `/v1/invites/requests/{id}/poll`. The `/.well-known/*` discovery
//! documents are never limited — see `daemon::build_router`'s doc comment.
//!
//! Mirrors `fetch::http::limiter::HostLimiter`'s governor-based keyed
//! limiter — same token-bucket construction, same periodic
//! `retain_recent`/`shrink_to_fit` housekeeping — keyed on the client's peer
//! IP instead of a destination host, and implemented independently here so
//! `server` does not pick up `fetch`'s outgoing-HTTP machinery (retry,
//! destination guards, …) just to reuse one struct.
//!
//! Unlike outbound pacing, inbound rate limiting must never make a request
//! wait: a blocked HTTP connection holding a client open would itself be a
//! resource a hostile caller could exhaust. So this checks the bucket
//! synchronously (`governor::RateLimiter::check_key`) and immediately
//! rejects with `429` plus `Retry-After` on failure, rather than sleeping.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter as GovernorRateLimiter};
use serde::Serialize;

use localdb_core::config::schema::AbuseLimitsConfig;

use crate::error::ErrorResponse;
use crate::state::AppState;

/// Governor's own keyed limiter, spelled out in full (mirrors
/// `fetch::http::limiter`'s `GovernorLimiter` type alias).
type GovernorLimiter = GovernorRateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// How many [`AbuseLimiter::check`] calls between housekeeping passes — see
/// `fetch::http::limiter::PRUNE_EVERY_N_ACQUIRES`'s doc comment for why a
/// periodic sweep (rather than one on every call) is the right trade-off.
const PRUNE_EVERY_N_CHECKS: u64 = 100;

fn nonzero_or(value: u32, fallback: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap_or_else(|| {
        NonZeroU32::new(fallback).expect("fallback for nonzero_or must itself be nonzero")
    })
}

struct Inner {
    limiter: GovernorLimiter,
    check_count: AtomicU64,
}

/// A per-client-IP token bucket, or a no-op when `abuse_limits.enable` is
/// `false`. `Arc`-backed and cheaply `Clone`, built once per `AppState` and
/// shared across every request.
#[derive(Clone)]
pub struct AbuseLimiter {
    inner: Option<Arc<Inner>>,
}

impl AbuseLimiter {
    /// Build from config. `enable: false` yields a limiter that always
    /// allows — the middleware stays wired, just inert.
    pub fn new(cfg: &AbuseLimitsConfig) -> Self {
        if !cfg.enable {
            return Self { inner: None };
        }
        let quota = Quota::per_second(nonzero_or(cfg.requests_per_second, 1))
            .allow_burst(nonzero_or(cfg.burst, 1));
        let limiter: GovernorLimiter = GovernorRateLimiter::new(
            quota,
            DefaultKeyedStateStore::default(),
            DefaultClock::default(),
        );
        Self {
            inner: Some(Arc::new(Inner {
                limiter,
                check_count: AtomicU64::new(0),
            })),
        }
    }

    /// A limiter that always allows — used where no `AbuseLimitsConfig` is
    /// in scope (tests that don't exercise this behavior).
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Check one request from `ip`. `Ok(())` means allowed and consumed a
    /// token; `Err(wait)` means rejected, with `wait` the time until a token
    /// would next be available.
    fn check(&self, ip: IpAddr) -> Result<(), Duration> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let n = inner.check_count.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(PRUNE_EVERY_N_CHECKS) {
            inner.limiter.retain_recent();
            inner.limiter.shrink_to_fit();
        }
        inner
            .limiter
            .check_key(&ip)
            .map_err(|not_until| not_until.wait_time_from(DefaultClock::default().now()))
    }
}

/// RFC 6749 §5.2 / RFC 8628 §3.5-style error body, matching the shape
/// `server::auth::oauth`'s `TokenErrorBody` and `server::auth::register`'s
/// `RegisterErrorBody` already use for this pair of routes.
#[derive(Debug, Serialize)]
struct OAuthStyleErrorBody {
    error: &'static str,
    error_description: String,
}

fn too_many_requests(path: &str, wait: Duration) -> Response {
    // Round up: a caller that retries at exactly the reported delay must
    // never arrive a token early.
    let retry_after_secs = wait.as_secs_f64().ceil().max(1.0) as u64;

    let mut response = match path {
        // RFC 6749 §5.2 defines no token-endpoint code for "too many
        // requests"; `slow_down` is RFC 8628 §3.5's registered code for
        // exactly this ("the client is polling too quickly"), the closest
        // fit on the books.
        "/token" => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OAuthStyleErrorBody {
                error: "slow_down",
                error_description: "too many requests; retry after the delay in Retry-After"
                    .to_string(),
            }),
        )
            .into_response(),
        // Consistent with `register.rs`'s own cap-reached 429 on this same
        // route: both describe "this server can't accept this registration
        // attempt right now".
        "/register" => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OAuthStyleErrorBody {
                error: "temporarily_unavailable",
                error_description: "too many registration attempts; retry after the delay in \
                                     Retry-After"
                    .to_string(),
            }),
        )
            .into_response(),
        _ => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                code: "rate_limited".to_string(),
                message: "too many requests from this address; retry after the delay in \
                          Retry-After"
                    .to_string(),
            }),
        )
            .into_response(),
    };
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    response
}

/// Middleware applied to the public auth-route group in
/// `daemon::build_router`. Keys on the TCP peer address
/// (`ConnectInfo<SocketAddr>`, populated by
/// `into_make_service_with_connect_info`), never `X-Forwarded-For`/
/// `Forwarded` (untrusted without a configured, trusted reverse proxy, which
/// this daemon does not yet model — behind a plain reverse proxy every
/// client shares the proxy's address). Loopback peers are always exempt,
/// regardless of `abuse_limits.enable` — a local daemon must never throttle
/// its own user. A request with no `ConnectInfo` at all (a test harness
/// invoking a handler directly, bypassing the real TCP listener) is also
/// never limited: there is nothing reliable to key on, the same "don't
/// limit what you can't identify" rule a non-TCP transport would get if the
/// daemon ever served `/v1` over one — today it does not (the discovery
/// socket, `server::socket::SocketGuard`, is a liveness probe only, never
/// an HTTP transport).
pub async fn limit_abuse(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // Read `ConnectInfo` out of the request's own extensions rather than
    // taking it as a separate typed extractor parameter: axum's generated
    // `from_fn` impls are keyed by exact parameter arity, and this keeps
    // this middleware's signature identical in shape to
    // `auth::middleware::require_auth`'s (`State`, `Request`, `Next`),
    // known to resolve cleanly, rather than introducing a second
    // `FromRequestParts` extractor whose arity this crate's axum version
    // does not resolve here.
    let connect_info = req.extensions().get::<ConnectInfo<SocketAddr>>().copied();
    let Some(ConnectInfo(addr)) = connect_info else {
        return next.run(req).await;
    };
    if addr.ip().is_loopback() {
        return next.run(req).await;
    }
    match state.abuse_limiter().check(addr.ip()) {
        Ok(()) => next.run(req).await,
        Err(wait) => too_many_requests(req.uri().path(), wait),
    }
}

#[cfg(test)]
mod tests;
