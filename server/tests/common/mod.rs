// This module is compiled once per integration-test binary, and no binary
// uses every helper — silence per-binary dead-code noise wholesale.
#![allow(dead_code)]

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use localdb_core::config::schema::{
    DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, RawConfig,
};
use serde_json::{json, Value};
use server::{
    build_router, mcp_bridge::AppStateStoreProvider, AppState, AuthMode, JobQueue,
    UrlRefreshScheduler,
};
use tempfile::TempDir;
use tower::ServiceExt;

pub(crate) async fn make_app() -> (TempDir, Router) {
    let (dir, _state, router) = make_app_with_mode(AuthMode::Open).await;
    (dir, router)
}

/// Like `make_app`, but with an explicit auth mode, also returning the
/// `AppState` so tests can seed users/keys through `state.auth()` against
/// the same database the router serves.
pub(crate) async fn make_enforced_app() -> (TempDir, AppState, Router) {
    make_app_with_mode(AuthMode::Enforced).await
}

pub(crate) async fn make_app_with_mode(mode: AuthMode) -> (TempDir, AppState, Router) {
    let dir = tempfile::tempdir().expect("tempdir is created for isolated server API test");
    let queue = JobQueue::new();
    let state = AppState::new(
        fake_yaml_config(),
        dir.path().to_path_buf(),
        queue.clone(),
        UrlRefreshScheduler::new(queue),
        mode,
    )
    .await
    .expect("fake daemon state should open a temp libsql database");

    // The provider wraps a clone of `state` (cheap — `AppState` is
    // `Arc`-backed) so it keeps resolving stores from the *same* live
    // database `state`/the returned `Router` share, even after this
    // function returns — this is what lets `mcp_route.rs`'s realtime test
    // add a store via `POST /v1/stores` after the router is built and see
    // it reflected in a later `list_stores` MCP call, no restart needed.
    let mcp_provider: std::sync::Arc<dyn mcp::StoreProvider> =
        std::sync::Arc::new(AppStateStoreProvider::new(state.clone()));

    let router = build_router(
        state.clone(),
        mcp_provider,
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );
    (dir, state, router)
}

fn fake_yaml_config() -> RawConfig {
    RawConfig {
        version: 1,
        server: Default::default(),
        paths: Default::default(),
        defaults: DefaultsConfig {
            indexing: IndexingPolicyConfig {
                chunking: Default::default(),
                embedding: EmbeddingPolicy {
                    provider: "fake".to_string(),
                    model: "default".to_string(),
                },
                ..Default::default()
            },
        },
        providers: vec![],
    }
}

pub(crate) async fn json_body(body: Body) -> Value {
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("response body should be readable");
    serde_json::from_slice(&bytes).expect("response body should be valid JSON")
}

pub(crate) async fn request(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> axum::response::Response {
    request_with_bearer(app, method, uri, body, None).await
}

/// Like `request`, optionally attaching `Authorization: Bearer <secret>`.
pub(crate) async fn request_with_bearer(
    app: Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    bearer: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(secret) = bearer {
        builder = builder.header("authorization", format!("Bearer {secret}"));
    }
    let request_body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };

    app.oneshot(
        builder
            .body(request_body)
            .expect("test request should be constructible"),
    )
    .await
    .expect("router should answer test request")
}

pub(crate) async fn create_store(app: Router, name: &str) -> Value {
    let resp = request(
        app,
        Method::POST,
        "/v1/stores",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    json_body(resp.into_body()).await
}
