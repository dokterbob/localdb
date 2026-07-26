//! Integration tests for T7: RFC 9728 protected-resource metadata, RFC 8414
//! authorization-server metadata, RFC 7591 Dynamic Client Registration, and
//! the `WWW-Authenticate: Bearer resource_metadata="..."` 401 upgrade — all
//! via `tower::ServiceExt::oneshot` against the full `build_router` router,
//! the same one the daemon serves, auth layer included.

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use tower::ServiceExt;

use common::{json_body, make_app_with_mode_and_public_url, make_enforced_app};
use server::AuthMode;

async fn get(app: Router, uri: &str, host: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(h) = host {
        builder = builder.header("host", h);
    }
    app.oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn post_json(app: Router, uri: &str, body: serde_json::Value) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
    .unwrap()
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

async fn post_form(app: Router, uri: &str, pairs: &[(&str, &str)]) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form_encode(pairs)))
            .unwrap(),
    )
    .await
    .unwrap()
}

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

// ---------------------------------------------------------------------
// GET /.well-known/oauth-protected-resource (RFC 9728)
// ---------------------------------------------------------------------

#[tokio::test]
async fn protected_resource_metadata_reachable_without_bearer_and_has_pinned_shape() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(
        app,
        "/.well-known/oauth-protected-resource",
        Some("127.0.0.1:7700"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(
        body,
        serde_json::json!({
            "resource": "http://127.0.0.1:7700",
            "authorization_servers": ["http://127.0.0.1:7700"],
            "bearer_methods_supported": ["header"],
        })
    );
}

#[tokio::test]
async fn authorization_server_metadata_reachable_without_bearer_and_has_pinned_shape() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(
        app,
        "/.well-known/oauth-authorization-server",
        Some("127.0.0.1:7700"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(
        body,
        serde_json::json!({
            "issuer": "http://127.0.0.1:7700",
            "authorization_endpoint": "http://127.0.0.1:7700/authorize",
            "token_endpoint": "http://127.0.0.1:7700/token",
            "revocation_endpoint": "http://127.0.0.1:7700/revoke",
            "registration_endpoint": "http://127.0.0.1:7700/register",
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "revocation_endpoint_auth_methods_supported": ["none"],
        })
    );
}

#[tokio::test]
async fn well_known_endpoints_use_configured_public_url_over_host_header() {
    let (_dir, _state, app) =
        make_app_with_mode_and_public_url(AuthMode::Enforced, Some("https://localdb.example.com/"))
            .await;
    let resp = get(
        app,
        "/.well-known/oauth-protected-resource",
        Some("127.0.0.1:9999"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["resource"], "https://localdb.example.com");
    assert_eq!(
        body["authorization_servers"][0],
        "https://localdb.example.com"
    );
}

#[tokio::test]
async fn well_known_endpoints_derive_base_from_host_header_when_no_public_url() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(
        app,
        "/.well-known/oauth-authorization-server",
        Some("192.168.1.50:8080"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["issuer"], "http://192.168.1.50:8080");
}

#[tokio::test]
async fn hostile_host_header_is_rejected_and_never_echoed() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(
        app,
        "/.well-known/oauth-protected-resource",
        Some("evil.com/../../etc/passwd"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    let text = body.to_string();
    assert!(
        !text.contains("evil.com"),
        "the hostile Host header must never be echoed back unsanitized: {text}"
    );
}

#[tokio::test]
async fn missing_host_header_and_no_public_url_is_rejected() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(app, "/.well-known/oauth-protected-resource", None).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------
// WWW-Authenticate: Bearer resource_metadata="..." on 401s
// ---------------------------------------------------------------------

#[tokio::test]
async fn v1_401_carries_resource_metadata_challenge() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(app, "/v1/status", Some("127.0.0.1:7700")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .expect("401 must carry WWW-Authenticate")
        .to_str()
        .unwrap();
    assert_eq!(
        www_auth,
        r#"Bearer resource_metadata="http://127.0.0.1:7700/.well-known/oauth-protected-resource""#
    );
}

#[tokio::test]
async fn mcp_401_carries_resource_metadata_challenge() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(app, "/mcp", Some("127.0.0.1:7700")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .expect("401 must carry WWW-Authenticate")
        .to_str()
        .unwrap();
    assert!(
        www_auth.contains(
            r#"resource_metadata="http://127.0.0.1:7700/.well-known/oauth-protected-resource""#
        ),
        "got: {www_auth}"
    );
}

#[tokio::test]
async fn invalid_bearer_401_also_carries_resource_metadata_challenge() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/status")
                .header("host", "127.0.0.1:7700")
                .header("authorization", "Bearer ldb_totally-invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(www_auth.contains("resource_metadata="));
}

#[tokio::test]
async fn no_public_url_and_no_host_header_falls_back_to_plain_bearer_challenge() {
    // The 401 itself must never be suppressed just because the discovery
    // hint couldn't be resolved.
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = get(app, "/v1/status", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(www_auth, "Bearer");
}

// ---------------------------------------------------------------------
// POST /register (RFC 7591 Dynamic Client Registration)
// ---------------------------------------------------------------------

#[tokio::test]
async fn register_happy_path_returns_201_no_secret() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({
            "redirect_uris": ["http://127.0.0.1:54321/callback"],
            "client_name": "Test MCP Client",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert!(body["client_id"].as_str().is_some());
    assert_eq!(body["client_name"], "Test MCP Client");
    assert_eq!(
        body["redirect_uris"],
        serde_json::json!(["http://127.0.0.1:54321/callback"])
    );
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert!(
        body.get("client_secret").is_none(),
        "a public client must never receive a client_secret"
    );
}

#[tokio::test]
async fn register_accepts_https_redirect_uri() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({"redirect_uris": ["https://app.example.com/callback"]}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn register_rejects_non_loopback_http_redirect_uri() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({"redirect_uris": ["http://evil.com/cb"]}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    // finding #10: RFC 7591 §3.2.2 shape — an `error` member, no `code`.
    assert_eq!(body["error"], "invalid_redirect_uri");
    assert!(body.get("code").is_none());
}

#[tokio::test]
async fn register_rejects_empty_redirect_uris() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(app, "/register", serde_json::json!({"redirect_uris": []})).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_client_metadata");
    assert!(body.get("code").is_none());
}

#[tokio::test]
async fn register_rejects_non_none_token_endpoint_auth_method() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({
            "redirect_uris": ["http://127.0.0.1:1234/cb"],
            "token_endpoint_auth_method": "client_secret_basic",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_client_metadata");
    assert!(body.get("code").is_none());
}

// ---------------------------------------------------------------------
// POST /register DCR bounds (finding #8, RFC-shaped per finding #10)
// ---------------------------------------------------------------------

#[tokio::test]
async fn register_rejects_too_many_redirect_uris() {
    let (_dir, _state, app) = make_enforced_app().await;
    // MAX_REGISTRATION_REDIRECT_URIS is 5 — 6 entries must be rejected.
    let redirect_uris: Vec<String> = (0..6)
        .map(|i| format!("https://app.example.com/cb{i}"))
        .collect();
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({"redirect_uris": redirect_uris}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_client_metadata");
    assert!(body.get("code").is_none());
}

#[tokio::test]
async fn register_rejects_over_long_redirect_uri() {
    let (_dir, _state, app) = make_enforced_app().await;
    // MAX_REGISTRATION_REDIRECT_URI_LEN is 2048 — pad the path past that.
    let long_uri = format!("https://app.example.com/{}", "a".repeat(2048));
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({"redirect_uris": [long_uri]}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_client_metadata");
    assert!(body.get("code").is_none());
}

#[tokio::test]
async fn register_rejects_over_long_client_name() {
    let (_dir, _state, app) = make_enforced_app().await;
    // MAX_REGISTRATION_CLIENT_NAME_LEN is 256.
    let long_name = "a".repeat(257);
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({
            "redirect_uris": ["https://app.example.com/cb"],
            "client_name": long_name,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_client_metadata");
    assert!(body.get("code").is_none());
}

#[tokio::test]
async fn register_accepts_normal_registration_within_bounds() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({
            "redirect_uris": ["https://app.example.com/cb"],
            "client_name": "A Normal Client",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn register_is_reachable_without_a_bearer_token() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({"redirect_uris": ["http://127.0.0.1:1/cb"]}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ---------------------------------------------------------------------
// POST /register grant_types/response_types validation (finding #3): an
// unsupported entry must not be echoed back in a 201 — this AS only ever
// supports authorization_code + refresh_token grants and the code response
// type (matches discovery::oauth_authorization_server's
// grant_types_supported/response_types_supported, single source of truth in
// core::auth::client::SUPPORTED_GRANT_TYPES/SUPPORTED_RESPONSE_TYPES).
// ---------------------------------------------------------------------

#[tokio::test]
async fn register_rejects_unsupported_grant_type() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({
            "redirect_uris": ["https://app.example.com/cb"],
            "grant_types": ["client_credentials"],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_client_metadata");
    assert!(body.get("code").is_none());
}

#[tokio::test]
async fn register_rejects_unsupported_response_type() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({
            "redirect_uris": ["https://app.example.com/cb"],
            "response_types": ["token"],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_client_metadata");
    assert!(body.get("code").is_none());
}

#[tokio::test]
async fn register_accepts_explicit_supported_grant_and_response_types() {
    let (_dir, _state, app) = make_enforced_app().await;
    let resp = post_json(
        app,
        "/register",
        serde_json::json!({
            "redirect_uris": ["https://app.example.com/cb"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert_eq!(
        body["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(body["response_types"], serde_json::json!(["code"]));
}

// ---------------------------------------------------------------------
// Full DCR -> code+PKCE -> token flow, as a registered (non-built-in) client
// ---------------------------------------------------------------------

#[tokio::test]
async fn registered_client_completes_full_code_pkce_flow() {
    let (_dir, state, app) = make_enforced_app().await;
    let user = state
        .auth()
        .create_user("dcr-user", localdb_core::auth::Role::Admin)
        .await
        .unwrap();
    let api_key = state.auth().issue_api_key(&user.id).await.unwrap().secret;

    // 1. Register a client with a loopback redirect.
    let register_resp = post_json(
        app.clone(),
        "/register",
        serde_json::json!({"redirect_uris": ["http://127.0.0.1:44556/cb"]}),
    )
    .await;
    assert_eq!(register_resp.status(), StatusCode::CREATED);
    let register_body = json_body(register_resp.into_body()).await;
    let client_id = register_body["client_id"].as_str().unwrap().to_string();

    // 2. Drive /authorize as that client with a valid API key credential.
    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let authorize_resp = post_form(
        app.clone(),
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", "http://127.0.0.1:44556/cb"),
            ("state", "xyz-state"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("credential", &api_key),
        ],
    )
    .await;
    assert_eq!(
        authorize_resp.status(),
        StatusCode::SEE_OTHER,
        "expected a redirect carrying the authorization code"
    );
    let location = authorize_resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let (_, query) = location.split_once('?').unwrap();
    let code = extract_query_param(query, "code").expect("Location must carry `code`");

    // 3. Exchange the code for tokens.
    let token_resp = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", "http://127.0.0.1:44556/cb"),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
        ],
    )
    .await;
    assert_eq!(token_resp.status(), StatusCode::OK);
    let token_body = json_body(token_resp.into_body()).await;
    assert!(token_body["access_token"]
        .as_str()
        .unwrap()
        .starts_with("ldb_"));
}

#[tokio::test]
async fn unknown_client_id_at_authorize_is_still_rejected() {
    let (_dir, _state, app) = make_enforced_app().await;
    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let resp = post_form(
        app,
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", "never-registered-client"),
            ("redirect_uri", "http://127.0.0.1:1/cb"),
            ("state", "s"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("credential", "irrelevant"),
        ],
    )
    .await;
    assert!(
        !resp.status().is_redirection(),
        "an unknown client_id must never redirect"
    );
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn registered_client_redirect_must_exact_match_not_loopback_any_port() {
    let (_dir, _state, app) = make_enforced_app().await;
    let register_resp = post_json(
        app.clone(),
        "/register",
        serde_json::json!({"redirect_uris": ["http://127.0.0.1:44556/cb"]}),
    )
    .await;
    let register_body = json_body(register_resp.into_body()).await;
    let client_id = register_body["client_id"].as_str().unwrap().to_string();

    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    // Different port than registered — must be rejected (no loopback-any-port
    // exception for registered clients, unlike the built-in localdb-cli).
    let resp = post_form(
        app,
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", "http://127.0.0.1:9999/cb"),
            ("state", "s"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("credential", "irrelevant"),
        ],
    )
    .await;
    assert!(!resp.status().is_redirection());
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
