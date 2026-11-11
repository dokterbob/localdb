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

/// The OAuth consent page, the OOB success page, and the `/join` page all
/// render attacker-controlled values into HTML; framing one in a hidden
/// iframe must not be possible, so every response this middleware wraps
/// carries both the modern and legacy clickjacking defenses alongside the
/// existing no-store/no-referrer headers.
#[tokio::test]
async fn protect_auth_response_sets_security_headers() {
    use axum::{body::Body, http::Request, middleware, routing::get, Router};
    use tower::ServiceExt;

    async fn ok() -> &'static str {
        "ok"
    }

    let app = Router::new()
        .route("/", get(ok))
        .layer(middleware::from_fn(protect_auth_response));

    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.headers()["referrer-policy"], "no-referrer");
    assert_eq!(
        resp.headers()["content-security-policy"],
        "frame-ancestors 'none'"
    );
    assert_eq!(resp.headers()["x-frame-options"], "DENY");
}
