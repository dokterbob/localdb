//! `AppState` store/source CRUD tests.

use localdb_core::Error;

use super::common::make_state;

#[tokio::test]
async fn add_and_list_stores() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let effective = state.effective_config().await.unwrap();
    assert_eq!(effective.stores.len(), 1);
    assert_eq!(effective.stores[0].name, "notes");
}

#[tokio::test]
async fn add_store_rejects_unknown_visibility() {
    let (_dir, state) = make_state().await;
    let result = state.add_store("notes", "public").await;
    assert!(matches!(result, Err(Error::InvalidRequest { .. })));
}

#[tokio::test]
async fn remove_store_not_found() {
    let (_dir, state) = make_state().await;
    let result = state.remove_store("non-existent").await;
    assert!(matches!(result, Err(Error::StoreNotFound { .. })));
}

#[tokio::test]
async fn remove_store_succeeds() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    state.remove_store("notes").await.unwrap();
    let effective = state.effective_config().await.unwrap();
    assert!(effective.stores.is_empty());
}

#[tokio::test]
async fn add_source_to_nonexistent_store_fails() {
    let (_dir, state) = make_state().await;
    let result = state
        .add_source(
            "no-such-store",
            "path",
            serde_json::json!({"root": "/tmp"}),
            "prose",
            None,
        )
        .await;
    assert!(matches!(result, Err(Error::StoreNotFound { .. })));
}

#[tokio::test]
async fn add_and_list_sources() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "path",
            serde_json::json!({"root": "/tmp/notes", "include": [], "exclude": []}),
            "prose",
            None,
        )
        .await
        .unwrap();

    let sources = state.list_sources("notes").await.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id, source.id);
}

#[tokio::test]
async fn add_source_rejects_non_array_include() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let result = state
        .add_source(
            "notes",
            "path",
            serde_json::json!({"root": "/tmp/notes", "include": "**/*.md"}),
            "prose",
            None,
        )
        .await;
    assert!(matches!(result, Err(Error::InvalidRequest { .. })));
}

#[tokio::test]
async fn add_source_rejects_non_string_exclude_entry() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let result = state
        .add_source(
            "notes",
            "path",
            serde_json::json!({"root": "/tmp/notes", "exclude": [42]}),
            "prose",
            None,
        )
        .await;
    assert!(matches!(result, Err(Error::InvalidRequest { .. })));
}

#[tokio::test]
async fn remove_source_not_found() {
    let (_dir, state) = make_state().await;
    let result = state.remove_source("no-such-source").await;
    assert!(matches!(result, Err(Error::SourceNotFound { .. })));
}

#[tokio::test]
async fn remove_source_succeeds() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "path",
            serde_json::json!({"root": "/tmp"}),
            "prose",
            None,
        )
        .await
        .unwrap();
    state.remove_source(&source.id).await.unwrap();
    let sources = state.list_sources("notes").await.unwrap();
    assert!(sources.is_empty());
}

#[tokio::test]
async fn update_store_updates_visibility() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    state.update_store("notes", Some("shared")).await.unwrap();
    let record = state.get_store_by_name("notes").await.unwrap();
    assert_eq!(record.visibility, "shared");
}

#[tokio::test]
async fn upsert_and_search_chunks_roundtrip() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let store_id = state
        .backend()
        .get_store_by_name("notes")
        .await
        .unwrap()
        .unwrap()
        .id;
    let source = state
        .add_source(
            "notes",
            "path",
            serde_json::json!({"root": "/tmp/notes"}),
            "prose",
            None,
        )
        .await
        .unwrap();

    let chunk = localdb_core::ChunkRecord {
        id: "chunk-1".to_string(),
        resource_id: "doc-1".to_string(),
        store_id: store_id.clone(),
        text: "hello world rust programming".to_string(),
        span: localdb_core::types::Span::new(0, 30),
        heading_path: vec![],
        embedding: vec![1.0; 128],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        modified_at: Some("2026-06-10T12:00:00Z".to_string()),
        content_hash: "abc".to_string(),
        origin_store: store_id.clone(),
        source_id: source.id,
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: "file:///test.md".to_string(),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    };

    let handle = state.backend().retrieval_store(&store_id).await.unwrap();
    handle.upsert_chunks(vec![chunk]).await.unwrap();
    let stats = handle.stats().await.unwrap();
    assert_eq!(stats.chunk_count, 1, "one chunk should be indexed");
}

#[tokio::test]
async fn add_store_duplicate_name_returns_invalid_request() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let result = state.add_store("notes", "private").await;
    assert!(
        matches!(result, Err(Error::InvalidRequest { .. })),
        "duplicate store name should return InvalidRequest; got: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn remove_store_cascades_sources() {
    let (_dir, state) = make_state().await;

    state.add_store("scratch", "private").await.unwrap();
    state
        .add_source(
            "scratch",
            "path",
            serde_json::json!({"root": "/tmp/a"}),
            "prose",
            None,
        )
        .await
        .unwrap();
    state
        .add_source(
            "scratch",
            "path",
            serde_json::json!({"root": "/tmp/b"}),
            "prose",
            None,
        )
        .await
        .unwrap();

    let before = state.list_sources("scratch").await.unwrap();
    assert_eq!(before.len(), 2);

    state.remove_store("scratch").await.unwrap();
    assert!(
        matches!(
            state.list_sources("scratch").await,
            Err(Error::StoreNotFound { .. })
        ),
        "removed store should not list sources"
    );
    assert!(state.backend().list_stores().await.unwrap().is_empty());
}
