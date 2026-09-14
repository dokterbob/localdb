use super::*;

#[test]
fn default_grant_and_response_types_match_rfc7591_defaults() {
    assert_eq!(
        default_grant_types(),
        vec![
            "authorization_code".to_string(),
            "refresh_token".to_string()
        ]
    );
    assert_eq!(default_response_types(), vec!["code".to_string()]);
}

// -----------------------------------------------------------------
// Finding #3: unsupported grant_types/response_types entries.
// -----------------------------------------------------------------

#[test]
fn unsupported_entry_flags_a_disallowed_grant_type() {
    let requested = vec!["client_credentials".to_string()];
    assert_eq!(
        unsupported_entry(&requested, localdb_core::auth::SUPPORTED_GRANT_TYPES),
        Some("client_credentials")
    );
}

#[test]
fn unsupported_entry_allows_every_supported_grant_type() {
    let requested = vec![
        "authorization_code".to_string(),
        "refresh_token".to_string(),
    ];
    assert_eq!(
        unsupported_entry(&requested, localdb_core::auth::SUPPORTED_GRANT_TYPES),
        None
    );
}

#[test]
fn unsupported_entry_flags_a_disallowed_response_type() {
    let requested = vec!["token".to_string()];
    assert_eq!(
        unsupported_entry(&requested, localdb_core::auth::SUPPORTED_RESPONSE_TYPES),
        Some("token")
    );
}

// -----------------------------------------------------------------
// Finding #4: a store-layer (internal-class) `register_client` failure
// must become a generic, detail-free 500 (`internal_register_error`),
// never a `400 invalid_client_metadata` echoing `e.to_string()`.
// `LibsqlAuthStore` has no seam to make `create_oauth_client` itself
// return a store-layer error, so — mirroring the same limitation noted
// for finding #1 in `oauth.rs` — this is covered at the
// response-shaping level rather than end-to-end through a real store
// fault; the shared classification predicate itself
// (`crate::auth::is_internal_class_error`) is exercised by `oauth.rs`'s
// own `internal_class_errors_are_classified_internal`/
// `client_input_errors_are_not_classified_internal` tests, which cover
// this call site too since the predicate is the same one.
// -----------------------------------------------------------------

#[tokio::test]
async fn internal_register_error_is_a_generic_500_with_no_leaked_detail() {
    let resp = internal_register_error();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"], "server_error");
    assert_eq!(body["error_description"], "internal error");
}
