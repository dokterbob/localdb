//! T5 integration tests: admin-only user/key/grant management routes, the
//! last-admin lockout guard rails, and D7 member store-visibility scoping —
//! specs/05-surfaces.md §3.1. Built the same way as `auth_middleware.rs`:
//! `oneshot` against the full `build_router` router in enforced mode, with
//! real bearer tokens minted through the state's own `AuthService`.

mod common;

use axum::http::{Method, StatusCode};
use serde_json::json;

use localdb_core::auth::Role;

use common::{
    create_store_as, json_body, make_enforced_app, request_with_bearer, seed_user_with_key,
};

// ---------------------------------------------------------------------------
// Admin user CRUD happy path: create -> list -> set-role -> delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_user_create_list_set_role_delete_happy_path() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin", Role::Admin).await;

    // Create.
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/users",
        Some(json!({ "name": "grace", "role": "member" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["name"], "grace");
    assert_eq!(body["role"], "member");
    let user_id = body["id"].as_str().unwrap().to_string();

    // List.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/users",
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"grace"));
    assert!(names.contains(&"root-admin"));

    // Set role.
    let resp = request_with_bearer(
        app.clone(),
        Method::PATCH,
        &format!("/v1/users/{user_id}"),
        Some(json!({ "role": "admin" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["role"], "admin");

    // Delete.
    let resp = request_with_bearer(
        app.clone(),
        Method::DELETE,
        &format!("/v1/users/{user_id}"),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = request_with_bearer(app, Method::GET, "/v1/users", None, Some(&admin_secret)).await;
    let body = json_body(resp.into_body()).await;
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"grace"), "deleted user must not be listed");
}

#[tokio::test]
async fn create_user_rejects_unknown_role() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin2", Role::Admin).await;

    let resp = request_with_bearer(
        app,
        Method::POST,
        "/v1/users",
        Some(json!({ "name": "hank", "role": "superuser" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "invalid_request");
}

// ---------------------------------------------------------------------------
// Member 403 on every admin-only route
// ---------------------------------------------------------------------------

#[tokio::test]
async fn member_gets_403_on_every_admin_route() {
    let (_dir, state, app) = make_enforced_app().await;
    let member_secret = seed_user_with_key(&state, "mallory", Role::Member).await;

    let admin_routes: &[(Method, &str, Option<serde_json::Value>)] = &[
        (Method::POST, "/v1/stores", Some(json!({ "name": "x" }))),
        (
            Method::PATCH,
            "/v1/stores/nonexistent",
            Some(json!({ "visibility": "shared" })),
        ),
        (Method::DELETE, "/v1/stores/nonexistent", None),
        (
            Method::POST,
            "/v1/stores/nonexistent/sources",
            Some(json!({ "kind": "path", "spec": {"root": "/tmp"} })),
        ),
        (Method::DELETE, "/v1/sources/nonexistent", None),
        (Method::POST, "/v1/jobs", Some(json!({ "store_name": "x" }))),
        (Method::GET, "/v1/jobs/nonexistent", None),
        (Method::GET, "/v1/config", None),
        (Method::GET, "/v1/users", None),
        (
            Method::POST,
            "/v1/users",
            Some(json!({ "name": "y", "role": "member" })),
        ),
        (
            Method::PATCH,
            "/v1/users/nonexistent",
            Some(json!({ "role": "admin" })),
        ),
        (Method::DELETE, "/v1/users/nonexistent", None),
        (Method::GET, "/v1/users/nonexistent/keys", None),
        (Method::DELETE, "/v1/keys/nonexistent", None),
        (Method::GET, "/v1/stores/nonexistent/grants", None),
        (
            Method::POST,
            "/v1/stores/nonexistent/grants",
            Some(json!({ "user": "someone" })),
        ),
        (
            Method::DELETE,
            "/v1/stores/nonexistent/grants/someone",
            None,
        ),
    ];

    for (method, path, body) in admin_routes {
        let resp = request_with_bearer(
            app.clone(),
            method.clone(),
            path,
            body.clone(),
            Some(&member_secret),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "expected 403 for member on {method} {path}, got {}",
            resp.status()
        );
    }
}

// ---------------------------------------------------------------------------
// Key create -> list -> revoke -> 401 on revoked key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn key_create_list_revoke_then_401_on_revoked_key() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin3", Role::Admin).await;
    let target = state
        .auth()
        .create_user("ivan", Role::Member)
        .await
        .unwrap();

    // Admin creates a key for another user.
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        &format!("/v1/users/{}/keys", target.id),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    let key_secret = body["secret"].as_str().unwrap().to_string();
    let key_id = body["id"].as_str().unwrap().to_string();
    assert!(key_secret.starts_with("ldb_"));

    // The new key authenticates.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/auth/me",
        None,
        Some(&key_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // List never carries the secret.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        &format!("/v1/users/{}/keys", target.id),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let keys = body.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["id"], key_id);
    assert!(
        serde_json::to_string(&keys[0]).unwrap().contains(&key_id),
        "sanity: id present"
    );
    assert!(
        !serde_json::to_string(&keys[0])
            .unwrap()
            .contains(&key_secret),
        "the listing must never carry the plaintext secret"
    );

    // Revoke.
    let resp = request_with_bearer(
        app.clone(),
        Method::DELETE,
        &format!("/v1/keys/{key_id}"),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Revoked key no longer authenticates.
    let resp = request_with_bearer(app, Method::GET, "/v1/auth/me", None, Some(&key_secret)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn member_can_create_a_key_for_themselves_but_not_for_others() {
    let (_dir, state, app) = make_enforced_app().await;
    let member = state
        .auth()
        .create_user("self-serve", Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;
    let other = state
        .auth()
        .create_user("someone-else", Role::Member)
        .await
        .unwrap();

    // Self: allowed.
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        &format!("/v1/users/{}/keys", member.id),
        None,
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Someone else: admin-only, 403 for a member.
    let resp = request_with_bearer(
        app,
        Method::POST,
        &format!("/v1/users/{}/keys", other.id),
        None,
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Last-admin lockout guard rails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn last_admin_self_delete_is_refused() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin = state
        .auth()
        .create_user("sole-admin", Role::Admin)
        .await
        .unwrap();
    let admin_secret = state.auth().issue_api_key(&admin.id).await.unwrap().secret;

    let resp = request_with_bearer(
        app,
        Method::DELETE,
        &format!("/v1/users/{}", admin.id),
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn last_admin_self_demote_is_refused() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin = state
        .auth()
        .create_user("sole-admin2", Role::Admin)
        .await
        .unwrap();
    let admin_secret = state.auth().issue_api_key(&admin.id).await.unwrap().secret;

    let resp = request_with_bearer(
        app,
        Method::PATCH,
        &format!("/v1/users/{}", admin.id),
        Some(json!({ "role": "member" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn non_last_admin_can_be_deleted_and_demoted() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin1 = state
        .auth()
        .create_user("admin-a", Role::Admin)
        .await
        .unwrap();
    let admin1_secret = state.auth().issue_api_key(&admin1.id).await.unwrap().secret;
    let admin2 = state
        .auth()
        .create_user("admin-b", Role::Admin)
        .await
        .unwrap();

    // admin1 demotes admin2 — two admins remain one, still > 0.
    let resp = request_with_bearer(
        app.clone(),
        Method::PATCH,
        &format!("/v1/users/{}", admin2.id),
        Some(json!({ "role": "member" })),
        Some(&admin1_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // admin1 (still the only admin) now cannot delete themselves.
    let resp = request_with_bearer(
        app,
        Method::DELETE,
        &format!("/v1/users/{}", admin1.id),
        None,
        Some(&admin1_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// D7 member store visibility scoping
// ---------------------------------------------------------------------------

async fn seed_chunk(state: &server::AppState, store_name: &str, text: &str, uri: &str) {
    use localdb_core::Embedder;

    let source = state
        .add_source(store_name, "path", json!({"root": "/tmp"}), "prose", None)
        .await
        .unwrap();
    let store_id = source.store_id.clone();
    let embedder = localdb_core::FakeEmbedder::new(128);
    let docs = vec![localdb_core::embedder::DocumentChunks {
        document_context: text.to_string(),
        chunks: vec![text.to_string()],
    }];
    let embedding = embedder
        .embed_documents(docs)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let chunk = localdb_core::ChunkRecord {
        id: format!("chunk-{store_name}"),
        resource_id: format!("doc-{store_name}"),
        store_id: store_id.clone(),
        text: text.to_string(),
        span: localdb_core::types::Span::new(0, text.len()),
        heading_path: vec![],
        embedding,
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: "abc123".to_string(),
        origin_store: store_id.clone(),
        source_id: source.id,
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: uri.to_string(),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        window_block_seqs: vec![],
    };
    state
        .backend()
        .retrieval_store(&store_id)
        .await
        .unwrap()
        .upsert_chunks(vec![chunk])
        .await
        .unwrap();
}

#[tokio::test]
async fn member_sees_only_the_granted_shared_store() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin4", Role::Admin).await;

    create_store_as(app.clone(), "granted-shared", "shared", Some(&admin_secret)).await;
    create_store_as(
        app.clone(),
        "ungranted-shared",
        "shared",
        Some(&admin_secret),
    )
    .await;
    create_store_as(
        app.clone(),
        "secret-private",
        "private",
        Some(&admin_secret),
    )
    .await;

    seed_chunk(&state, "granted-shared", "hello world rust", "file:///a.md").await;
    seed_chunk(
        &state,
        "ungranted-shared",
        "hello world rust",
        "file:///b.md",
    )
    .await;
    seed_chunk(&state, "secret-private", "hello world rust", "file:///c.md").await;

    let member = state
        .auth()
        .create_user("victor", Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    // Grant only "granted-shared".
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/stores/granted-shared/grants",
        Some(json!({ "user": "victor" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // `GET /v1/stores` shows exactly the granted store.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores",
        None,
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["granted-shared"]);

    // Direct GET on the ungranted shared store: 403, not 404.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/ungranted-shared",
        None,
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Direct GET on the private store: 403 too.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/secret-private",
        None,
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Direct GET on the granted store: 200.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/granted-shared",
        None,
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Unscoped search only returns citations from the granted store.
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/search",
        Some(json!({ "query": "hello world rust" })),
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let citations = body["citations"].as_array().unwrap();
    assert!(!citations.is_empty());
    assert!(citations
        .iter()
        .all(|c| c["uri"].as_str().unwrap() == "file:///a.md"));

    // Explicit store_filter naming an unreadable store is a 403.
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/search",
        Some(json!({ "query": "hello", "store_filter": ["ungranted-shared"] })),
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Mutation routes remain admin-only for a member even on a store they
    // can read.
    let resp = request_with_bearer(
        app,
        Method::PATCH,
        "/v1/stores/granted-shared",
        Some(json!({ "visibility": "private" })),
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admin_sees_every_store_regardless_of_visibility() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin5", Role::Admin).await;

    create_store_as(app.clone(), "shared-one", "shared", Some(&admin_secret)).await;
    create_store_as(app.clone(), "private-one", "private", Some(&admin_secret)).await;

    let resp = request_with_bearer(app, Method::GET, "/v1/stores", None, Some(&admin_secret)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let mut names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["private-one", "shared-one"]);
}

// ---------------------------------------------------------------------------
// D7 member store visibility scoping — GET /v1/status (finding #6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn member_status_counts_are_scoped_to_readable_stores() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin-status", Role::Admin).await;

    create_store_as(app.clone(), "granted-shared", "shared", Some(&admin_secret)).await;
    create_store_as(
        app.clone(),
        "ungranted-shared",
        "shared",
        Some(&admin_secret),
    )
    .await;
    create_store_as(
        app.clone(),
        "secret-private",
        "private",
        Some(&admin_secret),
    )
    .await;

    // One source per store, so `source_count` also has something to leak
    // if it isn't scoped.
    seed_chunk(&state, "granted-shared", "hello world rust", "file:///a.md").await;
    seed_chunk(
        &state,
        "ungranted-shared",
        "hello world rust",
        "file:///b.md",
    )
    .await;
    seed_chunk(&state, "secret-private", "hello world rust", "file:///c.md").await;

    let member = state
        .auth()
        .create_user("ursula", Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    // Grant only "granted-shared".
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/stores/granted-shared/grants",
        Some(json!({ "user": "ursula" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Admin sees the true totals: all three stores, one source each.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/status",
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["store_count"], 3);
    assert_eq!(body["source_count"], 3);

    // The member only holds a grant on one shared store: counts must
    // reflect just that store, not the ungranted-shared or private ones.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/status",
        None,
        Some(&member_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(
        body["store_count"], 1,
        "member must only see the granted store in the count"
    );
    assert_eq!(
        body["source_count"], 1,
        "member must only see sources from the granted store"
    );
}

// ---------------------------------------------------------------------------
// Grant create -> list -> delete; rejection on private stores; realtime
// revoke effect on the very next request (no restart, D7+D12 composition).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grant_create_list_delete_happy_path() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin6", Role::Admin).await;
    create_store_as(app.clone(), "shared-store", "shared", Some(&admin_secret)).await;
    state
        .auth()
        .create_user("wendy", Role::Member)
        .await
        .unwrap();

    // Create.
    let resp = request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/stores/shared-store/grants",
        Some(json!({ "user": "wendy" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["user_name"], "wendy");
    assert_eq!(body["store_name"], "shared-store");

    // List.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores/shared-store/grants",
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let grants = body.as_array().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["user_name"], "wendy");

    // Delete.
    let resp = request_with_bearer(
        app.clone(),
        Method::DELETE,
        "/v1/stores/shared-store/grants/wendy",
        None,
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = request_with_bearer(
        app,
        Method::GET,
        "/v1/stores/shared-store/grants",
        None,
        Some(&admin_secret),
    )
    .await;
    let body = json_body(resp.into_body()).await;
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn grant_on_private_store_is_rejected() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin7", Role::Admin).await;
    create_store_as(app.clone(), "private-store", "private", Some(&admin_secret)).await;
    state
        .auth()
        .create_user("xena", Role::Member)
        .await
        .unwrap();

    let resp = request_with_bearer(
        app,
        Method::POST,
        "/v1/stores/private-store/grants",
        Some(json!({ "user": "xena" })),
        Some(&admin_secret),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn grant_revoke_takes_effect_on_the_very_next_request() {
    let (_dir, state, app) = make_enforced_app().await;
    let admin_secret = seed_user_with_key(&state, "root-admin8", Role::Admin).await;
    create_store_as(app.clone(), "toggle-store", "shared", Some(&admin_secret)).await;
    let member = state
        .auth()
        .create_user("yusuf", Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    // No grant yet: the store is invisible to the member.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores",
        None,
        Some(&member_secret),
    )
    .await;
    let body = json_body(resp.into_body()).await;
    assert!(body["items"].as_array().unwrap().is_empty());

    // Grant it.
    request_with_bearer(
        app.clone(),
        Method::POST,
        "/v1/stores/toggle-store/grants",
        Some(json!({ "user": "yusuf" })),
        Some(&admin_secret),
    )
    .await;

    // Now visible, no restart.
    let resp = request_with_bearer(
        app.clone(),
        Method::GET,
        "/v1/stores",
        None,
        Some(&member_secret),
    )
    .await;
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // Revoke it.
    request_with_bearer(
        app.clone(),
        Method::DELETE,
        "/v1/stores/toggle-store/grants/yusuf",
        None,
        Some(&admin_secret),
    )
    .await;

    // Invisible again, no restart.
    let resp =
        request_with_bearer(app, Method::GET, "/v1/stores", None, Some(&member_secret)).await;
    let body = json_body(resp.into_body()).await;
    assert!(body["items"].as_array().unwrap().is_empty());
}
