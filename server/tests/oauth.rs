//! Integration tests for the T4 OAuth2 authorization-code + PKCE surface
//! (`GET/POST /authorize`, `POST /token`, `POST /revoke`) via
//! `tower::ServiceExt::oneshot` against the full `build_router` router — the
//! same router the daemon serves, auth layer included.

mod common;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use localdb_core::auth::{AuthCodeRow, AuthStore as _, Role};
use tower::ServiceExt;

use common::{json_body, make_enforced_app, request_with_bearer};

const CLIENT_ID: &str = "localdb-cli";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/callback";

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

async fn seed_admin_with_key(state: &server::AppState, name: &str) -> String {
    let user = state.auth().create_user(name, Role::Admin).await.unwrap();
    state.auth().issue_api_key(&user.id).await.unwrap().secret
}

fn authorize_query(state_param: &str, challenge: &str, redirect_uri: &str) -> String {
    format!(
        "/authorize?{}",
        form_encode(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect_uri),
            ("state", state_param),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ])
    )
}

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Drive the consent flow with a valid credential and follow the resulting
/// 302 far enough to pull `code` and `state` back out of `Location`.
async fn do_authorize(
    app: Router,
    redirect_uri: &str,
    state_param: &str,
    challenge: &str,
    credential: &str,
) -> (String, String) {
    let resp = post_form(
        app,
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect_uri),
            ("state", state_param),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("credential", credential),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER, "expected a redirect");
    let location = resp
        .headers()
        .get("location")
        .expect("redirect must carry a Location header")
        .to_str()
        .unwrap()
        .to_string();
    let (_, query) = location
        .split_once('?')
        .expect("redirect Location must carry a query string");
    let code = extract_query_param(query, "code").expect("Location must carry `code`");
    let got_state = extract_query_param(query, "state").unwrap_or_default();
    (code, got_state)
}

// ---------------------------------------------------------------------
// GET /authorize
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_authorize_renders_form_and_echoes_params() {
    let (_dir, _state, app) = make_enforced_app().await;
    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let uri = authorize_query("my-csrf-state", &challenge, REDIRECT_URI);

    let resp = get(app, &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp.into_body()).await;
    assert!(
        body.contains("my-csrf-state"),
        "state must be echoed: {body}"
    );
    assert!(
        body.contains(REDIRECT_URI),
        "redirect_uri must be echoed: {body}"
    );
    assert!(body.contains(CLIENT_ID), "client_id must be echoed: {body}");
}

#[tokio::test]
async fn non_loopback_redirect_uri_is_rejected_without_redirecting() {
    let (_dir, _state, app) = make_enforced_app().await;
    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let uri = authorize_query("s", &challenge, "http://evil.example.com/callback");

    let resp = get(app, &uri).await;

    assert!(
        !resp.status().is_redirection(),
        "must never redirect to a disallowed redirect_uri; got {}",
        resp.status()
    );
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_pkce_is_rejected() {
    let (_dir, _state, app) = make_enforced_app().await;
    let uri = format!(
        "/authorize?{}",
        form_encode(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("state", "s"),
        ])
    );

    let resp = get(app, &uri).await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn consent_page_escapes_hostile_state_param() {
    let (_dir, _state, app) = make_enforced_app().await;
    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let hostile_state = "<script>alert(1)</script>";
    let uri = authorize_query(hostile_state, &challenge, REDIRECT_URI);

    let resp = get(app, &uri).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp.into_body()).await;
    assert!(
        !body.contains("<script>"),
        "hostile state must not be echoed raw into the HTML: {body}"
    );
}

// ---------------------------------------------------------------------
// Full authorization_code + PKCE flow
// ---------------------------------------------------------------------

#[tokio::test]
async fn full_auth_code_pkce_flow_happy_path() {
    let (_dir, state, app) = make_enforced_app().await;
    let secret = seed_admin_with_key(&state, "alice").await;
    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();

    let (code, got_state) =
        do_authorize(app.clone(), REDIRECT_URI, "csrf-xyz", &challenge, &secret).await;
    assert_eq!(got_state, "csrf-xyz", "state must round-trip untouched");

    let resp = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert!(body["access_token"].as_str().unwrap().starts_with("ldb_"));
    assert!(body["refresh_token"].as_str().unwrap().starts_with("ldb_"));
    assert_eq!(body["token_type"], "Bearer");
    assert!(body["expires_in"].as_i64().unwrap() > 0);

    // Replayed code must fail — single-use.
    let replay = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    )
    .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let replay_body = json_body(replay.into_body()).await;
    assert_eq!(replay_body["error"], "invalid_grant");

    // The minted access token actually authenticates.
    let access_token = body["access_token"].as_str().unwrap().to_string();
    let me = request_with_bearer(app, Method::GET, "/v1/auth/me", None, Some(&access_token)).await;
    assert_eq!(me.status(), StatusCode::OK);
    let me_body = json_body(me.into_body()).await;
    assert_eq!(me_body["name"], "alice");
}

#[tokio::test]
async fn token_exchange_wrong_verifier_is_invalid_grant() {
    let (_dir, state, app) = make_enforced_app().await;
    let secret = seed_admin_with_key(&state, "bob").await;
    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let (code, _) = do_authorize(app.clone(), REDIRECT_URI, "s1", &challenge, &secret).await;

    let resp = post_form(
        app,
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", "totally-wrong-verifier"),
        ],
    )
    .await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn redirect_uri_mismatch_at_token_is_invalid_grant() {
    let (_dir, state, app) = make_enforced_app().await;
    let secret = seed_admin_with_key(&state, "dave").await;
    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let (code, _) = do_authorize(app.clone(), REDIRECT_URI, "s", &challenge, &secret).await;

    let resp = post_form(
        app,
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", "http://127.0.0.1:1/different"),
            ("client_id", CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    )
    .await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn expired_authorization_code_is_invalid_grant() {
    let (_dir, state, app) = make_enforced_app().await;
    let user = state
        .auth()
        .create_user("carol", Role::Admin)
        .await
        .unwrap();
    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let minted = localdb_core::auth::mint_secret();
    let row = AuthCodeRow {
        id: "code-expired-1".to_string(),
        client_id: CLIENT_ID.to_string(),
        user_id: user.id.clone(),
        code_hash: minted.hash.clone(),
        code_challenge: challenge,
        code_challenge_method: "S256".to_string(),
        redirect_uri: REDIRECT_URI.to_string(),
        expires_at: localdb_core::auth::rfc3339_from_now(-10),
        consumed_at: None,
        created_at: localdb_core::auth::rfc3339_from_now(-700),
    };
    state.auth_store().create_auth_code(&row).await.unwrap();

    let resp = post_form(
        app,
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &minted.secret),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    )
    .await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["error"], "invalid_grant");
}

// ---------------------------------------------------------------------
// One-time setup code bootstrap (D3b)
// ---------------------------------------------------------------------

#[tokio::test]
async fn setup_code_bootstraps_first_admin_and_is_single_use() {
    let (_dir, state, app) = make_enforced_app().await;
    assert_eq!(state.auth_store().count_users().await.unwrap(), 0);
    let setup_code = server::auth::generate_setup_code_if_needed(&state)
        .await
        .unwrap()
        .expect("zero users must yield a setup code");

    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let (code, _) = do_authorize(app.clone(), REDIRECT_URI, "s", &challenge, &setup_code).await;

    let users = state.auth_store().list_users().await.unwrap();
    assert_eq!(
        users.len(),
        1,
        "the setup code must create exactly one user"
    );
    assert_eq!(users[0].role, Role::Admin);

    let resp = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Second use of the same setup code must not succeed.
    let (_verifier2, challenge2) = localdb_core::auth::generate_pkce_pair();
    let second = post_form(
        app,
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("state", "s2"),
            ("code_challenge", &challenge2),
            ("code_challenge_method", "S256"),
            ("credential", &setup_code),
        ],
    )
    .await;
    assert!(
        !second.status().is_redirection(),
        "the setup code must not be redeemable a second time; got {}",
        second.status()
    );
}

/// Finding #5 regression: a first user created without `--admin` (a plain
/// `Role::Member`) must not block the setup-code bootstrap. Before the fix,
/// `generate_setup_code_if_needed` suppressed the code whenever
/// `count_users() > 0` (member included), and `resolve_credential`'s
/// defense-in-depth re-check made the same mistake — so even if a code had
/// been minted, redeeming it against a member-only instance would fail with
/// "an admin account already exists". Both must now key off admin
/// existence, not mere user existence.
#[tokio::test]
async fn setup_code_still_bootstraps_first_admin_when_only_members_exist() {
    let (_dir, state, app) = make_enforced_app().await;
    state.auth().create_user("bob", Role::Member).await.unwrap();

    let setup_code = server::auth::generate_setup_code_if_needed(&state)
        .await
        .unwrap()
        .expect("a member-only instance must still yield a setup code");

    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let (_code, _) = do_authorize(app, REDIRECT_URI, "s", &challenge, &setup_code).await;

    let users = state.auth_store().list_users().await.unwrap();
    assert_eq!(users.len(), 2, "bob plus the newly minted admin");
    assert!(
        users.iter().any(|u| u.role == Role::Admin),
        "the setup code must still create an admin: {users:?}"
    );
}

// ---------------------------------------------------------------------
// POST /revoke (RFC 7009)
// ---------------------------------------------------------------------

#[tokio::test]
async fn revoke_makes_refresh_unusable_and_unknown_token_still_returns_200() {
    let (_dir, state, app) = make_enforced_app().await;
    let user = state.auth().create_user("erin", Role::Admin).await.unwrap();
    let issued = state.auth().issue_refresh_token(&user.id).await.unwrap();

    let resp = post_form(app.clone(), "/revoke", &[("token", issued.secret.as_str())]).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let refresh_attempt = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", issued.secret.as_str()),
        ],
    )
    .await;
    assert_eq!(refresh_attempt.status(), StatusCode::BAD_REQUEST);

    let unknown = post_form(app, "/revoke", &[("token", "ldb_totally-unknown-token")]).await;
    assert_eq!(
        unknown.status(),
        StatusCode::OK,
        "revoking an unknown token must still answer 200 (RFC 7009 §2.2)"
    );
}

// ---------------------------------------------------------------------
// Refresh grant rotation + reuse detection
// ---------------------------------------------------------------------

#[tokio::test]
async fn refresh_grant_rotates_and_reuse_revokes_family() {
    let (_dir, state, app) = make_enforced_app().await;
    let user = state
        .auth()
        .create_user("frank", Role::Admin)
        .await
        .unwrap();
    let issued = state.auth().issue_refresh_token(&user.id).await.unwrap();

    let resp = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", issued.secret.as_str()),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let new_refresh = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(new_refresh, issued.secret);

    // Reuse of the OLD (rotated-away) refresh secret is theft — must fail
    // and burn the whole family, including the freshly rotated replacement.
    let reuse = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", issued.secret.as_str()),
        ],
    )
    .await;
    assert_eq!(reuse.status(), StatusCode::BAD_REQUEST);

    let new_attempt = post_form(
        app,
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &new_refresh),
        ],
    )
    .await;
    assert_eq!(
        new_attempt.status(),
        StatusCode::BAD_REQUEST,
        "reuse detection must revoke the whole family, including the newer replacement"
    );
}

// ---------------------------------------------------------------------
// Public-route check
// ---------------------------------------------------------------------

#[tokio::test]
async fn public_oauth_routes_reachable_without_bearer_but_v1_still_401() {
    let (_dir, _state, app) = make_enforced_app().await;
    let (_verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let uri = authorize_query("s", &challenge, REDIRECT_URI);

    let authorize_resp = get(app.clone(), &uri).await;
    assert_eq!(authorize_resp.status(), StatusCode::OK);

    let token_resp = post_form(
        app.clone(),
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", "ldb_nope"),
        ],
    )
    .await;
    assert_ne!(token_resp.status(), StatusCode::UNAUTHORIZED);

    let revoke_resp = post_form(app.clone(), "/revoke", &[("token", "ldb_nope")]).await;
    assert_eq!(revoke_resp.status(), StatusCode::OK);

    let v1_resp = get(app, "/v1/status").await;
    assert_eq!(
        v1_resp.status(),
        StatusCode::UNAUTHORIZED,
        "non-oauth /v1 routes must still require a bearer token"
    );
}
