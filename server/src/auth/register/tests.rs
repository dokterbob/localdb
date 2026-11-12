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
// `registration_error_code`: the one place this handler maps a
// `RegistrationError` onto its RFC 7591 §3.2.2 code. Unsupported
// grant_types/response_types entries, and every other validation rule, are
// `core::auth::client::validate_registration_request`'s own job — see that
// module's tests for coverage of the check order and each individual rule.
// -----------------------------------------------------------------

#[test]
fn registration_error_code_maps_invalid_redirect_uri() {
    assert_eq!(
        registration_error_code(&RegistrationError::InvalidRedirectUri("x".to_string())),
        "invalid_redirect_uri"
    );
}

#[test]
fn registration_error_code_maps_every_other_violation_to_invalid_client_metadata() {
    for err in [
        RegistrationError::UnsupportedAuthMethod("client_secret_basic".to_string()),
        RegistrationError::EmptyRedirectUris,
        RegistrationError::TooManyOrOverlongRedirectUris,
        RegistrationError::ClientNameTooLong,
        RegistrationError::UnsupportedGrantType("client_credentials".to_string()),
        RegistrationError::UnsupportedResponseType("token".to_string()),
    ] {
        assert_eq!(registration_error_code(&err), "invalid_client_metadata");
    }
}

// -----------------------------------------------------------------
// A store-layer (internal-class) `register_client` failure
// must become a generic, detail-free 500 (`internal_register_error`),
// never a `400 invalid_client_metadata` echoing `e.to_string()`.
// `LibsqlAuthStore` has no seam to make `create_oauth_client` itself
// return a store-layer error, so — mirroring the same limitation in
// `oauth.rs` — this test covers the response-shaping level rather
// than end-to-end through a real store fault; the shared
// classification predicate itself
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
