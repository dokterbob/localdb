use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

use super::common::{
    json_body, make_app, make_state_with_fake_config, seed_store_a_chunk, SeedChunkInput,
};

#[tokio::test]
async fn get_document_returns_404_when_missing() {
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
    assert_eq!(body["code"], "resource_not_found");
}

#[tokio::test]
async fn get_document_returns_record_when_indexed() {
    let (_dir, state) = make_state_with_fake_config().await;
    let metadata =
        localdb_core::metadata::Metadata::Document(localdb_core::metadata::DocumentMetadata {
            dublin_core: localdb_core::metadata::DublinCoreMetadata {
                title: Some("Test Doc".to_string()),
                creator: vec!["Test Author".to_string()],
                date: Some("2026-06-10".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
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

    let app = crate::daemon::build_router(
        state,
        std::sync::Arc::new(mcp::StaticStoreProvider::new(vec![])),
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );
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

/// Regression test: `GET /v1/documents/{id}` must reconstruct a multi-chunk
/// table from its persisted `blocks`, not by joining `ChunkRecord.text`. The
/// table chunker (spec 04 §3, intentional) re-emits the header + `|---|`
/// separator row in every chunk of a table split across multiple chunks —
/// joining chunk texts would duplicate that header once per chunk. The
/// single `Table` block holds the canonical text with the header exactly
/// once, so `normalized_text` must not duplicate it.
#[tokio::test]
async fn get_document_reconstructs_table_without_duplicated_header() {
    use localdb_core::block::{Block, BlockKind};
    use localdb_core::{chunk_blocks, resource_id, CharSizer, ChunkRecord, ChunkerConfig, Span};

    let (_dir, state) = make_state_with_fake_config().await;
    state.add_store("store-A", "private").await.unwrap();
    let source = state
        .add_source(
            "store-A",
            "path",
            serde_json::json!({"root": "/tmp"}),
            "prose",
            None,
        )
        .await
        .unwrap();
    let store_id = source.store_id.clone();

    // Same fixture shape as chunker.rs's own
    // `table_multi_chunk_split_preserves_header` unit test: with
    // target_tokens=40 and CharSizer, 2 data rows pack per chunk, so 10 rows
    // split into 5 chunks, each re-emitting the header/separator.
    let table_text = {
        let mut md = String::from("| A | B |\n|---|---|\n");
        let rows: Vec<String> = (0..10).map(|i| format!("| {i} | {i} |")).collect();
        md.push_str(&rows.join("\n"));
        md
    };
    let doc_uri = "file:///table.md";
    let doc_hash = localdb_core::content_hash(&table_text);
    let doc_id = resource_id(doc_uri, &doc_hash);

    let block = Block {
        seq: 0,
        kind: BlockKind::Table {
            headers: vec!["A".to_string(), "B".to_string()],
            rows: 10,
        },
        text: table_text.clone(),
        location: None,
    };
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(40),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunk_outputs = chunk_blocks(&doc_id, std::slice::from_ref(&block), &cfg, &CharSizer)
        .expect("chunking the table fixture must succeed");
    assert!(
        chunk_outputs.len() >= 2,
        "fixture must produce a multi-chunk table split, got {} chunk(s)",
        chunk_outputs.len()
    );

    let chunk_records: Vec<ChunkRecord> = chunk_outputs
        .iter()
        .map(|co| ChunkRecord {
            id: co.id.clone(),
            resource_id: doc_id.clone(),
            store_id: store_id.clone(),
            text: co.text.clone(),
            span: Span::new(co.span.start, co.span.end),
            heading_path: co.heading_path.clone(),
            embedding: vec![0.0; 128],
            policy_version: "policy-v1".to_string(),
            fetched_at: "2026-06-29T00:00:00Z".to_string(),
            content_hash: doc_hash.clone(),
            origin_store: store_id.clone(),
            source_id: source.id.clone(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: doc_uri.to_string(),
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq: co.block_seq,
            seq_in_block: co.seq_in_block,
            block_kind: co.block_kind.clone(),
            window_block_seqs: co.window_block_seqs.clone(),
        })
        .collect();

    state
        .backend()
        .retrieval_store(&store_id)
        .await
        .unwrap()
        .upsert_chunks_and_blocks(&store_id, &doc_id, chunk_records, &[block], None)
        .await
        .unwrap();

    let app = crate::daemon::build_router(
        state,
        std::sync::Arc::new(mcp::StaticStoreProvider::new(vec![])),
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(128)),
        vec![],
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/documents/{doc_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let normalized_text = body["normalized_text"].as_str().unwrap();

    assert_eq!(
        normalized_text.matches("| A | B |").count(),
        1,
        "normalized_text must contain the table header exactly once, not \
         once per chunk; got: {normalized_text:?}"
    );
    assert_eq!(
        normalized_text.matches("|---|---|").count(),
        1,
        "normalized_text must contain the separator row exactly once; got: {normalized_text:?}"
    );
    assert_eq!(
        normalized_text, table_text,
        "block-based reconstruction should equal the canonical block text exactly"
    );
}
