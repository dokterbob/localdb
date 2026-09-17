use super::conformance::*;
use super::*;

fn make_test_record(id: &str, doc_id: &str, text: &str, embedding: Vec<f32>) -> ChunkRecord {
    ChunkRecord {
        id: id.to_string(),
        resource_id: doc_id.to_string(),
        store_id: "test-store".to_string(),
        text: text.to_string(),
        span: Span::new(0, text.len()),
        heading_path: vec![],
        embedding,
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        modified_at: Some("2026-06-10T12:00:00Z".to_string()),
        content_hash: "abc123".to_string(),
        origin_store: "test-store".to_string(),
        source_id: "src-1".to_string(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: "file:///test.md".to_string(),
        metadata: crate::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    }
}

#[tokio::test]
async fn fake_store_upsert_and_stats() {
    let store = FakeStore::new();
    test_upsert_and_stats(&store).await;
}

#[tokio::test]
async fn fake_store_upsert_replaces_existing() {
    let store = FakeStore::new();
    test_upsert_replaces_existing(&store).await;
}

#[tokio::test]
async fn fake_store_delete_by_resource() {
    let store = FakeStore::new();
    test_delete_by_resource(&store).await;
}

#[tokio::test]
async fn fake_store_delete_nonexistent_document() {
    let store = FakeStore::new();
    test_delete_nonexistent_document(&store).await;
}

#[tokio::test]
async fn fake_store_replace_document() {
    let store = FakeStore::new();
    test_replace_document(&store).await;
}

#[tokio::test]
async fn fake_store_replace_same_resource_id() {
    let store = FakeStore::new();
    test_replace_same_resource_id(&store).await;
}

#[tokio::test]
async fn fake_store_dense_search_round_trip() {
    let store = FakeStore::new();
    test_dense_search_round_trip(&store).await;
}

#[tokio::test]
async fn fake_store_bm25_search_round_trip() {
    let store = FakeStore::new();
    test_bm25_search_round_trip(&store).await;
}

#[tokio::test]
async fn fake_store_metadata_filter_mime() {
    let store = FakeStore::new();
    test_metadata_filter_mime(&store).await;
}

#[tokio::test]
async fn fake_store_metadata_filter_uri_prefix() {
    let store = FakeStore::new();
    test_metadata_filter_uri_prefix(&store).await;
}

#[tokio::test]
async fn fake_store_metadata_filter_and_combination() {
    let store = FakeStore::new();
    test_metadata_filter_and_combination(&store).await;
}

#[tokio::test]
async fn fake_store_date_filter_null_axis_value_excluded() {
    let store = FakeStore::new();
    test_date_filter_null_axis_value_excluded(&store).await;
}

#[tokio::test]
async fn fake_store_date_filter_partial_bound_on_timestamp_axis() {
    let store = FakeStore::new();
    test_date_filter_partial_bound_on_timestamp_axis(&store).await;
}

#[tokio::test]
async fn fake_store_date_filter_document_axis_partial_precision_widening() {
    let store = FakeStore::new();
    test_date_filter_document_axis_partial_precision_widening(&store).await;
}

#[tokio::test]
async fn fake_store_date_filter_per_axis_round_trip() {
    let store = FakeStore::new();
    test_date_filter_per_axis_round_trip(&store).await;
}

#[tokio::test]
async fn fake_store_get_chunk() {
    let store = FakeStore::new();
    test_get_chunk(&store).await;
}

#[tokio::test]
async fn fake_store_get_chunks_for_resource() {
    let store = FakeStore::new();
    test_get_chunks_for_resource(&store).await;
}

#[tokio::test]
async fn fake_store_delete_by_store() {
    let store = FakeStore::new();
    test_delete_by_store(&store).await;
}

#[tokio::test]
async fn fake_store_dense_search_limit() {
    let store = FakeStore::new();
    test_dense_search_limit(&store).await;
}

#[tokio::test]
async fn fake_store_bm25_search_limit() {
    let store = FakeStore::new();
    test_bm25_search_limit(&store).await;
}

#[tokio::test]
async fn fake_store_window_block_seqs_round_trip() {
    let store = FakeStore::new();
    test_window_block_seqs_round_trip(&store).await;
}

#[tokio::test]
async fn fake_store_page_round_trip() {
    let store = FakeStore::new();
    test_page_round_trip(&store).await;
}

#[tokio::test]
async fn fake_store_blocks_round_trip_ordered() {
    let store = FakeStore::new();
    test_blocks_round_trip_ordered(&store).await;
}

#[tokio::test]
async fn fake_store_empty_stats() {
    let store = FakeStore::new();
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.chunk_count, 0);
    assert_eq!(stats.document_count, 0);
}

#[tokio::test]
async fn fake_store_dense_search_empty() {
    let store = FakeStore::new();
    let results = store.dense_search(&[1.0, 0.0], 10, &[]).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn fake_store_bm25_search_empty() {
    let store = FakeStore::new();
    let results = store.bm25_search("test", 10, &[]).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn fake_store_dense_search_sorted_descending() {
    let store = FakeStore::new();
    let records = vec![
        make_test_record("a", "doc-1", "text a", vec![0.0, 1.0]),
        make_test_record("b", "doc-1", "text b", vec![1.0, 0.0]),
        make_test_record("c", "doc-1", "text c", vec![0.707, 0.707]),
    ];
    store.upsert_chunks(records).await.unwrap();

    let results = store.dense_search(&[1.0, 0.0], 3, &[]).await.unwrap();
    assert_eq!(results.len(), 3);
    // Scores should be descending
    assert!(results[0].score >= results[1].score);
    assert!(results[1].score >= results[2].score);
    // chunk b should be first (closest to [1.0, 0.0])
    assert_eq!(results[0].chunk.id, "b");
}

#[tokio::test]
async fn chunk_record_from_chunk_helper() {
    use crate::types::{Chunk, Provenance, SourceRef};

    let chunk = Chunk {
        id: "chunk-id".to_string(),
        resource_id: "doc-id".to_string(),
        store_id: "store-id".to_string(),
        text: "Some text".to_string(),
        span: Span::new(0, 9),
        heading_path: vec!["Heading".to_string()],
        policy_version: "policy-v1".to_string(),
        provenance: Provenance {
            origin_store: "store-id".to_string(),
            source_ref: SourceRef {
                id: "source-id".to_string(),
                kind: "path".to_string(),
            },
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            share_path: vec![],
        },
        window_block_seqs: vec![7, 8],
    };

    let record = ChunkRecord::from_chunk(
        &chunk,
        vec![0.1, 0.2, 0.3],
        "file:///test.md".to_string(),
        Some("text/markdown".to_string()),
        crate::metadata::Metadata::default(),
    );

    assert_eq!(record.id, "chunk-id");
    assert_eq!(record.resource_id, "doc-id");
    assert_eq!(record.store_id, "store-id");
    assert_eq!(record.text, "Some text");
    assert_eq!(record.embedding, vec![0.1, 0.2, 0.3]);
    assert_eq!(record.uri, "file:///test.md");
    assert_eq!(record.mime, Some("text/markdown".to_string()));
    assert_eq!(record.source_id, "source-id");
    assert_eq!(record.ingestor_kind, "path");
    assert_eq!(record.window_block_seqs, vec![7, 8]);
}

#[tokio::test]
async fn cosine_similarity_known_values() {
    // Identical vectors → 1.0
    assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    // Orthogonal vectors → 0.0
    assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-6);
    // Zero vector → 0.0
    assert!((cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]) - 0.0).abs() < 1e-6);
}

#[tokio::test]
async fn metadata_filter_fetched_after() {
    let store = FakeStore::new();
    let mut r1 = make_test_record("old", "doc-1", "old text", vec![1.0, 0.0]);
    r1.fetched_at = "2026-01-01T00:00:00Z".to_string();
    let mut r2 = make_test_record("new", "doc-2", "new text", vec![0.5, 0.5]);
    r2.fetched_at = "2026-06-10T00:00:00Z".to_string();

    store.upsert_chunks(vec![r1, r2]).await.unwrap();

    let filter = vec![MetadataFilter::DateAfter {
        axis: DateAxis::Added,
        value: "2026-03-01T00:00:00Z".to_string(),
    }];
    let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].chunk.id, "new");
}

#[tokio::test]
async fn metadata_filter_source_id() {
    let store = FakeStore::new();
    let mut r1 = make_test_record("chunk-1", "doc-1", "source A text", vec![1.0, 0.0]);
    r1.source_id = "source-A".to_string();
    let mut r2 = make_test_record("chunk-2", "doc-2", "source B text", vec![0.5, 0.5]);
    r2.source_id = "source-B".to_string();

    store.upsert_chunks(vec![r1, r2]).await.unwrap();

    let filter = vec![MetadataFilter::SourceId("source-A".to_string())];
    let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].chunk.id, "chunk-1");
}

#[tokio::test]
async fn metadata_filter_policy_version() {
    let store = FakeStore::new();
    let mut r1 = make_test_record("chunk-1", "doc-1", "v1 text", vec![1.0, 0.0]);
    r1.policy_version = "policy-v1".to_string();
    let mut r2 = make_test_record("chunk-2", "doc-2", "v2 text", vec![0.5, 0.5]);
    r2.policy_version = "policy-v2".to_string();

    store.upsert_chunks(vec![r1, r2]).await.unwrap();

    let filter = vec![MetadataFilter::PolicyVersion("policy-v1".to_string())];
    let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].chunk.id, "chunk-1");
}

#[test]
fn metadata_filter_matches_all_variants() {
    let record = make_test_record("chunk-1", "doc-1", "text", vec![1.0, 0.0]);

    assert!(MetadataFilter::Mime("text/plain".to_string()).matches(&record));
    assert!(!MetadataFilter::Mime("text/html".to_string()).matches(&record));

    assert!(MetadataFilter::UriPrefix("file:///".to_string()).matches(&record));
    assert!(!MetadataFilter::UriPrefix("https://".to_string()).matches(&record));

    assert!(MetadataFilter::DateAfter {
        axis: DateAxis::Added,
        value: "2026-06-01T00:00:00Z".to_string(),
    }
    .matches(&record));
    assert!(!MetadataFilter::DateAfter {
        axis: DateAxis::Added,
        value: "2026-06-11T00:00:00Z".to_string(),
    }
    .matches(&record));

    assert!(MetadataFilter::DateBefore {
        axis: DateAxis::Added,
        value: "2026-07-01T00:00:00Z".to_string(),
    }
    .matches(&record));
    assert!(!MetadataFilter::DateBefore {
        axis: DateAxis::Added,
        value: "2026-06-01T00:00:00Z".to_string(),
    }
    .matches(&record));

    assert!(MetadataFilter::SourceId("src-1".to_string()).matches(&record));
    assert!(!MetadataFilter::SourceId("src-2".to_string()).matches(&record));

    assert!(MetadataFilter::ResourceId("doc-1".to_string()).matches(&record));
    assert!(!MetadataFilter::ResourceId("doc-2".to_string()).matches(&record));

    assert!(MetadataFilter::PolicyVersion("v1".to_string()).matches(&record));
    assert!(!MetadataFilter::PolicyVersion("v2".to_string()).matches(&record));
}

#[tokio::test]
async fn fake_store_touch_resource_checked_surfaces_in_list_indexed_documents() {
    let store = FakeStore::new();
    let record = make_test_record("chunk-1", "doc-1", "some text", vec![1.0, 0.0]);
    store.upsert_chunks(vec![record]).await.unwrap();

    let before = store.list_indexed_documents().await.unwrap();
    assert_eq!(
        before[0].last_checked_at, None,
        "a resource that was never touched must report last_checked_at: None"
    );
    assert_eq!(store.last_checked_at("doc-1").await, None);

    store
        .touch_resource_checked("test-store", "doc-1")
        .await
        .unwrap();

    assert!(
        store.last_checked_at("doc-1").await.is_some(),
        "the test-support accessor must surface the touch"
    );
    let after = store.list_indexed_documents().await.unwrap();
    assert_eq!(
        after[0].last_checked_at,
        store.last_checked_at("doc-1").await,
        "list_indexed_documents must surface the same value the accessor reports"
    );
}

#[tokio::test]
async fn fake_store_touch_resource_checked_errors_for_missing_resource() {
    let store = FakeStore::new();

    let err = store
        .touch_resource_checked("test-store", "does-not-exist")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::ResourceNotFound {
            id: "does-not-exist".to_string()
        }
    );
}

#[tokio::test]
async fn fake_search_identity_preserves_float32_bits_on_both_legs() {
    let store = FakeStore::new();
    let vectors = [
        vec![1.0, 0.0],
        vec![1.0, -0.0],
        vec![f32::from_bits(1.0f32.to_bits() + 1), 1.0],
        vec![f32::from_bits(1.0f32.to_bits() + 2), 1.0],
    ];
    let chunks = vectors
        .iter()
        .enumerate()
        .map(|(i, vector)| make_test_record(&i.to_string(), "doc", "query", vector.clone()))
        .collect();
    store.upsert_chunks(chunks).await.unwrap();
    let dense = store.dense_search(&[1.0, 0.0], 50, &[]).await.unwrap();
    let bm25 = store.bm25_search("query", 50, &[]).await.unwrap();
    for results in [dense, bm25] {
        let identities: Vec<_> = (0..vectors.len())
            .map(|i| {
                results
                    .iter()
                    .find(|r| r.chunk.id == i.to_string())
                    .unwrap()
                    .embedding_identity
                    .as_ref()
                    .unwrap()
            })
            .collect();
        assert_ne!(identities[0], identities[1]);
        assert_ne!(identities[2], identities[3]);
        for (i, identity) in identities.into_iter().enumerate() {
            assert_eq!(identity.format, "fake-f32-le-v1");
            assert_eq!(identity.encoding, crate::embedder::VectorEncoding::Float32);
            assert_eq!(identity.dimensions, 2);
            let expected: Vec<_> = vectors[i]
                .iter()
                .flat_map(|v| v.to_bits().to_le_bytes())
                .collect();
            assert_eq!(identity.bytes.as_ref(), expected);
        }
    }
}

#[tokio::test]
async fn fake_search_ties_sort_by_store_and_chunk_before_limit() {
    let store = FakeStore::new();
    let mut chunks = Vec::new();
    for (store_id, chunk_id) in [("b", "z"), ("a", "y"), ("a", "x")] {
        let mut chunk = make_test_record(chunk_id, "doc", "query", vec![1.0, 0.0]);
        chunk.store_id = store_id.into();
        chunks.push(chunk);
    }
    store.upsert_chunks(chunks).await.unwrap();
    for results in [
        store.dense_search(&[1.0, 0.0], 2, &[]).await.unwrap(),
        store.bm25_search("query", 2, &[]).await.unwrap(),
    ] {
        assert_eq!(
            results
                .iter()
                .map(|r| (r.chunk.store_id.as_str(), r.chunk.id.as_str()))
                .collect::<Vec<_>>(),
            [("a", "x"), ("a", "y")]
        );
        assert_eq!(results[0].embedding_identity, results[1].embedding_identity);
    }
}
