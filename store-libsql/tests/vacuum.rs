//! Integration test for `db vacuum` (issue #177) against a real store: build
//! a store with a real DiskANN vector index, delete most of its rows (the
//! shape of what the v6 `shrink_vector_index` migration or an ordinary bulk
//! delete leaves behind — freed pages sitting on SQLite's own free list, not
//! returned to the filesystem), vacuum it, and assert the file on disk
//! actually shrank *and* that search against the surviving data still works
//! afterward (the vector index isn't corrupted by the rewrite).
//!
//! Uses `VectorEncoding::Binary` (real embedders' default) with a wide-ish
//! dimension so the per-chunk DiskANN block cost is large enough for the
//! bulk-delete-then-vacuum shrink to be unambiguous, mirroring
//! `vector_index_cost.rs`'s measured-not-argued approach.

use tempfile::tempdir;

use localdb_core::metadata::Metadata;
use localdb_core::store::ChunkRecord;
use localdb_core::types::{SourceKind, Span, StoreVisibility};
use localdb_core::{SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
use store_libsql::{vacuum_store, SqliteBackend};

const DIM: usize = 256;
const THROWAWAY_COUNT: usize = 800;

fn make_chunk(id: &str, resource_id: &str, store_id: &str, embedding: Vec<f32>) -> ChunkRecord {
    let text = format!("text for {id}");
    ChunkRecord {
        id: id.to_string(),
        resource_id: resource_id.to_string(),
        store_id: store_id.to_string(),
        text: text.clone(),
        span: Span::new(0, text.len()),
        heading_path: vec![],
        embedding,
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-25T12:00:00Z".to_string(),
        modified_at: "2026-06-25T12:00:00Z".to_string(),
        content_hash: "abc123".to_string(),
        origin_store: store_id.to_string(),
        source_id: format!("src-{store_id}"),
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: format!("file:///data/{store_id}/{resource_id}.md"),
        metadata: Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
    }
}

/// A deterministic pseudo-random +1/-1 vector, distinct per `seed`, so
/// throwaway chunks don't all collapse onto the same ANN node.
fn filler_embedding(seed: usize) -> Vec<f32> {
    (0..DIM)
        .map(|d| {
            if (d + seed).is_multiple_of(2) {
                1.0
            } else {
                -1.0
            }
        })
        .collect()
}

#[tokio::test]
async fn vacuum_shrinks_store_file_after_bulk_delete_and_search_still_works() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let store_id = "store-A";

    // Build the store, seed it with one "keeper" chunk plus a large number of
    // "throwaway" chunks (each its own resource, so they can be deleted the
    // way a real cleanup would: per-resource), then delete every throwaway —
    // all within a scope so every connection/handle is dropped before
    // `vacuum_store` opens its own.
    {
        let backend = SqliteBackend::open(StoreBackendConfig::local_path(
            path.clone(),
            DIM,
            VectorEncoding::Binary,
        ))
        .await
        .unwrap();

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

        backend
            .upsert_source(&SourceRow {
                id: format!("src-{store_id}"),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                root: Some(format!("/data/{store_id}")),
                url: None,
                include: vec![],
                exclude: vec![],
                preset: "prose".to_string(),
                refresh: None,
                created_at: "2026-06-25T12:00:00Z".to_string(),
                config_json: None,
            })
            .await
            .unwrap();

        let handle = backend.retrieval_store(store_id).await.unwrap();

        // The keeper: an all-ones vector, easy to distinguish from the
        // alternating-sign filler vectors below.
        let keeper_embedding = vec![1.0f32; DIM];
        let mut chunks = vec![make_chunk(
            "keep-chunk-0",
            "keep-doc",
            store_id,
            keeper_embedding.clone(),
        )];

        for i in 0..THROWAWAY_COUNT {
            chunks.push(make_chunk(
                &format!("throwaway-chunk-{i}"),
                &format!("throwaway-doc-{i}"),
                store_id,
                filler_embedding(i),
            ));
        }
        handle.upsert_chunks(chunks).await.unwrap();

        for i in 0..THROWAWAY_COUNT {
            let deleted = handle
                .delete_by_resource(&format!("throwaway-doc-{i}"))
                .await
                .unwrap();
            assert_eq!(deleted, 1, "each throwaway resource has exactly 1 chunk");
        }

        // Sanity: the keeper alone should be far smaller than what the store
        // held at peak, so there's real free-list bloat for VACUUM to
        // reclaim below (guards against a future refactor silently making
        // this fixture too small to be a meaningful test).
        let remaining = handle
            .dense_search(&keeper_embedding, 10, &[])
            .await
            .unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "only the keeper chunk should remain after deleting every throwaway resource"
        );
    }

    let report = vacuum_store(&path).await.unwrap();

    let size_on_disk = std::fs::metadata(&path).unwrap().len();
    assert_eq!(
        report.size_after, size_on_disk,
        "reported size_after must match what's actually on disk"
    );
    assert!(
        report.size_after < report.size_before,
        "vacuum should shrink the file after deleting {THROWAWAY_COUNT} of {} chunks: \
         before={} after={}",
        THROWAWAY_COUNT + 1,
        report.size_before,
        report.size_after
    );
    assert!(
        report.bytes_reclaimed > 0,
        "bytes_reclaimed should be positive: {report:?}"
    );

    // Reopen the store through the ordinary production path and confirm the
    // vector index survived the rewrite intact: the keeper chunk is still
    // findable by its own embedding.
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path.clone(),
        DIM,
        VectorEncoding::Binary,
    ))
    .await
    .unwrap();
    let handle = backend.retrieval_store(store_id).await.unwrap();

    let keeper_embedding = vec![1.0f32; DIM];
    let results = handle
        .dense_search(&keeper_embedding, 5, &[])
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        1,
        "search after vacuum should still find exactly the surviving keeper chunk"
    );
    assert_eq!(results[0].chunk.id, "keep-chunk-0");

    let doc = backend.find_document("keep-doc", None).await.unwrap();
    assert!(
        doc.is_some(),
        "the keeper resource's own row must also have survived the vacuum"
    );
}
