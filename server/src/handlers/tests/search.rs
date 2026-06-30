use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use super::common::{
    json_body, make_app, make_state_with_fake_config, seed_store_a_chunk, SeedChunkInput,
};

#[tokio::test]
async fn search_empty_query_returns_400() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": ""}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_with_nonexistent_store_filter_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"query": "hello", "store_filter": ["no-such-store"]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_returns_citations_shape() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": "hello world"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert!(body["citations"].is_array());
    assert!(body["total_candidates"].is_number());
}

#[tokio::test]
async fn search_returns_citations_after_indexing() {
    let (_dir, state) = make_state_with_fake_config().await;
    seed_store_a_chunk(
        &state,
        SeedChunkInput {
            chunk_id: "chunk-1",
            doc_id: "doc-1",
            text: "hello world rust programming",
            uri: "file:///hello.md",
            metadata: localdb_core::DocumentMetadata::default(),
        },
    )
    .await;

    let app = crate::daemon::build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": "hello world"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let citations = body["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "got: {:?}", body);
    assert_eq!(citations[0]["uri"], "file:///hello.md");
}

#[tokio::test]
async fn search_with_nonexistent_store_filter_returns_empty() {
    let (_dir, state) = make_state_with_fake_config().await;
    state.add_store("my-store", "private").await.unwrap();
    seed_store_a_chunk(
        &state,
        SeedChunkInput {
            chunk_id: "chunk-ff",
            doc_id: "doc-ff",
            text: "hello world",
            uri: "file:///foreign.md",
            metadata: localdb_core::DocumentMetadata::default(),
        },
    )
    .await;

    let app = crate::daemon::build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"query": "hello world", "store_filter": ["my-store"]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let citations = body["citations"].as_array().unwrap();
    assert!(citations.is_empty(), "got: {:?}", body);
}
