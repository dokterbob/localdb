use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

use super::common::{
    json_body, make_app, make_state_with_auth_mode, make_state_with_fake_config,
    seed_chunk_in_store, seed_store_a_chunk, seed_user_with_key, SeedChunkInput,
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

/// D7 regression test: `GET /v1/documents/{id}` for a document that
/// genuinely exists, in a `shared` store the caller was never granted
/// access to, must be `403 Forbidden` — not `404` (which would let a caller
/// distinguish "unknown id" from "real id, no access" by status code alone)
/// and not `200` (which would leak the document). See
/// `handlers::documents::get_document`'s doc comment: masked as
/// `resource_not_found` only when the document id itself is unknown, but
/// `forbidden` when it exists in an unreadable store — same 403-over-404
/// consistency point as `handlers::stores::get_store` and
/// specs/05-surfaces.md:137's named-but-unreadable-store rule.
///
/// Drives a real `AuthMode::Enforced` router with a genuine bearer token
/// (not `Principal::local_trust()`, which is admin-equivalent and bypasses
/// D7 entirely) for a `Role::Member` principal holding no grants at all.
#[tokio::test]
async fn get_document_without_store_grant_returns_403() {
    let (_dir, state) = make_state_with_auth_mode(crate::auth::AuthMode::Enforced).await;

    seed_chunk_in_store(
        &state,
        "store-shared",
        "shared",
        SeedChunkInput {
            chunk_id: "chunk-secret-1",
            doc_id: "doc-secret-1",
            text: "confidential contents the ungranted member must never see",
            uri: "file:///secret.md",
            metadata: localdb_core::metadata::Metadata::default(),
        },
    )
    .await;

    let bearer =
        seed_user_with_key(&state, "member-no-grant", localdb_core::auth::Role::Member).await;

    let app = crate::daemon::build_router(
        state,
        std::sync::Arc::new(mcp::StaticStoreProvider::new(vec![])),
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(128)),
        vec![],
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/documents/doc-secret-1")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "forbidden");
}

/// Positive counterpart to `get_document_without_store_grant_returns_403`:
/// a `Role::Member` principal *with* a grant on the document's owning
/// `shared` store gets the document back, status `200`.
#[tokio::test]
async fn get_document_with_store_grant_returns_200() {
    use localdb_core::types::StoreVisibility;

    let (_dir, state) = make_state_with_auth_mode(crate::auth::AuthMode::Enforced).await;

    seed_chunk_in_store(
        &state,
        "store-shared",
        "shared",
        SeedChunkInput {
            chunk_id: "chunk-visible-1",
            doc_id: "doc-visible-1",
            text: "contents a granted member may read",
            uri: "file:///visible.md",
            metadata: localdb_core::metadata::Metadata::default(),
        },
    )
    .await;

    let admin = state
        .auth()
        .create_user("admin-granter", localdb_core::auth::Role::Admin)
        .await
        .unwrap();
    let member = state
        .auth()
        .create_user("member-with-grant", localdb_core::auth::Role::Member)
        .await
        .unwrap();
    state
        .auth()
        .grant_store(
            "store-shared",
            StoreVisibility::Shared,
            &member.id,
            &admin.id,
        )
        .await
        .unwrap();
    let bearer = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    let app = crate::daemon::build_router(
        state,
        std::sync::Arc::new(mcp::StaticStoreProvider::new(vec![])),
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(128)),
        vec![],
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/documents/doc-visible-1")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["id"], "doc-visible-1");
}
