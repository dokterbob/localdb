use super::*;
use std::net::{IpAddr, Ipv4Addr};

fn cfg(enable: bool, rps: u32, burst: u32) -> AbuseLimitsConfig {
    AbuseLimitsConfig {
        enable,
        max_dynamic_clients: 1000,
        unused_client_ttl_secs: 86400,
        requests_per_second: rps,
        burst,
        trusted_proxies: Vec::new(),
    }
}

fn ip(last_octet: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, last_octet))
}

// -----------------------------------------------------------------
// `AbuseLimiter::check` — a tiny burst makes the token-bucket boundary
// deterministic without a fake clock or any sleep: two back-to-back calls
// against burst=1 always land on either side of the boundary.
// -----------------------------------------------------------------

#[test]
fn burst_of_one_then_the_second_call_is_rejected() {
    let limiter = AbuseLimiter::new(&cfg(true, 1, 1));
    assert!(limiter.check(ip(1)).is_ok(), "first call is within burst");
    assert!(
        limiter.check(ip(1)).is_err(),
        "second call exceeds the burst"
    );
}

#[test]
fn rejection_reports_a_positive_wait() {
    let limiter = AbuseLimiter::new(&cfg(true, 1, 1));
    limiter.check(ip(1)).unwrap();
    let wait = limiter.check(ip(1)).unwrap_err();
    assert!(wait > Duration::ZERO);
}

#[test]
fn distinct_ips_have_independent_buckets() {
    let limiter = AbuseLimiter::new(&cfg(true, 1, 1));
    assert!(limiter.check(ip(1)).is_ok());
    assert!(
        limiter.check(ip(2)).is_ok(),
        "a different IP must have its own bucket"
    );
}

#[test]
fn disabled_config_never_limits() {
    let limiter = AbuseLimiter::new(&cfg(false, 1, 1));
    for _ in 0..50 {
        assert!(
            limiter.check(ip(1)).is_ok(),
            "a disabled limiter must always allow"
        );
    }
}

#[test]
fn disabled_helper_never_limits() {
    let limiter = AbuseLimiter::disabled();
    for _ in 0..50 {
        assert!(limiter.check(ip(1)).is_ok());
    }
}

// -----------------------------------------------------------------
// `AbuseLimiter::resolve_client_addr` — the glue between config and
// `trusted_proxy::resolve_client_addr`; the resolution logic itself is
// table-tested in `trusted_proxy::tests`.
// -----------------------------------------------------------------

fn cfg_with_trusted_proxies(trusted_proxies: Vec<String>) -> AbuseLimitsConfig {
    AbuseLimitsConfig {
        trusted_proxies,
        ..cfg(true, 5, 20)
    }
}

#[test]
fn resolve_client_addr_ignores_the_header_from_an_untrusted_peer() {
    let limiter = AbuseLimiter::new(&cfg_with_trusted_proxies(Vec::new()));
    let resolved = limiter.resolve_client_addr(ip(7), &["198.51.100.1".to_string()]);
    assert_eq!(resolved, ip(7));
}

#[test]
fn resolve_client_addr_honors_the_header_from_a_trusted_peer() {
    let limiter = AbuseLimiter::new(&cfg_with_trusted_proxies(vec!["127.0.0.1".to_string()]));
    let resolved = limiter.resolve_client_addr(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        &["198.51.100.1".to_string()],
    );
    assert_eq!(resolved, "198.51.100.1".parse::<IpAddr>().unwrap());
}

#[test]
#[should_panic(expected = "should already be validated")]
fn new_panics_on_an_unvalidated_trusted_proxies_entry() {
    // Defensive: `validate_config` should already have rejected this, but
    // `AbuseLimiter::new` must fail loudly, not silently drop the entry,
    // if that invariant is ever violated.
    AbuseLimiter::new(&cfg_with_trusted_proxies(
        vec!["not-an-address".to_string()],
    ));
}

// -----------------------------------------------------------------
// `too_many_requests` — the three response-body shapes.
// -----------------------------------------------------------------

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body should be readable");
    serde_json::from_slice(&bytes).expect("response body should be valid JSON")
}

#[tokio::test]
async fn token_route_gets_the_oauth_style_temporarily_unavailable_body() {
    let response = too_many_requests("/token", Duration::from_secs(5));
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .unwrap(),
        "5"
    );
    let json = body_json(response).await;
    // Not `slow_down`: RFC 8628 §3.5's device-flow code, which this
    // endpoint (authorization-code and refresh grants only) never
    // otherwise emits.
    assert_eq!(json["error"], "temporarily_unavailable");
}

#[tokio::test]
async fn register_route_gets_the_oauth_style_temporarily_unavailable_body() {
    let response = too_many_requests("/register", Duration::from_secs(1));
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let json = body_json(response).await;
    assert_eq!(json["error"], "temporarily_unavailable");
}

#[tokio::test]
async fn other_routes_get_the_standard_error_shape() {
    let response = too_many_requests("/authorize", Duration::from_millis(1500));
    // Rounds up: 1.5s must report 2, never 1 — a caller retrying at exactly
    // the reported delay must never arrive a token early.
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .unwrap(),
        "2"
    );
    let json = body_json(response).await;
    assert_eq!(json["code"], "rate_limited");
    assert!(json.get("error").is_none(), "must not be the OAuth shape");
}

#[tokio::test]
async fn zero_wait_still_reports_at_least_one_second() {
    let response = too_many_requests("/join", Duration::ZERO);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .unwrap(),
        "1"
    );
}
