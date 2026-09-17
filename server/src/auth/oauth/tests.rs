use super::*;
use localdb_core::auth::{AuthService, FakeAuthStore};
use std::sync::Arc;

/// A fresh in-memory `AuthService` — recognizes the built-in
/// `localdb-cli` client purely (no DB row needed) plus whatever T7 test
/// callers register on it, mirroring `core::auth::service`'s own test
/// helper.
fn fake_auth() -> AuthService<FakeAuthStore> {
    AuthService::new(Arc::new(FakeAuthStore::new()))
}

// -----------------------------------------------------------------
// Finding #5: internal-class token-issuance/rotation failures must map
// to 500, not 400 `server_error`/`invalid_grant`. `AuthService`'s
// `FakeAuthStore` has no seam to make `issue_access_token`/
// `issue_refresh_token`/`rotate_refresh_token` return a store-layer
// error (its only poison hook, `poison_next_revoke`, makes
// `revoke_token` report a lost race — which `rotate_refresh_token`
// already turns into `Error::Unauthorized`, not an internal error), so
// this is covered at the classification-helper level per the fix's own
// guidance rather than end-to-end through a real store fault.
// -----------------------------------------------------------------

#[test]
fn internal_class_errors_are_classified_internal() {
    let internal_errors = [
        CoreError::Internal {
            message: "bug".into(),
            correlation_id: "x".into(),
        },
        CoreError::RuntimeStateLocked,
        CoreError::DaemonRunning,
        CoreError::IndexInProgress,
        CoreError::DaemonUnreachable,
        CoreError::ProviderUnavailable {
            message: "down".into(),
        },
        CoreError::ModelMissing {
            message: "missing".into(),
        },
    ];
    for err in internal_errors {
        assert!(
            crate::auth::is_internal_class_error(&err),
            "{err:?} should be classified internal"
        );
    }
}

#[test]
fn client_input_errors_are_not_classified_internal() {
    let client_errors = [
        CoreError::Unauthorized {
            message: "bad token".into(),
        },
        CoreError::InvalidRequest {
            message: "bad request".into(),
        },
        CoreError::Forbidden {
            message: "no access".into(),
        },
        CoreError::StoreNotFound { id: "x".into() },
    ];
    for err in client_errors {
        assert!(
            !crate::auth::is_internal_class_error(&err),
            "{err:?} should not be classified internal"
        );
    }
}

async fn response_body_json(resp: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn token_error_for_failure_maps_internal_class_to_500_generic_body() {
    let err = CoreError::Internal {
        message: "raw sql error: table auth_tokens has no column named oops".into(),
        correlation_id: "abc123".into(),
    };
    let resp = token_error_for_failure(
        err,
        "the refresh token is invalid, expired, or already used",
    );
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_body_json(resp).await;
    assert_eq!(body["error"], "server_error");
    let description = body["error_description"].as_str().unwrap();
    assert!(
        !description.contains("auth_tokens") && !description.contains("abc123"),
        "internal error detail must not leak into the response: {description}"
    );
}

#[tokio::test]
async fn token_error_for_failure_keeps_client_class_at_400_invalid_grant() {
    let err = CoreError::Unauthorized {
        message: "refresh token has expired".to_string(),
    };
    let resp = token_error_for_failure(
        err,
        "the refresh token is invalid, expired, or already used",
    );
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = response_body_json(resp).await;
    assert_eq!(body["error"], "invalid_grant");
    assert_eq!(
        body["error_description"],
        "the refresh token is invalid, expired, or already used"
    );
}

// -----------------------------------------------------------------
// Finding #1: `handle_auth_code_grant`'s `redeem_auth_code` failure arm
// must classify the error the same way the issuance/refresh paths do
// (via `token_error_for_failure`), not collapse every failure to `400
// invalid_grant` unconditionally. `FakeAuthStore` has no seam to make
// `redeem_auth_code` itself return a store-layer error (its `auth_codes`
// vec has no poison hook), so — per the fix's own guidance — this pins
// the classification at this call site's exact description text rather
// than driving a real internal fault end-to-end through the HTTP route.
// -----------------------------------------------------------------

const AUTH_CODE_GRANT_FAILURE_DESCRIPTION: &str =
    "the authorization code is invalid, expired, already used, or does not match \
         this client/redirect_uri/code_verifier";

#[tokio::test]
async fn auth_code_grant_failure_maps_internal_class_error_to_500() {
    let err = CoreError::Internal {
        message: "raw sql error: database is locked".into(),
        correlation_id: "corr-1".into(),
    };
    let resp = token_error_for_failure(err, AUTH_CODE_GRANT_FAILURE_DESCRIPTION);
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response_body_json(resp).await;
    assert_eq!(body["error"], "server_error");
    let description = body["error_description"].as_str().unwrap();
    assert!(
        !description.contains("database is locked") && !description.contains("corr-1"),
        "internal error detail must not leak into the response: {description}"
    );
}

#[tokio::test]
async fn auth_code_grant_failure_keeps_genuine_bad_code_at_400_invalid_grant() {
    let err = CoreError::Unauthorized {
        message: "authorization code already used".to_string(),
    };
    let resp = token_error_for_failure(err, AUTH_CODE_GRANT_FAILURE_DESCRIPTION);
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = response_body_json(resp).await;
    assert_eq!(body["error"], "invalid_grant");
    assert_eq!(
        body["error_description"],
        AUTH_CODE_GRANT_FAILURE_DESCRIPTION
    );
}

#[test]
fn escape_html_neutralizes_script_tags() {
    let escaped = escape_html("<script>alert(1)</script>");
    assert!(!escaped.contains("<script>"));
    assert!(escaped.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn validate_authorize_params_happy_path() {
    let auth = fake_auth();
    let params = validate_authorize_params(
        &auth,
        Some("code"),
        Some("localdb-cli"),
        Some("http://127.0.0.1:1234/callback"),
        Some("xyz"),
        Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
        Some("S256"),
    )
    .await
    .unwrap();
    assert_eq!(params.client_id, "localdb-cli");
    assert_eq!(params.state, "xyz");
}

#[tokio::test]
async fn validate_authorize_params_rejects_bad_redirect_uri() {
    let auth = fake_auth();
    let err = validate_authorize_params(
        &auth,
        Some("code"),
        Some("localdb-cli"),
        Some("http://evil.example.com/callback"),
        None,
        Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
        Some("S256"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "invalid_request");
}

#[tokio::test]
async fn validate_authorize_params_rejects_missing_pkce() {
    let auth = fake_auth();
    let err = validate_authorize_params(
        &auth,
        Some("code"),
        Some("localdb-cli"),
        Some("http://127.0.0.1:1/callback"),
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "invalid_request");
}

#[tokio::test]
async fn validate_authorize_params_rejects_plain_challenge_method() {
    let auth = fake_auth();
    let err = validate_authorize_params(
        &auth,
        Some("code"),
        Some("localdb-cli"),
        Some("http://127.0.0.1:1/callback"),
        None,
        Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
        Some("plain"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "invalid_request");
}

#[tokio::test]
async fn validate_authorize_params_rejects_unknown_client() {
    let auth = fake_auth();
    let err = validate_authorize_params(
        &auth,
        Some("code"),
        Some("some-other-client"),
        Some("http://127.0.0.1:1/callback"),
        None,
        Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
        Some("S256"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "unauthorized_client");
}

#[tokio::test]
async fn validate_authorize_params_accepts_registered_client_exact_redirect() {
    let auth = fake_auth();
    let row = auth
        .register_client(vec!["https://app.example.com/cb".to_string()], None)
        .await
        .unwrap();
    let params = validate_authorize_params(
        &auth,
        Some("code"),
        Some(&row.id),
        Some("https://app.example.com/cb"),
        None,
        Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
        Some("S256"),
    )
    .await
    .unwrap();
    assert_eq!(params.client_id, row.id);
}

#[tokio::test]
async fn validate_authorize_params_rejects_registered_client_mismatched_redirect() {
    let auth = fake_auth();
    let row = auth
        .register_client(vec!["https://app.example.com/cb".to_string()], None)
        .await
        .unwrap();
    // A different port/path than what was registered must be rejected —
    // registered clients get exact match only, no loopback-any-port
    // exception (T7 decision).
    let err = validate_authorize_params(
        &auth,
        Some("code"),
        Some(&row.id),
        Some("https://app.example.com/other-path"),
        None,
        Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"),
        Some("S256"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "invalid_request");
}

#[test]
fn consent_page_escapes_hostile_state_and_client_id() {
    let params = ValidParams {
        client_id: "<script>alert(1)</script>".to_string(),
        redirect_uri: "http://127.0.0.1:1/cb".to_string(),
        state: "\"><script>alert(2)</script>".to_string(),
        code_challenge: "c".to_string(),
    };
    let html = render_consent_page(&params, "", None, None).0;
    assert!(
        !html.contains("<script>"),
        "raw script tag must never appear: {html}"
    );
}

#[test]
fn append_query_preserves_existing_query_string() {
    let out = append_query(
        "http://127.0.0.1:1234/callback?foo=bar",
        &[("code", "abc"), ("state", "xyz")],
    );
    assert!(out.contains("foo=bar"));
    assert!(out.contains("code=abc"));
    assert!(out.contains("state=xyz"));
}
