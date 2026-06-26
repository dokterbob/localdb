//! Run the `RetrievalStore` conformance suite against the unified-DB-backed
//! `StoreHandle`. Mirrors `tests/conformance.rs` which exercises the legacy
//! per-file `LibsqlStore`. The cutover in PR 2 leaves only this file.
//!
//! The conformance functions live in `localdb_core::store::conformance`.
//! `test_delete_by_store` is multi-tenant by design (it inserts records with
//! different `store_id`s and queries `stats()` afterwards expecting the
//! cross-tenant count) — that semantic doesn't fit a single-handle view, so
//! we replace it with `delete_by_store_cross_handle` below.

use tempfile::tempdir;

use localdb_core::store::conformance;
use localdb_core::store::ChunkRecord;
use localdb_core::types::{SourceKind, Span, StoreVisibility};
use localdb_core::{Error, SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
use store_libsql::SqliteBackend;

/// Open a fresh DB and register the stores/source the conformance records
/// depend on. The conformance helpers hard-code `source_id="src-1"` and use
/// `store_id` values `store-1`, `store-A`, `store-B`.
async fn setup() -> (tempfile::TempDir, SqliteBackend) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        2,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();

    for store_id in ["store-1", "store-A", "store-B"] {
        backend
            .upsert_store(&StoreRow {
                id: store_id.to_string(),
                name: store_id.to_string(),
                visibility: StoreVisibility::Private,
                backend: "libsql".to_string(),
                indexing_policy: "{}".to_string(),
                policy_version: "v1".to_string(),
                acl: "{}".to_string(),
                created_at: "2026-06-25T12:00:00Z".to_string(),
            })
            .await
            .unwrap();
    }

    backend
        .upsert_source(&SourceRow {
            id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Path,
            root: Some("/test/conformance".to_string()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: "2026-06-25T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    (dir, backend)
}

#[tokio::test]
async fn upsert_and_stats() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_upsert_and_stats(handle.as_ref()).await;
}

#[tokio::test]
async fn upsert_replaces_existing() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_upsert_replaces_existing(handle.as_ref()).await;
}

#[tokio::test]
async fn delete_by_document() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_delete_by_document(handle.as_ref()).await;
}

#[tokio::test]
async fn delete_nonexistent_document() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_delete_nonexistent_document(handle.as_ref()).await;
}

#[tokio::test]
async fn dense_search_round_trip() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_dense_search_round_trip(handle.as_ref()).await;
}

#[tokio::test]
async fn bm25_search_round_trip() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_bm25_search_round_trip(handle.as_ref()).await;
}

#[tokio::test]
async fn metadata_filter_mime() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_metadata_filter_mime(handle.as_ref()).await;
}

#[tokio::test]
async fn metadata_filter_uri_prefix() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_metadata_filter_uri_prefix(handle.as_ref()).await;
}

#[tokio::test]
async fn get_chunk() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_get_chunk(handle.as_ref()).await;
}

#[tokio::test]
async fn get_chunks_for_document() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_get_chunks_for_document(handle.as_ref()).await;
}

#[tokio::test]
async fn dense_search_limit() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_dense_search_limit(handle.as_ref()).await;
}

#[tokio::test]
async fn bm25_search_limit() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_bm25_search_limit(handle.as_ref()).await;
}

/// Replacement for `test_delete_by_store`: the trait-level conformance
/// version assumes a multi-tenant store, but `StoreHandle` is a
/// single-tenant view. Drive the test via two handles into the same DB.
#[tokio::test]
async fn delete_by_store_cross_handle() {
    let (_dir, db) = setup().await;
    let handle_a = db.retrieval_store("store-A").await.unwrap();
    let handle_b = db.retrieval_store("store-B").await.unwrap();

    let records_a = vec![
        make_record("chunk-1", "doc-1", "store-A", vec![1.0, 0.0]),
        make_record("chunk-2", "doc-2", "store-A", vec![0.0, 1.0]),
    ];
    handle_a.upsert_chunks(records_a).await.unwrap();

    let records_b = vec![make_record("chunk-3", "doc-3", "store-B", vec![0.5, 0.5])];
    handle_b.upsert_chunks(records_b).await.unwrap();

    assert_eq!(handle_a.stats().await.unwrap().chunk_count, 2);
    assert_eq!(handle_b.stats().await.unwrap().chunk_count, 1);

    let deleted = handle_a.delete_by_store("store-A").await.unwrap();
    assert_eq!(deleted, 2, "delete_by_store should remove 2 store-A chunks");

    assert_eq!(handle_a.stats().await.unwrap().chunk_count, 0);
    assert_eq!(
        handle_b.stats().await.unwrap().chunk_count,
        1,
        "store-B should be untouched"
    );
}

#[tokio::test]
async fn upsert_chunks_rejects_cross_tenant_record() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-A").await.unwrap();
    let result = handle
        .upsert_chunks(vec![make_record(
            "chunk-cross",
            "doc-cross",
            "store-B",
            vec![0.5, 0.5],
        )])
        .await;
    assert!(matches!(
        result,
        Err(Error::Internal {
            correlation_id,
            ..
        }) if correlation_id == "store_handle_tenant_violation"
    ));
}

#[tokio::test]
async fn tenant_delete_by_store_rejects_foreign_store_id() {
    let (_dir, backend) = setup().await;
    let handle_a = backend.retrieval_store("store-A").await.unwrap();
    let result = handle_a.delete_by_store("store-B").await;
    assert!(matches!(
        result,
        Err(Error::Internal {
            correlation_id,
            ..
        }) if correlation_id == "store_handle_tenant_violation"
    ));
}

#[tokio::test]
async fn find_document_errors_when_id_exists_in_multiple_stores() {
    let (_dir, db) = setup().await;
    let handle_a = db.retrieval_store("store-A").await.unwrap();
    let handle_b = db.retrieval_store("store-B").await.unwrap();
    handle_a
        .upsert_chunks(vec![make_record(
            "chunk-a",
            "doc-shared",
            "store-A",
            vec![1.0, 0.0],
        )])
        .await
        .unwrap();
    handle_b
        .upsert_chunks(vec![make_record(
            "chunk-b",
            "doc-shared",
            "store-B",
            vec![0.0, 1.0],
        )])
        .await
        .unwrap();

    let result = db.find_document("doc-shared").await;
    assert!(matches!(
        result,
        Err(Error::Internal {
            correlation_id,
            ..
        }) if correlation_id == "runtime_state_find_doc_ambiguous"
    ));
}

fn make_record(id: &str, doc_id: &str, store_id: &str, embedding: Vec<f32>) -> ChunkRecord {
    let text = format!("text for {id}");
    ChunkRecord {
        id: id.to_string(),
        document_id: doc_id.to_string(),
        store_id: store_id.to_string(),
        text: text.clone(),
        span: Span::new(0, text.len()),
        heading_path: vec![],
        embedding,
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-25T12:00:00Z".to_string(),
        content_hash: "abc123".to_string(),
        origin_store: store_id.to_string(),
        source_id: "src-1".to_string(),
        source_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: format!("file:///{store_id}/{doc_id}.md"),
        metadata: localdb_core::parser::DocumentMetadata::default(),
    }
}
