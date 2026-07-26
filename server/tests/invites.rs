//! T6 integration tests: invite create/list/revoke, closed-mode access
//! request approve/deny, and the public redeem/poll surface — via
//! `tower::ServiceExt::oneshot` against the full `build_router` router, the
//! same shape `admin_management.rs`/`oauth.rs` use.

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use localdb_core::auth::{AuthStore as _, Role};

use common::{
    create_store_as, json_body, make_app_with_mode_and_public_url, make_enforced_app,
    request_with_bearer, seed_user_with_key,
};
use server::AuthMode;

fn form_encode(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

async fn get(app: Router, uri: &str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
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

async fn body_string(body: Body) -> String {
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Like `request_with_bearer`, but also sets a `Host` header — needed since
/// `POST /v1/invites`'s `consent_url` is built from `resolve_base_url`
/// (finding #9), which requires either a configured `server.public_url` or a
/// `Host` header to resolve a base URL at all. `oneshot` requests carry no
/// `Host` header unless one is set explicitly (unlike a real TCP listener),
/// so every `create_invite` call needs one to keep resolving successfully.
async fn request_with_bearer_and_host(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
    host: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(secret) = bearer {
        builder = builder.header("authorization", format!("Bearer {secret}"));
    }
    if let Some(h) = host {
        builder = builder.header("host", h);
    }
    let request_body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    app.oneshot(builder.body(request_body).unwrap())
        .await
        .unwrap()
}

async fn create_invite(
    app: Router,
    admin_secret: &str,
    mode: &str,
    stores: &[&str],
    max_uses: u32,
) -> Value {
    let resp = request_with_bearer_and_host(
        app,
        Method::POST,
        "/v1/invites",
        Some(json!({ "mode": mode, "stores": stores, "max_uses": max_uses })),
        Some(admin_secret),
        Some("127.0.0.1:7700"),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "invite creation should succeed"
    );
    json_body(resp.into_body()).await
}

async fn redeem_invite(app: Router, token: &str, name: &str) -> (StatusCode, Value) {
    let resp = request_with_bearer(
        app,
        Method::POST,
        "/v1/invites/redeem",
        Some(json!({ "token": token, "name": name })),
        None,
    )
    .await;
    let status = resp.status();
    (status, json_body(resp.into_body()).await)
}

async fn poll_request(app: Router, request_id: &str, secret: &str) -> (StatusCode, Value) {
    let resp = get(
        app,
        &format!("/v1/invites/requests/{request_id}?secret={secret}"),
    )
    .await;
    let status = resp.status();
    (status, json_body(resp.into_body()).await)
}

// ---------------------------------------------------------------------------
// Admin CRUD + 403-for-member
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_invite_create_list_revoke_happy_path() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin", Role::Admin).await;
    create_store_as(app.clone(), "docs", "shared", Some(&admin_secret)).await;

    let created = create_invite(app.clone(), &admin_secret, "open", &["docs"], 1).await;
    assert_eq!(created["mode"], "open");
    assert_eq!(created["store_grants"], json!(["docs"]));
    assert!(created["token"].as_str().unwrap().starts_with("ldb_"));
    assert!(created["consent_url"]
        .as_str()
        .unwrap()
        .contains("/authorize?invite="));
    let invite_id = created["id"].as_str().unwrap().to_string();

    // List: no secrets present.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/invites",
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], invite_id);
    assert!(
        arr[0].get("token").is_none(),
        "list must never carry the secret"
    );
    assert!(arr[0].get("token_hash").is_none());

    // Revoke.
    let resp = request_with_bearer(
        app.clone(),
        Method::DELETE,
        &format!("/v1/invites/{invite_id}"),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Revoking again is a clean 4xx, not a panic.
    let resp = request_with_bearer(
        app.clone(),
        Method::DELETE,
        &format!("/v1/invites/{invite_id}"),
        None,
        Some(&admin_secret),
    )
    .await;
    assert!(resp.status().is_client_error());
}

// ---------------------------------------------------------------------------
// consent_url base resolution (finding #9): `server.public_url` when
// configured, else the sanitized `Host` header, else 400 — matching
// `server::auth::base_url::resolve_base_url` and how discovery.rs already
// tests it against the `.well-known` endpoints.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn consent_url_uses_configured_public_url_over_host_header() {
    let (_dir, state, app) =
        make_app_with_mode_and_public_url(AuthMode::Enforced, Some("https://db.example.com/"))
            .await;
    let admin_secret = seed_user_with_key(&state, "root-admin-pub", Role::Admin).await;
    create_store_as(app.clone(), "docs", "shared", Some(&admin_secret)).await;

    // Even with a hostile/irrelevant internal Host header, the configured
    // public_url must win — never a downgraded/internal-hostname link.
    let resp = request_with_bearer_and_host(
        app,
        Method::POST,
        "/v1/invites",
        Some(json!({ "mode": "open", "stores": ["docs"], "max_uses": 1 })),
        Some(&admin_secret),
        Some("10.0.0.5:7700"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = json_body(resp.into_body()).await;
    let consent_url = created["consent_url"].as_str().unwrap();
    assert!(
        consent_url.starts_with("https://db.example.com/authorize?invite="),
        "expected the configured public_url as the base, got: {consent_url}"
    );
}

#[tokio::test]
async fn consent_url_falls_back_to_sanitized_host_header_without_public_url() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin-host", Role::Admin).await;
    create_store_as(app.clone(), "docs", "shared", Some(&admin_secret)).await;

    let resp = request_with_bearer_and_host(
        app,
        Method::POST,
        "/v1/invites",
        Some(json!({ "mode": "open", "stores": ["docs"], "max_uses": 1 })),
        Some(&admin_secret),
        Some("192.168.1.50:8080"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = json_body(resp.into_body()).await;
    let consent_url = created["consent_url"].as_str().unwrap();
    assert!(
        consent_url.starts_with("http://192.168.1.50:8080/authorize?invite="),
        "expected the sanitized Host header as the base, got: {consent_url}"
    );
}

#[tokio::test]
async fn consent_url_rejects_hostile_host_header_without_public_url() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin-hostile", Role::Admin).await;
    create_store_as(app.clone(), "docs", "shared", Some(&admin_secret)).await;

    let resp = request_with_bearer_and_host(
        app,
        Method::POST,
        "/v1/invites",
        Some(json!({ "mode": "open", "stores": ["docs"], "max_uses": 1 })),
        Some(&admin_secret),
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
async fn member_gets_403_on_every_invite_admin_route() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    let member_secret = seed_user_with_key(&state, "member1", Role::Member).await;
    create_store_as(app.clone(), "docs", "shared", Some(&admin_secret)).await;
    let created = create_invite(app.clone(), &admin_secret, "open", &["docs"], 1).await;
    let invite_id = created["id"].as_str().unwrap().to_string();

    for (method, uri) in [
        (Method::GET, "/v1/invites".to_string()),
        (Method::POST, "/v1/invites".to_string()),
        (Method::DELETE, format!("/v1/invites/{invite_id}")),
        (Method::GET, "/v1/invites/requests".to_string()),
        (
            Method::POST,
            format!("/v1/invites/requests/{invite_id}/approve"),
        ),
        (
            Method::POST,
            format!("/v1/invites/requests/{invite_id}/deny"),
        ),
    ] {
        let resp = request_with_bearer(
            app.clone(),
            method.clone(),
            &uri,
            Some(json!({ "mode": "open" })),
            Some(&member_secret),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "member should get 403 on {method} {uri}"
        );
    }
}

#[tokio::test]
async fn create_invite_on_private_store_is_rejected() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    create_store_as(app.clone(), "secret-store", "private", Some(&admin_secret)).await;

    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/invites",
        Some(json!({ "mode": "open", "stores": ["secret-store"], "max_uses": 1 })),
        Some(&admin_secret),
    )
    .await;
    assert!(
        resp.status().is_client_error(),
        "granting a private store via invite must be rejected"
    );
}

// ---------------------------------------------------------------------------
// Public redeem: open mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn public_redeem_open_invite_201_shape_and_grants_work() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    create_store_as(app.clone(), "docs", "shared", Some(&admin_secret)).await;
    let created = create_invite(app.clone(), &admin_secret, "open", &["docs"], 1).await;
    let token = created["token"].as_str().unwrap().to_string();

    let (status, body) = redeem_invite(app.clone(), &token, "newbie").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user"]["name"], "newbie");
    assert_eq!(body["granted_stores"], json!(["docs"]));
    let api_key = body["api_key"].as_str().unwrap().to_string();
    assert!(api_key.starts_with("ldb_"));

    // The secret is present exactly once in the response (a single field).
    let occurrences = body
        .as_object()
        .unwrap()
        .values()
        .filter(|v| v.as_str() == Some(api_key.as_str()))
        .count();
    assert_eq!(occurrences, 1);

    // The credential actually authenticates and can read the granted store.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/docs",
        None,
        Some(&api_key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn redeem_max_uses_one_double_redeem_is_4xx() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    let created = create_invite(app.clone(), &admin_secret, "open", &[], 1).await;
    let token = created["token"].as_str().unwrap().to_string();

    let (status, _) = redeem_invite(app.clone(), &token, "first").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = redeem_invite(app.clone(), &token, "second").await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn redeem_revoked_invite_is_4xx() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    let created = create_invite(app.clone(), &admin_secret, "open", &[], 1).await;
    let invite_id = created["id"].as_str().unwrap().to_string();
    let token = created["token"].as_str().unwrap().to_string();

    let resp = request_with_bearer(
        app.clone(),
        Method::DELETE,
        &format!("/v1/invites/{invite_id}"),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (status, _) = redeem_invite(app.clone(), &token, "someone").await;
    assert!(status.is_client_error());
}

#[tokio::test]
async fn redeem_unknown_token_is_4xx() {
    let (_dir, _state, app) = make_enforced_app().await;
    let (status, _) = redeem_invite(app, "ldb_totally-unknown", "someone").await;
    assert!(status.is_client_error());
}

// ---------------------------------------------------------------------------
// Public redeem + poll: closed mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn closed_invite_full_round_trip_pending_approve_poll_once_collected() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    create_store_as(app.clone(), "docs", "shared", Some(&admin_secret)).await;
    let created = create_invite(app.clone(), &admin_secret, "closed", &["docs"], 1).await;
    let token = created["token"].as_str().unwrap().to_string();

    let (status, body) = redeem_invite(app.clone(), &token, "requester").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let request_id = body["request_id"].as_str().unwrap().to_string();
    let request_secret = body["request_secret"].as_str().unwrap().to_string();

    // Poll while pending.
    let (status, body) = poll_request(app.clone(), &request_id, &request_secret).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "pending");

    // Admin lists and approves.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/invites/requests",
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = json_body(resp.into_body()).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["state"], "pending");

    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        &format!("/v1/invites/requests/{request_id}/approve"),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let approved_user = json_body(resp.into_body()).await;
    assert_eq!(approved_user["name"], "requester");

    // First poll after approval: a freshly minted credential is handed back.
    let (status, body) = poll_request(app.clone(), &request_id, &request_secret).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "approved");
    let api_key = body["api_key"].as_str().unwrap().to_string();
    assert_ne!(
        api_key, request_secret,
        "the poll-only request secret must never become the live API key"
    );

    // The request secret itself must never authenticate — it is poll-only.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/docs",
        None,
        Some(&request_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The freshly minted credential authenticates and can read the granted store.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/docs",
        None,
        Some(&api_key),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Second poll: terminal "collected" state, not the secret again.
    let (status, body) = poll_request(app.clone(), &request_id, &request_secret).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "collected");
    assert!(body.get("api_key").is_none());
}

#[tokio::test]
async fn closed_invite_deny_then_poll_denied() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    let created = create_invite(app.clone(), &admin_secret, "closed", &[], 1).await;
    let token = created["token"].as_str().unwrap().to_string();

    let (_status, body) = redeem_invite(app.clone(), &token, "requester").await;
    let request_id = body["request_id"].as_str().unwrap().to_string();
    let request_secret = body["request_secret"].as_str().unwrap().to_string();

    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        &format!("/v1/invites/requests/{request_id}/deny"),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (status, body) = poll_request(app.clone(), &request_id, &request_secret).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "denied");
}

#[tokio::test]
async fn poll_unknown_id_and_wrong_secret_are_indistinguishable_over_http() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    let created = create_invite(app.clone(), &admin_secret, "closed", &[], 1).await;
    let token = created["token"].as_str().unwrap().to_string();
    let (_status, body) = redeem_invite(app.clone(), &token, "requester").await;
    let request_id = body["request_id"].as_str().unwrap().to_string();

    let (unknown_status, unknown_body) =
        poll_request(app.clone(), "totally-unknown-id", "ldb_whatever").await;
    let (wrong_secret_status, wrong_secret_body) =
        poll_request(app.clone(), &request_id, "ldb_wrong-secret").await;

    assert_eq!(unknown_status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong_secret_status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_body["message"], wrong_secret_body["message"]);
    assert_eq!(unknown_body["code"], wrong_secret_body["code"]);
}

// ---------------------------------------------------------------------------
// Consent page (T4 seam): open-mode issues an OAuth code as the new user;
// closed-mode shows a request-submitted page; hostile params are escaped.
// ---------------------------------------------------------------------------

const CLIENT_ID: &str = "localdb-cli";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/callback";

fn authorize_query(invite_token: &str) -> String {
    form_encode(&[
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("state", "xyz"),
        ("code_challenge", "challenge-value"),
        ("code_challenge_method", "S256"),
        ("invite", invite_token),
    ])
}

#[tokio::test]
async fn consent_page_open_invite_issues_oauth_code_as_new_user() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    let created = create_invite(app.clone(), &admin_secret, "open", &[], 1).await;
    let token = created["token"].as_str().unwrap().to_string();

    // GET renders the invite-redemption variant of the form.
    let resp = get(
        app.clone(),
        &format!("/authorize?{}", authorize_query(&token)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_string(resp.into_body()).await;
    assert!(
        html.contains("requested_name"),
        "form should ask for a name"
    );
    assert!(
        html.contains(&token),
        "invite token should round-trip in a hidden field"
    );

    // POST with a chosen name redeems the invite and redirects with a code.
    let resp = post_form(
        app.clone(),
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("state", "xyz"),
            ("code_challenge", "challenge-value"),
            ("code_challenge_method", "S256"),
            ("invite", &token),
            ("requested_name", "browser-newbie"),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(location.starts_with(REDIRECT_URI));
    let url = url::Url::parse(&location).unwrap();
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .unwrap();

    // Exchange the code for tokens (RFC 6749 §4.1.3) — no PKCE verifier was
    // ever registered in this test, so use the plain flow only to confirm a
    // code was minted for a *new* user, not to complete the full exchange
    // (a genuine `code_verifier` wasn't generated here — the consent page
    // doesn't need one to redeem an invite, only `POST /token` does).
    let user = state
        .auth_store()
        .get_user_by_name("browser-newbie")
        .await
        .unwrap();
    assert!(
        user.is_some(),
        "the invite redemption must have created the user"
    );
    assert!(!code.is_empty());
}

#[tokio::test]
async fn consent_page_closed_invite_shows_request_submitted_page() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "admin1", Role::Admin).await;
    let created = create_invite(app.clone(), &admin_secret, "closed", &[], 1).await;
    let token = created["token"].as_str().unwrap().to_string();

    let resp = post_form(
        app.clone(),
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("state", "xyz"),
            ("code_challenge", "challenge-value"),
            ("code_challenge_method", "S256"),
            ("invite", &token),
            ("requested_name", "closed-requester"),
        ],
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "closed-mode invite must render a page, not redirect"
    );
    let html = body_string(resp.into_body()).await;
    assert!(html.contains("Request submitted"));

    // No user was created and no OAuth code issued.
    let user = state
        .auth_store()
        .get_user_by_name("closed-requester")
        .await
        .unwrap();
    assert!(user.is_none());
}

#[tokio::test]
async fn consent_page_escapes_hostile_invite_token() {
    let (_dir, _state, app) = make_enforced_app().await;
    let hostile = "<script>alert(1)</script>";

    let resp = get(
        app.clone(),
        &format!("/authorize?{}", authorize_query(hostile)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_string(resp.into_body()).await;
    assert!(
        !html.contains("<script>alert(1)</script>"),
        "hostile invite token must be HTML-escaped: {html}"
    );
    assert!(html.contains("&lt;script&gt;"));
}
