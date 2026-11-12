//! Abuse limits on the daemon's public, unauthenticated auth routes
//! (specs/05-surfaces.md §3.1): the `POST /register` dynamic-client
//! registration cap/eviction, and the per-client-address rate limit
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
        trusted_proxies: Vec::new(),
    }
}

/// Like `abuse_limits`, with an explicit `trusted_proxies` list — the
/// trusted-proxy/`X-Forwarded-For` tests need this; every other test in
/// this file trusts no proxy.
fn abuse_limits_with_trusted_proxies(
    requests_per_second: u32,
    burst: u32,
    trusted_proxies: Vec<String>,
) -> AbuseLimitsConfig {
    AbuseLimitsConfig {
        enable: true,
        max_dynamic_clients: 1000,
        unused_client_ttl_secs: 86400,
        requests_per_second,
        burst,
        trusted_proxies,
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

/// With the cap configured to 3, registering past it does not refuse —
/// each new registration evicts the oldest dynamically registered client
/// that has never completed an authorization, so a registration flood
/// can't lock out every other registration attempt. The oldest client
/// (never authorized) ends up evicted; the most recent survives.
#[tokio::test]
async fn registrations_past_the_cap_evict_the_oldest_unused_client() {
    // Generous rate limit so only the registration cap is under test —
    // `request()` carries no `ConnectInfo` anyway (see its doc comment),
    // so the rate limiter would never apply to these calls regardless.
    let (_dir, state, app) = make_app_with_abuse_limits(abuse_limits(true, 3, 1000, 1000)).await;

    let mut client_ids = Vec::new();
    for i in 0..5 {
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
            "registration {i} should succeed — past the cap, the oldest unused client is \
             evicted rather than the request refused"
        );
        let body = json_body(resp.into_body()).await;
        client_ids.push(
            body["client_id"]
                .as_str()
                .expect("201 response must carry client_id")
                .to_string(),
        );
    }

    assert!(
        !state.auth().is_known_client(&client_ids[0]).await.unwrap(),
        "the first (oldest, never-authorized) registered client must have been evicted"
    );
    assert!(
        state
            .auth()
            .is_known_client(client_ids.last().unwrap())
            .await
            .unwrap(),
        "the most recently registered client must still be known"
    );
}

/// Once every dynamic-client slot is held by a client with a completed
/// authorization, there is nothing left to evict and `POST /register`
/// refuses with `429` and `Retry-After`, in the RFC 7591-shaped body this
/// route already uses for validation failures.
#[tokio::test]
async fn registration_is_refused_once_every_slot_is_used() {
    let (_dir, state, app) = make_app_with_abuse_limits(abuse_limits(true, 1, 1000, 1000)).await;

    let resp = request(
        app.clone(),
        Method::POST,
        "/register",
        Some(register_body("https://app0.example.com/cb")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    let client_id = body["client_id"]
        .as_str()
        .expect("201 response must carry client_id")
        .to_string();

    // Give the only dynamic client a completed authorization — it is now
    // "used" and must never be evicted.
    let user = state
        .auth()
        .create_user("abuse-limits-cap-user", localdb_core::auth::Role::Member)
        .await
        .unwrap();
    state
        .auth()
        .issue_auth_code(
            &client_id,
            &user.id,
            "https://app0.example.com/cb",
            "challenge",
            None,
        )
        .await
        .unwrap();

    let resp = request(
        app.clone(),
        Method::POST,
        "/register",
        Some(register_body("https://app1.example.com/cb")),
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

/// `enable: false` is the master switch for every limit in
/// `server.abuse_limits` (its own doc comment) — a tiny
/// `max_dynamic_clients` must not still be enforced while the section is
/// disabled.
#[tokio::test]
async fn registrations_past_a_tiny_cap_succeed_while_abuse_limits_are_disabled() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(false, 1, 1000, 1000)).await;

    for i in 0..5 {
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
            "registration {i} must succeed past the cap while abuse_limits.enable is false"
        );
    }
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

// -----------------------------------------------------------------
// Trusted-proxy support (specs/05-surfaces.md §3.1): `X-Forwarded-For` is
// honored only from a configured, trusted peer — see
// `server::auth::trusted_proxy` for the pure resolution logic, table-tested
// there; these prove the end-to-end wiring through the real router.
// -----------------------------------------------------------------

#[tokio::test]
async fn forwarded_for_from_an_untrusted_peer_is_ignored() {
    let (_dir, _state, app) =
        make_app_with_abuse_limits(abuse_limits_with_trusted_proxies(1, 1, Vec::new())).await;
    let peer = non_loopback_peer();

    // Two distinct X-Forwarded-For values from the same (untrusted) peer
    // must share the peer's own bucket, not get independent limits.
    let first = request_from_with_headers(
        app.clone(),
        Method::GET,
        "/join",
        None,
        peer,
        &[("x-forwarded-for", "198.51.100.1")],
    )
    .await;
    assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    let second = request_from_with_headers(
        app.clone(),
        Method::GET,
        "/join",
        None,
        peer,
        &[("x-forwarded-for", "198.51.100.2")],
    )
    .await;
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "an untrusted peer's claimed X-Forwarded-For must never carve out a separate bucket"
    );
}

#[tokio::test]
async fn forwarded_for_from_a_trusted_loopback_peer_is_honored() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits_with_trusted_proxies(
        1,
        1,
        vec!["127.0.0.1".to_string()],
    ))
    .await;
    let peer = loopback_peer();

    let first = request_from_with_headers(
        app.clone(),
        Method::GET,
        "/join",
        None,
        peer,
        &[("x-forwarded-for", "198.51.100.1")],
    )
    .await;
    assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    let second = request_from_with_headers(
        app.clone(),
        Method::GET,
        "/join",
        None,
        peer,
        &[("x-forwarded-for", "198.51.100.1")],
    )
    .await;
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the real client named by a trusted proxy's header must be rate limited"
    );

    // A different claimed client through the same trusted proxy gets its
    // own, independent bucket.
    let other_client = request_from_with_headers(
        app.clone(),
        Method::GET,
        "/join",
        None,
        peer,
        &[("x-forwarded-for", "198.51.100.2")],
    )
    .await;
    assert_ne!(
        other_client.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a different X-Forwarded-For client must have its own bucket"
    );
}

// -----------------------------------------------------------------
// IPv6 keyed per /64 (specs/05-surfaces.md §3.1).
// -----------------------------------------------------------------

fn ipv6_peer(addr: &str) -> SocketAddr {
    SocketAddr::new(addr.parse().unwrap(), 54321)
}

#[tokio::test]
async fn two_ipv6_addresses_in_one_64_share_a_bucket() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 1000, 1, 1)).await;
    let first = request_from(
        app.clone(),
        Method::GET,
        "/join",
        None,
        ipv6_peer("2001:db8:1:2::aaaa"),
    )
    .await;
    assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    let second = request_from(
        app.clone(),
        Method::GET,
        "/join",
        None,
        ipv6_peer("2001:db8:1:2::bbbb"),
    )
    .await;
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "two addresses inside the same /64 must share one bucket"
    );
}

#[tokio::test]
async fn two_different_ipv6_64s_have_independent_buckets() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 1000, 1, 1)).await;
    let a = request_from(
        app.clone(),
        Method::GET,
        "/join",
        None,
        ipv6_peer("2001:db8:1:2::1"),
    )
    .await;
    assert_ne!(a.status(), StatusCode::TOO_MANY_REQUESTS);
    let b = request_from(
        app.clone(),
        Method::GET,
        "/join",
        None,
        ipv6_peer("2001:db8:1:3::1"),
    )
    .await;
    assert_ne!(
        b.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a different /64 must have its own bucket"
    );
}

#[tokio::test]
async fn mapped_ipv4_loopback_peer_is_exempt() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits(true, 1000, 1, 1)).await;
    let peer = ipv6_peer("::ffff:127.0.0.1");
    for i in 0..10 {
        let resp = request_from(app.clone(), Method::GET, "/join", None, peer).await;
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "call {i} via a mapped loopback peer must never be rate limited"
        );
    }
}

#[tokio::test]
async fn a_trusted_proxy_connecting_directly_with_no_header_falls_back_to_its_own_address() {
    let (_dir, _state, app) = make_app_with_abuse_limits(abuse_limits_with_trusted_proxies(
        1,
        1,
        vec!["203.0.113.7".to_string()],
    ))
    .await;
    // The trusted proxy's own address is not loopback, so with no
    // X-Forwarded-For header it is rate limited on its own address, same
    // as an ordinary non-loopback peer.
    let peer = non_loopback_peer();
    let first = request_from(app.clone(), Method::GET, "/join", None, peer).await;
    assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    let second = request_from(app.clone(), Method::GET, "/join", None, peer).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}
