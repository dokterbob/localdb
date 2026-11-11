use super::*;
use std::net::{IpAddr, Ipv4Addr};

fn cfg(enable: bool, rps: u32, burst: u32) -> AbuseLimitsConfig {
    AbuseLimitsConfig {
        enable,
        max_dynamic_clients: 1000,
        unused_client_ttl_secs: 86400,
        requests_per_second: rps,
        burst,
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
// `too_many_requests` — the three response-body shapes.
// -----------------------------------------------------------------

async fn body_json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body should be readable");
    serde_json::from_slice(&bytes).expect("response body should be valid JSON")
}

#[tokio::test]
async fn token_route_gets_the_oauth_style_slow_down_body() {
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
    assert_eq!(json["error"], "slow_down");
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
