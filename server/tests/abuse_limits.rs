//! Abuse limits on the daemon's public, unauthenticated auth routes
//! (specs/05-surfaces.md §3.1): the `POST /register` dynamic-client
//! registration cap, and the per-client-IP rate limit
//! (`server::auth::rate_limit`).

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use localdb_core::config::schema::AbuseLimitsConfig;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn abuse_limits(
    enable: bool,
    max_dynamic_clients: u64,
    requests_per_second: u32,
    burst: u32,
) -> AbuseLimitsConfig {
    AbuseLimitsConfig {
        enable,
        max_dynamic_clients,
        unused_client_ttl_secs: 86400,
        requests_per_second,
        burst,
    }
}

fn loopback_peer() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 54321)
}

fn non_loopback_peer() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 54321)
}

fn register_body(redirect: &str) -> serde_json::Value {
    json!({ "redirect_uris": [redirect] })
}

// -----------------------------------------------------------------
// Registration cap (`AuthService::register_client` ->
// `AuthStore::prune_and_create_oauth_client`).
// -----------------------------------------------------------------

/// With the cap configured to 3, a 4th `POST /register` is refused with
/// `429` and `Retry-After`, in the RFC 7591-shaped body this route already
/// uses for validation failures.
#[tokio::test]
async fn fourth_registration_past_the_cap_is_refused_with_429() {
    // Generous rate limit so only the registration cap is under test —
    // `request()` carries no `ConnectInfo` anyway (see its doc comment),
    // so the rate limiter would never apply to these calls regardless.
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 3, 1000, 1000)).await;

    for i in 0..3 {
        let resp = request(
            app.clone(),
            Method::POST,
            "/register",
            Some(register_body(&format!("https://app{i}.example.com/cb"))),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "registration {i} should succeed under the cap"
        );
    }

    let resp = request(
        app.clone(),
        Method::POST,
        "/register",
        Some(register_body("https://app3.example.com/cb")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        resp.headers()
            .get(axum::http::header::RETRY_AFTER)
            .is_some(),
        "a cap-reached refusal must carry Retry-After"
    );
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "temporarily_unavailable");
}

// -----------------------------------------------------------------
// Per-client-IP rate limit (`server::auth::rate_limit::limit_abuse`).
// `GET /join` (no query) is used as the generic probe route: it needs no
// body, and a request that reaches the handler gets a `400` (missing
// `invite`) — clearly distinguishable from the middleware's own `429`.
// -----------------------------------------------------------------

#[tokio::test]
async fn loopback_peer_is_never_rate_limited() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 1000, 1, 1)).await;
    for i in 0..50 {
        let resp = request_from(app.clone(), Method::GET, "/join", None, loopback_peer()).await;
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "loopback call {i} must never be rate limited"
        );
    }
}

#[tokio::test]
async fn non_loopback_peer_is_rate_limited_past_the_burst() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 1000, 1, 1)).await;
    let peer = non_loopback_peer();

    let first = request_from(app.clone(), Method::GET, "/join", None, peer).await;
    assert_ne!(
        first.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the first call is within the burst"
    );

    let second = request_from(app.clone(), Method::GET, "/join", None, peer).await;
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the second call exceeds burst=1"
    );
    assert!(second
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .is_some());
    let body = json_body(second.into_body()).await;
    assert_eq!(body["code"], "rate_limited");
}

#[tokio::test]
async fn distinct_non_loopback_peers_have_independent_limits() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 1000, 1, 1)).await;
    let peer_a = non_loopback_peer();
    let peer_b = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8)), 54321);

    let a = request_from(app.clone(), Method::GET, "/join", None, peer_a).await;
    assert_ne!(a.status(), StatusCode::TOO_MANY_REQUESTS);
    let b = request_from(app.clone(), Method::GET, "/join", None, peer_b).await;
    assert_ne!(
        b.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a different peer must have its own bucket"
    );
}

#[tokio::test]
async fn limiter_disabled_by_config_never_limits_a_non_loopback_peer() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(false, 1000, 1, 1)).await;
    let peer = non_loopback_peer();
    for i in 0..10 {
        let resp = request_from(app.clone(), Method::GET, "/join", None, peer).await;
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "call {i} must not be limited while abuse_limits.enable is false"
        );
    }
}

/// A request with no `ConnectInfo` at all (every other test in this file
/// that calls the plain `request()` helper, not `request_from`) must never
/// be limited — see `limit_abuse`'s doc comment for why.
#[tokio::test]
async fn request_with_no_connect_info_is_never_limited() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 1000, 1, 1)).await;
    for i in 0..10 {
        let resp = request(app.clone(), Method::GET, "/join", None).await;
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "call {i} without ConnectInfo must not be limited"
        );
    }
}
