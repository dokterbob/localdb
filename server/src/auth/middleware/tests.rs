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
