//! Regression tests for store-libsql fixes.
//!
//! Each test targets a specific bug fix and exercises the observable behavior
//! through the `RetrievalStore` trait.

use localdb_core::parser::DocumentMetadata;
use localdb_core::store::{ChunkRecord, MetadataFilter, RetrievalStore};
use localdb_core::types::Span;
use localdb_core::VectorEncoding;
use store_libsql::LibsqlStore;
use tempfile::TempDir;

const DIM: usize = 4;

/// Create a fresh store in a temporary directory.
async fn fresh_store() -> (TempDir, LibsqlStore) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.db");
    let store = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();
    (tmp, store)
}

/// Build a `ChunkRecord` for testing with 4-dim embeddings.
fn make_chunk(
    id: &str,
    document_id: &str,
    store_id: &str,
    source_id: &str,
    text: &str,
    embedding: Vec<f32>,
) -> ChunkRecord {
    ChunkRecord {
        id: id.to_string(),
        document_id: document_id.to_string(),
        store_id: store_id.to_string(),
        text: text.to_string(),
        span: Span::new(0, text.len()),
        heading_path: vec![],
        embedding,
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: "abc123".to_string(),
        origin_store: store_id.to_string(),
        source_id: source_id.to_string(),
        source_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: "file:///test.md".to_string(),
        metadata: DocumentMetadata::default(),
    }
}

// ---------------------------------------------------------------------------
// Test 1: FTS5 query with special characters doesn't crash
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bm25_search_special_chars_does_not_error() {
    let (_tmp, store) = fresh_store().await;

    // Insert a document with normal text so the FTS index is populated.
    let record = make_chunk(
        "c1",
        "doc-1",
        "store-1",
        "src-1",
        "The quick brown fox jumps over the lazy dog",
        vec![1.0, 0.0, 0.0, 0.0],
    );
    store.upsert_chunks(vec![record]).await.unwrap();

    // Each query with special characters must return Ok (possibly empty), not Err.
    let special_queries = [
        "foo-bar",
        "C++",
        "path/to/file",
        "hello (world)",
        "it's",
        "", // empty string
    ];

    for query in special_queries {
        let result = store.bm25_search(query, 10, &[]).await;
        assert!(
            result.is_ok(),
            "BM25 search for {query:?} should not error, got: {result:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: Dense search with selective metadata filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dense_search_with_filter_returns_matching_chunks() {
    let (_tmp, store) = fresh_store().await;

    // Insert 18 "other" chunks with embeddings very close to our query vector.
    // These would normally dominate the top results without filtering.
    let mut records = Vec::new();
    for i in 0..18 {
        let mut chunk = make_chunk(
            &format!("other-{i}"),
            &format!("doc-other-{i}"),
            "store-1",
            "other",
            &format!("other chunk number {i}"),
            // Embeddings close to the query vector [1, 0, 0, 0]
            vec![0.9 + (i as f32) * 0.005, 0.1, 0.0, 0.0],
        );
        chunk.uri = format!("file:///other-{i}.md");
        records.push(chunk);
    }

    // Insert 2 "target" chunks with distant embeddings (they would not appear in top-3 unfiltered).
    for i in 0..2 {
        let mut chunk = make_chunk(
            &format!("target-{i}"),
            &format!("doc-target-{i}"),
            "store-1",
            "target",
            &format!("target chunk number {i}"),
            // Embeddings orthogonal to query vector — low similarity
            vec![0.0, 0.0, 0.0, 1.0],
        );
        chunk.uri = format!("file:///target-{i}.md");
        records.push(chunk);
    }

    store.upsert_chunks(records).await.unwrap();

    // Dense search with source_id filter for "target", limit 2.
    let filter = vec![MetadataFilter::SourceId("target".to_string())];
    let results = store.dense_search(&[1.0, 0.0, 0.0, 0.0], 2, &filter).await.unwrap();

    assert_eq!(
        results.len(),
        2,
        "should return exactly 2 target chunks, got {}",
        results.len()
    );

    let ids: Vec<&str> = results.iter().map(|r| r.chunk.id.as_str()).collect();
    assert!(
        ids.contains(&"target-0") && ids.contains(&"target-1"),
        "both target chunks should be returned, got: {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Schema mismatch on reopen
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reopen_with_different_encoding_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.db");

    // Open with Float32
    let store = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();
    drop(store);

    // Reopen with Binary — should fail with an error indicating config mismatch.
    let result = LibsqlStore::open(&path, DIM, VectorEncoding::Binary).await;
    match result {
        Ok(_) => panic!("reopening with different encoding should return Err"),
        Err(err) => {
            let err_msg = format!("{err}");
            assert!(
                err_msg.to_lowercase().contains("config")
                    || err_msg.to_lowercase().contains("mismatch")
                    || err_msg.to_lowercase().contains("encoding")
                    || err_msg.to_lowercase().contains("schema"),
                "error message should mention config/mismatch/encoding/schema, got: {err_msg}"
            );
        }
    }
}

#[tokio::test]
async fn reopen_with_same_encoding_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.db");

    // Open with Float32
    let store = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();

    // Insert a chunk to verify data survives reopen
    let chunk = make_chunk(
        "c1",
        "doc-1",
        "store-1",
        "src-1",
        "hello world",
        vec![1.0, 0.0, 0.0, 0.0],
    );
    store.upsert_chunks(vec![chunk]).await.unwrap();
    drop(store);

    // Reopen with same encoding — should succeed
    let store2 = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();

    // Verify data is still there
    let stats = store2.stats().await.unwrap();
    assert_eq!(
        stats.chunk_count, 1,
        "chunk should persist across reopen"
    );
}

// ---------------------------------------------------------------------------
// Test 4: schema_version doesn't accumulate rows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schema_version_stays_single_row_across_reopens() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("store.db");

    // Open, close, reopen, close, reopen
    let store = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();
    drop(store);

    let store = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();
    drop(store);

    let _store = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();

    // Open a separate connection to query schema_version directly.
    let db = libsql::Builder::new_local(&path)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();

    let mut rows = conn
        .query("SELECT COUNT(*) FROM schema_version", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("should have a row");
    let count: i64 = row.get(0).unwrap();

    assert_eq!(
        count, 1,
        "schema_version should contain exactly 1 row after multiple reopens, got {count}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Upsert with changed text updates FTS index
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_updated_text_updates_fts_index() {
    let (_tmp, store) = fresh_store().await;

    // Insert a chunk with text "alpha bravo charlie"
    let chunk_v1 = make_chunk(
        "c1",
        "doc-1",
        "store-1",
        "src-1",
        "alpha bravo charlie",
        vec![1.0, 0.0, 0.0, 0.0],
    );
    store.upsert_chunks(vec![chunk_v1]).await.unwrap();

    // BM25 search for "alpha" should find it
    let results = store.bm25_search("alpha", 10, &[]).await.unwrap();
    assert_eq!(
        results.len(),
        1,
        "should find 'alpha' in original text"
    );
    assert_eq!(results[0].chunk.id, "c1");

    // Upsert same chunk ID with new text "delta echo foxtrot"
    let chunk_v2 = make_chunk(
        "c1",
        "doc-1",
        "store-1",
        "src-1",
        "delta echo foxtrot",
        vec![1.0, 0.0, 0.0, 0.0],
    );
    store.upsert_chunks(vec![chunk_v2]).await.unwrap();

    // BM25 search for "alpha" should return empty (old text removed from FTS)
    let results = store.bm25_search("alpha", 10, &[]).await.unwrap();
    assert!(
        results.is_empty(),
        "old text 'alpha' should be gone from FTS index after upsert, got {} results",
        results.len()
    );

    // BM25 search for "delta" should find the updated chunk
    let results = store.bm25_search("delta", 10, &[]).await.unwrap();
    assert_eq!(
        results.len(),
        1,
        "should find 'delta' in updated text"
    );
    assert_eq!(results[0].chunk.id, "c1");
}
