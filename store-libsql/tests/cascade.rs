//! Foreign-key cascade regression test for the unified schema.
//!
//! Verifies the chain `stores → sources` and `stores → documents → chunks`
//! both cascade on `DELETE FROM stores`, that `chunks_fts` stays in sync via
//! triggers fired by the cascade-deleted `chunks` rows, and that the
//! `chunks_vec_idx` DiskANN index becomes empty for the dropped tenant
//! (auto-maintained by libsql).
//!
//! Failure modes this guards against:
//! - Missing `ON DELETE CASCADE` on any FK in the chain.
//! - FTS rows orphaned because `chunks_ad` doesn't fire on cascade deletes.
//! - Vector index entries surviving a row deletion (would surface as
//!   dangling rowids returned by `vector_top_k`).

use tempfile::tempdir;

use localdb_core::parser::DocumentMetadata;
use localdb_core::store::ChunkRecord;
use localdb_core::types::{SourceKind, Span, StoreVisibility};
use localdb_core::{SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
use store_libsql::SqliteBackend;

async fn open_db() -> (tempfile::TempDir, SqliteBackend) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        4,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    (dir, backend)
}

async fn seed_store(db: &SqliteBackend, store_id: &str, n_chunks: usize) {
    db.upsert_store(&StoreRow {
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

    let source_id = format!("src-{store_id}");
    db.upsert_source(&SourceRow {
        id: source_id.clone(),
        store_id: store_id.to_string(),
        kind: SourceKind::Path,
        root: Some(format!("/data/{store_id}")),
        url: None,
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: "2026-06-25T12:00:00Z".to_string(),
    })
    .await
    .unwrap();

    let handle = db.retrieval_store(store_id).await.unwrap();
    let records: Vec<ChunkRecord> = (0..n_chunks)
        .map(|i| make_record(store_id, &source_id, i))
        .collect();
    handle.upsert_chunks(records).await.unwrap();
}

fn make_record(store_id: &str, source_id: &str, idx: usize) -> ChunkRecord {
    let text = format!("chunk {idx} of {store_id} alpha beta gamma");
    ChunkRecord {
        id: format!("chunk-{store_id}-{idx}"),
        document_id: format!("doc-{store_id}"),
        store_id: store_id.to_string(),
        text: text.clone(),
        span: Span::new(idx * 100, idx * 100 + text.len()),
        heading_path: vec![],
        embedding: vec![0.1, 0.2, 0.3, 0.4],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-25T12:00:00Z".to_string(),
        content_hash: format!("hash-{store_id}"),
        origin_store: store_id.to_string(),
        source_id: source_id.to_string(),
        source_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: format!("file:///data/{store_id}/doc.md"),
        metadata: DocumentMetadata::default(),
    }
}

#[tokio::test]
async fn delete_store_cascades_to_sources_documents_chunks_fts_and_vec() {
    let (_dir, db) = open_db().await;
    seed_store(&db, "tenant-a", 5).await;
    seed_store(&db, "tenant-b", 3).await;

    let handle_a = db.retrieval_store("tenant-a").await.unwrap();
    let handle_b = db.retrieval_store("tenant-b").await.unwrap();

    let stats_a = handle_a.stats().await.unwrap();
    assert_eq!(stats_a.chunk_count, 5);
    assert_eq!(stats_a.document_count, 1);

    assert!(
        !handle_a
            .bm25_search("alpha", 10, &[])
            .await
            .unwrap()
            .is_empty(),
        "FTS should find 'alpha' in seeded chunks"
    );
    assert!(
        !handle_a
            .dense_search(&[0.1, 0.2, 0.3, 0.4], 10, &[])
            .await
            .unwrap()
            .is_empty(),
        "vector index should return seeded chunks"
    );

    assert!(db.delete_store("tenant-a").await.unwrap());

    let stats_a = handle_a.stats().await.unwrap();
    assert_eq!(stats_a.chunk_count, 0, "chunks should cascade from store");
    assert_eq!(
        stats_a.document_count, 0,
        "documents should cascade from store"
    );
    assert!(
        db.list_sources("tenant-a").await.unwrap().is_empty(),
        "sources should cascade from store"
    );
    assert!(
        handle_a
            .bm25_search("alpha", 10, &[])
            .await
            .unwrap()
            .is_empty(),
        "FTS rows should be removed by chunks_ad trigger on cascade delete"
    );
    assert!(
        handle_a
            .dense_search(&[0.1, 0.2, 0.3, 0.4], 10, &[])
            .await
            .unwrap()
            .is_empty(),
        "vector index should drop the deleted rows"
    );

    let stats_b = handle_b.stats().await.unwrap();
    assert_eq!(
        stats_b.chunk_count, 3,
        "tenant-b chunks should be untouched"
    );
    assert!(
        !handle_b
            .bm25_search("alpha", 10, &[])
            .await
            .unwrap()
            .is_empty(),
        "tenant-b FTS should still match"
    );
}

#[tokio::test]
async fn delete_source_cascades_to_documents_and_chunks() {
    let (_dir, db) = open_db().await;
    seed_store(&db, "tenant-c", 4).await;

    let handle = db.retrieval_store("tenant-c").await.unwrap();
    assert_eq!(handle.stats().await.unwrap().chunk_count, 4);

    assert!(db.delete_source("src-tenant-c").await.unwrap());

    let stats = handle.stats().await.unwrap();
    assert_eq!(
        stats.chunk_count, 0,
        "chunks should cascade when their source is deleted"
    );
    assert_eq!(
        stats.document_count, 0,
        "documents should cascade when their source is deleted"
    );
    assert!(
        handle
            .bm25_search("alpha", 10, &[])
            .await
            .unwrap()
            .is_empty(),
        "FTS should reflect the cascade"
    );
}
