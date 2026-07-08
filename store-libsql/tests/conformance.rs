use tempfile::tempdir;

use localdb_core::block::{Block, BlockKind, BlockLocation};
use localdb_core::store::conformance;
use localdb_core::store::{ChunkRecord, MetadataFilter};
use localdb_core::types::{SourceKind, Span, StoreVisibility};
use localdb_core::{Error, SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
use store_libsql::SqliteBackend;

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

    // Each store needs its own source so the composite FK
    // `FOREIGN KEY (store_id, source_id) REFERENCES sources(store_id, id)`
    // is satisfied when chunks are upserted into any of the three stores.
    for (src_id, store_id, root) in [
        ("src-1", "store-1", "/test/conformance"),
        ("src-A", "store-A", "/test/conformance-A"),
        ("src-B", "store-B", "/test/conformance-B"),
    ] {
        backend
            .upsert_source(&SourceRow {
                id: src_id.to_string(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                root: Some(root.to_string()),
                url: None,
                include: vec![],
                exclude: vec![],
                preset: "prose".to_string(),
                refresh: None,
                created_at: "2026-06-25T12:00:00Z".to_string(),
            })
            .await
            .unwrap();
    }

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
async fn delete_by_resource() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_delete_by_resource(handle.as_ref()).await;
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
async fn window_block_seqs_round_trip() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_window_block_seqs_round_trip(handle.as_ref()).await;
}

#[tokio::test]
async fn get_chunks_for_resource() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_get_chunks_for_resource(handle.as_ref()).await;
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
async fn bm25_search_special_chars_does_not_error() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    handle
        .upsert_chunks(vec![make_record(
            "special-c1",
            "special-doc-1",
            "store-1",
            vec![1.0, 0.0],
        )])
        .await
        .unwrap();

    for query in [
        "foo-bar",
        "C++",
        "path/to/file",
        "hello (world)",
        "it's",
        "",
    ] {
        let result = handle.bm25_search(query, 10, &[]).await;
        assert!(
            result.is_ok(),
            "BM25 search for {query:?} should not error, got: {result:?}"
        );
    }
}

#[tokio::test]
async fn dense_search_with_filter_returns_matching_chunks() {
    let (_dir, db) = setup().await;

    for src_id in ["other", "target"] {
        db.upsert_source(&SourceRow {
            id: src_id.to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Path,
            root: Some(format!("/test/{src_id}")),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: "2026-06-25T12:00:00Z".to_string(),
        })
        .await
        .unwrap();
    }

    let handle = db.retrieval_store("store-1").await.unwrap();
    let mut records = Vec::new();
    for i in 0..18 {
        let mut chunk = make_record(
            &format!("other-{i}"),
            &format!("doc-other-{i}"),
            "store-1",
            vec![0.9 + (i as f32) * 0.005, 0.1],
        );
        chunk.source_id = "other".to_string();
        records.push(chunk);
    }
    for i in 0..2 {
        let mut chunk = make_record(
            &format!("target-{i}"),
            &format!("doc-target-{i}"),
            "store-1",
            vec![0.0, 1.0],
        );
        chunk.source_id = "target".to_string();
        records.push(chunk);
    }
    handle.upsert_chunks(records).await.unwrap();

    let filter = vec![MetadataFilter::SourceId("target".to_string())];
    let results = handle.dense_search(&[1.0, 0.0], 2, &filter).await.unwrap();
    assert_eq!(results.len(), 2);
    let ids: Vec<&str> = results.iter().map(|r| r.chunk.id.as_str()).collect();
    assert!(ids.contains(&"target-0") && ids.contains(&"target-1"));
}

#[tokio::test]
async fn reopen_with_same_encoding_succeeds() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = SqliteBackend::open(StoreBackendConfig::local_path(
        path.clone(),
        2,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    for store_id in ["store-1"] {
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
    }
    db.upsert_source(&SourceRow {
        id: "src-1".to_string(),
        store_id: "store-1".to_string(),
        kind: SourceKind::Path,
        root: Some("/test/reopen".to_string()),
        url: None,
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: "2026-06-25T12:00:00Z".to_string(),
    })
    .await
    .unwrap();
    db.retrieval_store("store-1")
        .await
        .unwrap()
        .upsert_chunks(vec![make_record(
            "persist-c1",
            "persist-doc-1",
            "store-1",
            vec![1.0, 0.0],
        )])
        .await
        .unwrap();
    drop(db);

    let reopened = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        2,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    let stats = reopened
        .retrieval_store("store-1")
        .await
        .unwrap()
        .stats()
        .await
        .unwrap();
    assert_eq!(stats.chunk_count, 1);
}

#[tokio::test]
async fn reopen_with_different_encoding_errors() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = SqliteBackend::open(StoreBackendConfig::local_path(
        path.clone(),
        2,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    drop(db);

    match SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        2,
        VectorEncoding::Binary,
    ))
    .await
    {
        Err(Error::InvalidConfig { message }) => assert!(message.contains("mismatch")),
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!("expected InvalidConfig"),
    }
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
async fn replace_document() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_replace_document(handle.as_ref()).await;
}

#[tokio::test]
async fn replace_same_resource_id() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();
    conformance::test_replace_same_resource_id(handle.as_ref()).await;
}

/// Issue #79 (WI-1): the replace delete must run *inside* the same
/// transaction as the replacement upsert. This test forces a write failure
/// (an FK violation on the replacement resource) that fires *after* the
/// in-transaction delete of the old document but before COMMIT, and asserts
/// the old document survives — i.e. the delete rolled back along with the
/// failed insert rather than having already committed on its own.
#[tokio::test]
async fn replace_rolls_back_old_document_on_write_failure() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();

    // Seed document A.
    let doc_a = make_record("chunk-a1", "doc-a", "store-1", vec![1.0, 0.0]);
    handle.upsert_chunks(vec![doc_a]).await.unwrap();

    // Attempt to replace doc-a with doc-b, but doc-b's chunk references a
    // source_id that does not exist for this store. The composite FK
    // `resources(store_id, source_id) REFERENCES sources(store_id, id)`
    // rejects the INSERT INTO resources for doc-b, which fires inside the
    // transaction, after the in-transaction delete of doc-a would have run.
    let mut doc_b = make_record("chunk-b1", "doc-b", "store-1", vec![0.0, 1.0]);
    doc_b.source_id = "nonexistent-src".to_string();

    let result = handle
        .upsert_chunks_and_blocks("store-1", "doc-b", vec![doc_b], &[], Some("doc-a"))
        .await;
    assert!(
        result.is_err(),
        "FK violation on the replacement insert should surface as an error"
    );

    // doc-a's chunk must still be present: the delete must have rolled back
    // along with the failed insert, not already committed on its own.
    let remaining_a = handle.get_chunks_for_resource("doc-a").await.unwrap();
    assert_eq!(
        remaining_a.len(),
        1,
        "doc-a should still be present after a failed replace"
    );
    assert_eq!(remaining_a[0].id, "chunk-a1");

    // doc-b must be absent — the failed insert must not have partially landed.
    let remaining_b = handle.get_chunks_for_resource("doc-b").await.unwrap();
    assert!(
        remaining_b.is_empty(),
        "doc-b should not have been inserted after a failed replace"
    );
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
    assert!(
        matches!(result, Err(Error::InvalidRequest { .. })),
        "expected InvalidRequest for ambiguous cross-store document; got: {:?}",
        result.err()
    );
}

/// Coverage: a per-record write failure *inside* the `upsert_chunks`
/// transaction (as opposed to the pre-`BEGIN` tenant-ownership check) must
/// roll back the whole batch, including records that would otherwise have
/// landed successfully earlier in the same call. Forces an FK violation
/// (nonexistent `source_id`) on the second record.
#[tokio::test]
async fn upsert_chunks_rolls_back_batch_on_write_failure() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();

    let good = make_record("chunk-good", "doc-good", "store-1", vec![1.0, 0.0]);
    let mut bad = make_record("chunk-bad", "doc-bad", "store-1", vec![0.0, 1.0]);
    bad.source_id = "nonexistent-src".to_string();

    let result = handle.upsert_chunks(vec![good, bad]).await;
    assert!(
        result.is_err(),
        "FK violation on the second record should surface as an error"
    );

    let stats = handle.stats().await.unwrap();
    assert_eq!(
        stats.chunk_count, 0,
        "the whole batch must roll back, including the earlier valid record"
    );
}

/// Coverage: the standalone `RetrievalStore::upsert_blocks` trait method
/// (not routed through `upsert_chunks_and_blocks`) writes rows into `blocks`
/// and supports the `ON CONFLICT ... DO UPDATE` re-upsert path. Exercises
/// both the `Some(location)` and `None` location branches.
#[tokio::test]
async fn upsert_blocks_direct_through_trait() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();

    // The resource row must already exist (written by upsert_chunks).
    handle
        .upsert_chunks(vec![make_record(
            "chunk-1",
            "doc-blocks",
            "store-1",
            vec![1.0, 0.0],
        )])
        .await
        .unwrap();

    let blocks = vec![
        Block {
            seq: 0,
            kind: BlockKind::Paragraph,
            text: "first block".to_string(),
            location: Some(BlockLocation {
                line_start: Some(1),
                ..Default::default()
            }),
        },
        Block {
            seq: 1,
            kind: BlockKind::Heading { level: 2 },
            text: "second block".to_string(),
            location: None,
        },
    ];

    handle
        .upsert_blocks("store-1", "doc-blocks", &blocks)
        .await
        .unwrap();

    // Re-upsert with changed text to exercise the ON CONFLICT DO UPDATE path.
    let mut updated = blocks;
    updated[0].text = "first block, updated".to_string();
    handle
        .upsert_blocks("store-1", "doc-blocks", &updated)
        .await
        .unwrap();
}

/// Coverage: `upsert_chunks_and_blocks` performs the same pre-`BEGIN`
/// tenant-ownership check as `upsert_chunks` — a cross-tenant record must be
/// rejected before any write is attempted.
#[tokio::test]
async fn upsert_chunks_and_blocks_rejects_cross_tenant_record() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-A").await.unwrap();
    let result = handle
        .upsert_chunks_and_blocks(
            "store-A",
            "doc-cross",
            vec![make_record(
                "chunk-cross",
                "doc-cross",
                "store-B",
                vec![0.5, 0.5],
            )],
            &[],
            None,
        )
        .await;
    assert!(matches!(
        result,
        Err(Error::Internal {
            correlation_id,
            ..
        }) if correlation_id == "store_handle_tenant_violation"
    ));
}

/// Coverage: `upsert_chunks_and_blocks` with a non-empty `blocks` slice writes
/// both chunks and blocks inside the same transaction (all prior conformance
/// tests exercise this call with `blocks: &[]`, which never touches the block
/// insert loop).
#[tokio::test]
async fn upsert_chunks_and_blocks_writes_blocks_in_same_transaction() {
    let (_dir, db) = setup().await;
    let handle = db.retrieval_store("store-1").await.unwrap();

    let records = vec![make_record(
        "chunk-1",
        "doc-with-blocks",
        "store-1",
        vec![1.0, 0.0],
    )];
    let blocks = vec![
        Block {
            seq: 0,
            kind: BlockKind::Paragraph,
            text: "body text".to_string(),
            location: Some(BlockLocation {
                page: Some(1),
                ..Default::default()
            }),
        },
        Block {
            seq: 1,
            kind: BlockKind::Quote,
            text: "quoted text".to_string(),
            location: None,
        },
    ];

    let written = handle
        .upsert_chunks_and_blocks("store-1", "doc-with-blocks", records, &blocks, None)
        .await
        .unwrap();
    assert_eq!(written, 1);
}

/// Coverage: `upsert_chunks_inner`'s encoding match dispatches to
/// `f32_to_vector1bit_sql` for `VectorEncoding::Binary` stores — all other
/// conformance tests use the default `Float32` encoding.
#[tokio::test]
async fn upsert_chunks_with_binary_encoding_round_trips() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        2,
        VectorEncoding::Binary,
    ))
    .await
    .unwrap();
    db.upsert_store(&StoreRow {
        id: "store-1".to_string(),
        name: "store-1".to_string(),
        visibility: StoreVisibility::Private,
        backend: "libsql".to_string(),
        indexing_policy: "{}".to_string(),
        policy_version: "v1".to_string(),
        acl: "{}".to_string(),
        created_at: "2026-06-25T12:00:00Z".to_string(),
    })
    .await
    .unwrap();
    db.upsert_source(&SourceRow {
        id: "src-1".to_string(),
        store_id: "store-1".to_string(),
        kind: SourceKind::Path,
        root: Some("/test/binary".to_string()),
        url: None,
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: "2026-06-25T12:00:00Z".to_string(),
    })
    .await
    .unwrap();

    let handle = db.retrieval_store("store-1").await.unwrap();
    let written = handle
        .upsert_chunks(vec![make_record(
            "bin-chunk-1",
            "bin-doc-1",
            "store-1",
            vec![1.0, -1.0],
        )])
        .await
        .unwrap();
    assert_eq!(written, 1);

    let stats = handle.stats().await.unwrap();
    assert_eq!(stats.chunk_count, 1);
}

fn make_record(id: &str, doc_id: &str, store_id: &str, embedding: Vec<f32>) -> ChunkRecord {
    let text = format!("text for {id}");
    // Derive a source_id that belongs to this store:
    //   store-1 → src-1,  store-A → src-A,  store-B → src-B.
    // This matches the sources created in setup() and satisfies the composite
    // FK  FOREIGN KEY (store_id, source_id) REFERENCES sources(store_id, id).
    let source_id = store_id.replacen("store-", "src-", 1);
    ChunkRecord {
        id: id.to_string(),
        resource_id: doc_id.to_string(),
        store_id: store_id.to_string(),
        text: text.clone(),
        span: Span::new(0, text.len()),
        heading_path: vec![],
        embedding,
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-25T12:00:00Z".to_string(),
        content_hash: "abc123".to_string(),
        origin_store: store_id.to_string(),
        source_id,
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: format!("file:///{store_id}/{doc_id}.md"),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        window_block_seqs: vec![],
    }
}
