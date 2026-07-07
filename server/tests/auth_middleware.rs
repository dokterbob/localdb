//! Integration tests for the T3 auth middleware (specs/05-surfaces.md §3.1)
//! via `tower::ServiceExt::oneshot` against the full `build_router` router —
//! the same router the daemon serves, auth layer and `/mcp` mount included.

mod common;

use axum::http::{header, Method, StatusCode};
use localdb_core::auth::Role;
use server::AuthMode;

use common::{json_body, make_app_with_mode, make_enforced_app, request, request_with_bearer};

/// Seed a user and mint an API key through the state's own AuthService —
/// the same persistent database the router serves.
async fn seed_user_with_key(state: &server::AppState, name: &str, role: Role) -> String {
    let user = state
        .auth()
        .create_user(name, role)
        .await
        .expect("seeding a user must succeed");
    state
        .auth()
        .issue_api_key(&user.id)
        .await
        .expect("minting an api key must succeed")
        .secret
}

#[tokio::test]
async fn enforced_no_header_is_401_with_www_authenticate_and_envelope() {
    let (_dir, _state, app) = make_enforced_app().await;

    let resp = request(app, Method::GET, "/v1/status", None).await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer"),
        "D6: 401 must carry WWW-Authenticate: Bearer"
    );
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "unauthorized", "standard error envelope");
    assert!(body["message"].is_string());
}

#[tokio::test]
async fn enforced_garbage_token_is_401() {
    let (_dir, _state, app) = make_enforced_app().await;

    let resp = request_with_bearer(
        app,
        Method::GET,
        "/v1/status",
        None,
        Some("ldb_definitely-not-a-real-token"),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "unauthorized");
}

#[tokio::test]
async fn enforced_valid_admin_key_reaches_auth_me_with_identity() {
    let (_dir, state, app) = make_enforced_app().await;
    let secret = seed_user_with_key(&state, "alice", Role::Admin).await;

    let resp = request_with_bearer(app, Method::GET, "/v1/auth/me", None, Some(&secret)).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["name"], "alice");
    assert_eq!(body["role"], "admin");
    assert_eq!(body["store_access"], "all");
    assert!(body["user_id"].is_string());
}

#[tokio::test]
async fn enforced_member_key_is_403_wholesale() {
    // T3 interim policy: members are rejected on every protected route;
    // store-grant-scoped access activates in T5.
    let (_dir, state, app) = make_enforced_app().await;
    let secret = seed_user_with_key(&state, "bob", Role::Member).await;

    let resp =
        request_with_bearer(app.clone(), Method::GET, "/v1/auth/me", None, Some(&secret)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "forbidden");

    // Not just /v1/auth/me — any protected route.
    let resp = request_with_bearer(app, Method::GET, "/v1/stores", None, Some(&secret)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn enforced_mcp_without_header_is_401() {
    // The auth layer is applied after the `/mcp` nest_service, so it wraps
    // the MCP mount too — an unauthenticated POST never reaches rmcp.
    let (_dir, _state, app) = make_enforced_app().await;

    let resp = request_with_bearer(
        app,
        Method::POST,
        "/mcp",
        Some(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "t", "version": "0"}
            }
        })),
        None,
    )
    .await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
}

#[tokio::test]
async fn open_mode_needs_no_header_and_me_returns_local_trust() {
    let (_dir, _state, app) = make_app_with_mode(AuthMode::Open).await;

    let status = request(app.clone(), Method::GET, "/v1/status", None).await;
    assert_eq!(status.status(), StatusCode::OK);

    let me = request(app, Method::GET, "/v1/auth/me", None).await;
    assert_eq!(me.status(), StatusCode::OK);
    let body = json_body(me.into_body()).await;
    assert_eq!(body["user_id"], "local");
    assert_eq!(body["name"], "local");
    assert_eq!(body["role"], "admin");
    assert_eq!(body["store_access"], "all");
}

#[tokio::test]
async fn enforced_admin_key_can_use_regular_routes() {
    // Beyond /v1/auth/me: an authenticated admin passes everywhere.
    let (_dir, state, app) = make_enforced_app().await;
    let secret = seed_user_with_key(&state, "carol", Role::Admin).await;

    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/stores",
        Some(serde_json::json!({ "name": "docs" })),
        Some(&secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = request_with_bearer(app, Method::GET, "/v1/stores", None, Some(&secret)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn enforced_revoked_key_is_401_again() {
    // The middleware consults the live AuthService on every request: a key
    // revoked after successful use stops working immediately.
    let (_dir, state, app) = make_enforced_app().await;
    let user = state.auth().create_user("dave", Role::Admin).await.unwrap();
    let issued = state.auth().issue_api_key(&user.id).await.unwrap();

    let ok = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/status",
        None,
        Some(&issued.secret),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::OK);

    use localdb_core::auth::AuthStore as _;
    state
        .auth_store()
        .revoke_token(&issued.row.id)
        .await
        .unwrap();

    let denied =
        request_with_bearer(app, Method::GET, "/v1/status", None, Some(&issued.secret)).await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
}
