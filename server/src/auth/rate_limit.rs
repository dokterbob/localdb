//! Per-client-address token-bucket rate limiting for the daemon's public,
//! unauthenticated auth routes (specs/05-surfaces.md §3.1): `/authorize`,
//! `/join`, `/token`, `/revoke`, `/register`, `/v1/invites/redeem`, and
//! `/v1/invites/requests/{id}/poll`. The `/.well-known/*` discovery
//! documents are never limited — see `daemon::build_router`'s doc comment.
//!
//! Mirrors `fetch::http::limiter::HostLimiter`'s governor-based keyed
//! limiter — same token-bucket construction, same periodic
//! `retain_recent`/`shrink_to_fit` housekeeping, see
//! [`PRUNE_EVERY_N_CHECKS`] — keyed on the client's address instead of a
//! destination host, and implemented independently here so `server` does
//! not pick up `fetch`'s outgoing-HTTP machinery (retry, destination
//! guards, …) just to reuse one struct.
//!
//! Unlike outbound pacing, inbound rate limiting must never make a request
//! wait: a blocked HTTP connection holding a client open would itself be a
//! resource a hostile caller could exhaust. So this checks the bucket
//! synchronously (`governor::RateLimiter::check_key`) and immediately
//! rejects with `429` plus `Retry-After` on failure, rather than sleeping.
//!
//! The client address a request is limited/exempted on is resolved by
//! [`trusted_proxy::resolve_client_addr`] — the TCP peer address, unless the
//! peer is a configured `server.abuse_limits.trusted_proxies` entry, in
//! which case `X-Forwarded-For` (walked right to left) names the real
//! client. [`trusted_proxy::limiter_key`] then reduces that address to its
//! bucket key — unchanged for IPv4, masked to its /64 network prefix for a
//! genuine IPv6 address (specs/05-surfaces.md §3.1).
//!
//! The loopback exemption applies to exactly one of
//! [`trusted_proxy::ClientAddr`]'s three cases — a *direct* TCP peer that is
//! not itself a configured trusted proxy. An address `X-Forwarded-For`
//! named is never exempt, whatever it is, because nothing authenticates a
//! header's content; a trusted proxy whose header resolves to nothing
//! usable is limited under its own address instead of being treated as
//! exempt or as an unidentifiable caller. See `trusted_proxy::
//! resolve_client_addr`'s doc comment for the exact walk and why a
//! malformed hop ends resolution rather than being skipped.

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

use localdb_core::config::schema::{parse_ip_cidr, AbuseLimitsConfig};

use super::trusted_proxy::{self, ClientAddr};
use crate::error::ErrorResponse;
use crate::state::AppState;

/// Governor's own keyed limiter, spelled out in full (mirrors
/// `fetch::http::limiter`'s `GovernorLimiter` type alias).
type GovernorLimiter = GovernorRateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// How many [`AbuseLimiter::check`] calls between housekeeping passes — see
/// `fetch::http::limiter::PRUNE_EVERY_N_ACQUIRES`'s doc comment for why a
/// periodic inline sweep (rather than a separate background task, and
/// rather than one on every call) is the right trade-off. This bounds the
/// limiter's keyed state to roughly the number of distinct client buckets
/// seen in the last `PRUNE_EVERY_N_CHECKS` requests, not to any fixed
/// ceiling — a daemon seeing a wide, sustained spread of distinct
/// addresses keeps a bucket per address (per /64, for IPv6) between
/// sweeps. In practice this is bounded by how many distinct callers a
/// public auth surface sees concurrently, which is small relative to, say,
/// indexed chunk counts; a daemon under a sustained many-address flood
/// should lower `requests_per_second`/`burst` rather than rely on this
/// sweep for memory control.
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

/// A per-client-address token bucket, or a no-op when `abuse_limits.enable` is
/// `false`. `Arc`-backed and cheaply `Clone`, built once per `AppState` and
/// shared across every request.
#[derive(Clone)]
pub struct AbuseLimiter {
    inner: Option<Arc<Inner>>,
    /// Parsed `server.abuse_limits.trusted_proxies`. Kept outside `inner`
    /// so trusted-proxy resolution is available even when the limiter
    /// itself is disabled — resolving the address is cheap and keeps
    /// `limit_abuse`'s logic uniform regardless of `enable`, even though a
    /// disabled limiter's `check` always allows either way.
    trusted_proxies: Arc<Vec<(IpAddr, u8)>>,
}

impl AbuseLimiter {
    /// Build from config. `enable: false` yields a limiter that always
    /// allows — the middleware stays wired, just inert. `trusted_proxies`
    /// entries are assumed already validated (`core::config::loader::
    /// validate_config` runs `parse_ip_cidr` on every entry at config load
    /// time), so a parse failure here is a bug, not a user-facing error.
    pub fn new(cfg: &AbuseLimitsConfig) -> Self {
        let trusted_proxies = Arc::new(
            cfg.trusted_proxies
                .iter()
                .map(|entry| {
                    parse_ip_cidr(entry).unwrap_or_else(|e| {
                        panic!("server.abuse_limits.trusted_proxies entry '{entry}' should already be validated: {e}")
                    })
                })
                .collect(),
        );
        if !cfg.enable {
            return Self {
                inner: None,
                trusted_proxies,
            };
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
            trusted_proxies,
        }
    }

    /// A limiter that always allows and trusts no proxy — used where no
    /// `AbuseLimitsConfig` is in scope (tests that don't exercise this
    /// behavior).
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            inner: None,
            trusted_proxies: Arc::new(Vec::new()),
        }
    }

    /// A limiter with `burst` capacity and a 5-minute replenish period,
    /// trusting no proxy. Used by tests that make exactly `burst` calls and
    /// assert the next one is rejected: a 5-minute-per-token quota (instead
    /// of `requests_per_second: 1`'s real one-second quota) makes a token
    /// refilling between those calls impossible short of a multi-minute
    /// scheduler stall, so the assertion never depends on how quickly the
    /// two calls actually ran.
    #[cfg(test)]
    pub fn for_test_with_burst(burst: u32) -> Self {
        let quota = Quota::with_period(Duration::from_secs(300))
            .expect("300s is a valid quota period")
            .allow_burst(nonzero_or(burst, 1));
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
            trusted_proxies: Arc::new(Vec::new()),
        }
    }

    /// Resolve the address a request from `peer` carrying `forwarded_for`
    /// (every `X-Forwarded-For` header line present, in order — `None` if
    /// the header was missing or any line of it failed to decode as text)
    /// should be limited/exempted on — see
    /// [`trusted_proxy::resolve_client_addr`].
    fn resolve_client_addr(&self, peer: IpAddr, forwarded_for: Option<&[String]>) -> ClientAddr {
        trusted_proxy::resolve_client_addr(peer, &self.trusted_proxies, forwarded_for)
    }

    /// Check one request keyed on `key` (already resolved and reduced to
    /// its bucket key — see [`trusted_proxy::limiter_key`]). `Ok(())` means
    /// allowed and consumed a token; `Err(wait)` means rejected, with
    /// `wait` the time until a token would next be available.
    fn check(&self, key: IpAddr) -> Result<(), Duration> {
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
            .check_key(&key)
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
        // requests". `slow_down` is tempting (RFC 8628 §3.5's registered
        // code for "polling too quickly") but belongs to the device-flow
        // grant this endpoint never implements (authorization-code and
        // refresh grants only) — emitting it here would be a code this
        // route otherwise never sends. `temporarily_unavailable` (RFC 6749
        // §4.1.2.1) fits a generic "can't serve this request right now"
        // better and matches `/register`'s own 429 below.
        "/token" | "/register" => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(OAuthStyleErrorBody {
                error: "temporarily_unavailable",
                error_description: "too many requests; retry after the delay in Retry-After"
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
/// `daemon::build_router`. Resolves the client address from the TCP peer
/// address (`ConnectInfo<SocketAddr>`, populated by
/// `into_make_service_with_connect_info`) and, only when that peer is a
/// configured `server.abuse_limits.trusted_proxies` entry,
/// `X-Forwarded-For` — see [`trusted_proxy::resolve_client_addr`] for the
/// exact right-to-left walk. Without a configured trusted proxy, every
/// client behind a reverse proxy shares the proxy's own address, same as
/// before trusted-proxy support existed.
///
/// A direct loopback peer that is not itself a configured trusted proxy is
/// always exempt, regardless of `abuse_limits.enable` — a local daemon
/// must never throttle its own user. Every other case is limited,
/// including a request arriving via a trusted loopback proxy: the real
/// client `X-Forwarded-For` names is limited on its own address (never
/// exempt, even if that address is itself loopback — a header never
/// authenticates anything), and a trusted proxy whose header resolves to
/// nothing usable is limited under the proxy's own address rather than
/// treated as exempt or unidentifiable. See
/// `trusted_proxy::ClientAddr::is_loopback_exempt`'s doc comment.
///
/// A request with no `ConnectInfo` at all (a test harness invoking a
/// handler directly, bypassing the real TCP listener) is also never
/// limited: there is nothing reliable to key on, the same "don't limit
/// what you can't identify" rule a non-TCP transport would get if the
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
    let forwarded_for = forwarded_for_header_values(&req);
    let limiter = state.abuse_limiter();
    let client = limiter.resolve_client_addr(addr.ip(), forwarded_for.as_deref());
    if client.is_loopback_exempt() {
        return next.run(req).await;
    }
    match limiter.check(trusted_proxy::limiter_key(client.key())) {
        Ok(()) => next.run(req).await,
        Err(wait) => too_many_requests(req.uri().path(), wait),
    }
}

/// Every `X-Forwarded-For` header line on `req`, in the order they appear,
/// or `None` if the header is absent or any one line is not valid UTF-8.
/// A header that fails to decode must not silently fall back to whatever
/// other lines *did* decode — that would let an attacker who can make one
/// line unreadable control which of the remaining lines resolution sees,
/// the same class of problem a malformed hop inside a line is (see
/// `trusted_proxy::resolve_client_addr`'s doc comment) — so one bad line
/// fails the whole header, just as a missing header does. Only this header
/// is accepted — not RFC 7239's `Forwarded: for=`.
fn forwarded_for_header_values(req: &Request) -> Option<Vec<String>> {
    let mut values = Vec::new();
    for v in req.headers().get_all("x-forwarded-for").iter() {
        values.push(v.to_str().ok()?.to_string());
    }
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

#[cfg(test)]
mod tests;
