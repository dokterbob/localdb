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
