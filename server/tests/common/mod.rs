use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    Router,
};
use localdb_core::config::schema::{
    DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, RawConfig,
};
use serde_json::{json, Value};
use server::{build_router, AppState, JobQueue, UrlRefreshScheduler};
use tempfile::TempDir;
use tower::ServiceExt;

pub(crate) async fn make_app() -> (TempDir, Router) {
    let dir = tempfile::tempdir().expect("tempdir is created for isolated server API test");
    let queue = JobQueue::new();
    let state = AppState::new(
        fake_yaml_config(),
        dir.path().to_path_buf(),
        queue.clone(),
        UrlRefreshScheduler::new(queue),
    )
    .await
    .expect("fake daemon state should open a temp libsql database");

    (dir, build_router(state))
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
    let mut builder = Request::builder().method(method).uri(uri);
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
