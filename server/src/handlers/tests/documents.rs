use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

use super::common::{
    json_body, make_app, make_state_with_fake_config, seed_store_a_chunk, SeedChunkInput,
};

#[tokio::test]
async fn get_document_not_found_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/documents/nonexistent-doc-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "document_not_found");
}

#[tokio::test]
async fn get_document_returns_record_when_indexed() {
    let (_dir, state) = make_state_with_fake_config().await;
    let metadata = localdb_core::parser::DocumentMetadata {
        title: Some("Test Doc".to_string()),
        creator: vec!["Test Author".to_string()],
        date: Some("2026-06-10".to_string()),
        ..Default::default()
    };
    seed_store_a_chunk(
        &state,
        SeedChunkInput {
            chunk_id: "chunk-doc-abc123",
            doc_id: "doc-abc123",
            text: "hello world",
            uri: "file:///test.md",
            metadata,
        },
    )
    .await;

    let app = crate::daemon::build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/documents/doc-abc123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["id"], "doc-abc123");
    assert_eq!(body["uri"], "file:///test.md");
    assert_eq!(body["title"], "Test Doc");
    assert_eq!(body["normalized_text"], "hello world");
    assert!(body.get("metadata").is_some());
    assert_eq!(
        body["metadata"]["creator"].as_array().unwrap()[0]
            .as_str()
            .unwrap(),
        "Test Author"
    );
}
