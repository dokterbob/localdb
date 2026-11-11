use super::*;

#[test]
fn known_client_is_localdb_cli_only() {
    assert!(is_known_client(LOCALDB_CLI_CLIENT_ID));
    assert!(!is_known_client("some-other-client"));
    assert!(!is_known_client(""));
}

#[test]
fn loopback_v4_with_path_is_valid_for_localdb_cli() {
    assert!(validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://127.0.0.1:54321/callback"
    ));
}

#[test]
fn loopback_localhost_with_path_is_valid_for_localdb_cli() {
    assert!(validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://localhost:8080/cb"
    ));
}

#[test]
fn loopback_without_path_is_valid() {
    assert!(validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://127.0.0.1:1234"
    ));
}

#[test]
fn any_port_is_accepted() {
    assert!(validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://127.0.0.1:1/x"
    ));
    assert!(validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://127.0.0.1:65535/x"
    ));
}

#[test]
fn oob_sentinel_is_valid_for_localdb_cli() {
    assert!(validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        OOB_REDIRECT_URI
    ));
}

#[test]
fn non_loopback_host_is_rejected() {
    assert!(!validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://example.com:8080/callback"
    ));
    assert!(!validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://evil.com:80/callback"
    ));
}

#[test]
fn https_loopback_is_rejected() {
    // The CLI's own ephemeral listener is plain HTTP; https loopback is
    // not part of the RFC 8252 exception this function implements.
    assert!(!validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "https://127.0.0.1:8080/callback"
    ));
}

#[test]
fn missing_port_is_rejected() {
    assert!(!validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://127.0.0.1/callback"
    ));
}

#[test]
fn unknown_client_is_always_rejected_regardless_of_uri() {
    assert!(!validate_redirect_uri(
        "some-other-client",
        "http://127.0.0.1:1234/callback"
    ));
}

#[test]
fn host_substring_lookalikes_are_rejected() {
    assert!(!validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://127.0.0.1.evil.com:1234/callback"
    ));
    assert!(!validate_redirect_uri(
        LOCALDB_CLI_CLIENT_ID,
        "http://notlocalhost:1234/callback"
    ));
}

// -----------------------------------------------------------------
// T7: DCR redirect_uri validation
// -----------------------------------------------------------------

#[test]
fn registration_accepts_https_url() {
    assert!(validate_registration_redirect_uri(
        "https://app.example.com/oauth/callback"
    ));
}

#[test]
fn registration_accepts_loopback_http() {
    assert!(validate_registration_redirect_uri(
        "http://127.0.0.1:51234/callback"
    ));
    assert!(validate_registration_redirect_uri(
        "http://localhost:8080/cb"
    ));
}

#[test]
fn registration_rejects_plain_http_non_loopback() {
    assert!(!validate_registration_redirect_uri(
        "http://app.example.com/callback"
    ));
}

#[test]
fn registration_rejects_custom_scheme() {
    assert!(!validate_registration_redirect_uri(
        "myapp://oauth/callback"
    ));
    assert!(!validate_registration_redirect_uri("cursor://callback"));
}

#[test]
fn registration_rejects_empty_https_host() {
    assert!(!validate_registration_redirect_uri("https://"));
}

#[test]
fn registration_rejects_oob_sentinel() {
    // The OOB sentinel is a localdb-cli-only fallback, not a valid
    // registration redirect for a general public client.
    assert!(!validate_registration_redirect_uri(OOB_REDIRECT_URI));
}

// -----------------------------------------------------------------
// `validate_registration_request`: the one DCR validator
// `AuthService::register_client` enforces.
// -----------------------------------------------------------------

#[test]
fn validate_registration_request_accepts_a_normal_request() {
    assert_eq!(
        validate_registration_request(
            &["https://app.example.com/cb".to_string()],
            Some("A Normal Client"),
            None,
            None,
            None,
        ),
        Ok(())
    );
}

#[test]
fn validate_registration_request_rejects_empty_redirect_uris() {
    assert_eq!(
        validate_registration_request(&[], None, None, None, None),
        Err(RegistrationError::EmptyRedirectUris)
    );
}

#[test]
fn validate_registration_request_rejects_too_many_redirect_uris() {
    let redirect_uris: Vec<String> = (0..MAX_REGISTRATION_REDIRECT_URIS + 1)
        .map(|i| format!("https://app.example.com/cb{i}"))
        .collect();
    assert_eq!(
        validate_registration_request(&redirect_uris, None, None, None, None),
        Err(RegistrationError::TooManyOrOverlongRedirectUris)
    );
}

#[test]
fn validate_registration_request_rejects_an_overlong_redirect_uri() {
    let long_uri = format!(
        "https://app.example.com/{}",
        "a".repeat(MAX_REGISTRATION_REDIRECT_URI_LEN)
    );
    assert_eq!(
        validate_registration_request(&[long_uri], None, None, None, None),
        Err(RegistrationError::TooManyOrOverlongRedirectUris)
    );
}

#[test]
fn validate_registration_request_rejects_an_overlong_client_name() {
    let long_name = "a".repeat(MAX_REGISTRATION_CLIENT_NAME_LEN + 1);
    assert_eq!(
        validate_registration_request(
            &["https://app.example.com/cb".to_string()],
            Some(&long_name),
            None,
            None,
            None,
        ),
        Err(RegistrationError::ClientNameTooLong)
    );
}

#[test]
fn validate_registration_request_rejects_an_invalid_redirect_uri_shape() {
    assert_eq!(
        validate_registration_request(
            &["http://evil.example.com/cb".to_string()],
            None,
            None,
            None,
            None,
        ),
        Err(RegistrationError::InvalidRedirectUri(
            "http://evil.example.com/cb".to_string()
        ))
    );
}

#[test]
fn validate_registration_request_checks_bounds_before_per_uri_validity() {
    // A request that violates both the count cap and per-URI shape still
    // reports the bounds violation — the validator checks bounds before
    // looping over shapes.
    let mut redirect_uris: Vec<String> = (0..MAX_REGISTRATION_REDIRECT_URIS + 1)
        .map(|i| format!("https://app.example.com/cb{i}"))
        .collect();
    redirect_uris.push("http://evil.example.com/cb".to_string());
    assert_eq!(
        validate_registration_request(&redirect_uris, None, None, None, None),
        Err(RegistrationError::TooManyOrOverlongRedirectUris)
    );
}

// -----------------------------------------------------------------
// `validate_registration_request`'s full precedence order: an
// `token_endpoint_auth_method`/`grant_types`/`response_types` violation
// must never outrank a `redirect_uris` violation — a request that violates
// both must report the `redirect_uris` problem, matching the order this
// server enforced before `token_endpoint_auth_method`/`grant_types`/
// `response_types` moved from `server::auth::register::post_register`'s own
// handler-side checks into this single validator.
// -----------------------------------------------------------------

#[test]
fn validate_registration_request_rejects_an_unsupported_auth_method() {
    assert_eq!(
        validate_registration_request(
            &["https://app.example.com/cb".to_string()],
            None,
            Some("client_secret_basic"),
            None,
            None,
        ),
        Err(RegistrationError::UnsupportedAuthMethod(
            "client_secret_basic".to_string()
        ))
    );
}

#[test]
fn validate_registration_request_accepts_auth_method_none() {
    assert_eq!(
        validate_registration_request(
            &["https://app.example.com/cb".to_string()],
            None,
            Some("none"),
            None,
            None,
        ),
        Ok(())
    );
}

#[test]
fn validate_registration_request_auth_method_outranks_redirect_uri_problems() {
    // An unsupported auth method on an otherwise-invalid request still
    // reports the auth-method violation — it is the first check.
    assert_eq!(
        validate_registration_request(&[], None, Some("client_secret_basic"), None, None,),
        Err(RegistrationError::UnsupportedAuthMethod(
            "client_secret_basic".to_string()
        ))
    );
}

#[test]
fn validate_registration_request_rejects_an_unsupported_grant_type() {
    assert_eq!(
        validate_registration_request(
            &["https://app.example.com/cb".to_string()],
            None,
            None,
            Some(&["client_credentials".to_string()]),
            None,
        ),
        Err(RegistrationError::UnsupportedGrantType(
            "client_credentials".to_string()
        ))
    );
}

#[test]
fn validate_registration_request_rejects_an_unsupported_response_type() {
    assert_eq!(
        validate_registration_request(
            &["https://app.example.com/cb".to_string()],
            None,
            None,
            None,
            Some(&["token".to_string()]),
        ),
        Err(RegistrationError::UnsupportedResponseType(
            "token".to_string()
        ))
    );
}

/// The regression this fixes: `redirect_uris` validation must outrank
/// `grant_types`/`response_types` validation — a request with both an
/// invalid `redirect_uris` entry and an unsupported `grant_types` entry
/// must report `InvalidRedirectUri`, matching this server's behavior before
/// the two checks' relative order drifted (grant_types/response_types were
/// briefly checked in the handler *before* `register_client`'s own
/// redirect_uris validation ran).
#[test]
fn validate_registration_request_redirect_uri_outranks_grant_type_problems() {
    assert_eq!(
        validate_registration_request(
            &["http://evil.example.com/cb".to_string()],
            None,
            None,
            Some(&["client_credentials".to_string()]),
            None,
        ),
        Err(RegistrationError::InvalidRedirectUri(
            "http://evil.example.com/cb".to_string()
        ))
    );
}
