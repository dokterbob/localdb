//! `tenant::write` tests.

use localdb_core::metadata::Metadata;
use localdb_core::types::Span;
use localdb_core::StoreBackend;
use tempfile::tempdir;

use super::common::backend_with_store_and_source;

/// Regression test for issue C4 on the tenant read path
/// (`tenant::rows::row_to_chunk_record_strict`, via
/// `connection::parse_metadata_json_lenient`): a resource row with
/// syntactically invalid `metadata_json` must still be readable through
/// `get_chunk` — falling back to `Metadata::default()` — rather than
/// erroring the whole read. This exercises the same shared helper that
/// `registry::documents::find_document` covers on the registry side
/// (`registry::tests::find_document_tolerates_invalid_metadata_json`).
#[tokio::test]
async fn get_chunk_tolerates_invalid_metadata_json() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;

    let handle = backend.retrieval_store("store-1").await.unwrap();
    let record = localdb_core::ChunkRecord {
        id: "chunk-1".to_string(),
        resource_id: "doc-1".to_string(),
        store_id: "store-1".to_string(),
        text: "some chunk text".to_string(),
        span: Span::new(0, 15),
        heading_path: vec![],
        embedding: vec![0.1, 0.2, 0.3, 0.4],
        policy_version: "v1".to_string(),
        fetched_at: "2026-07-01T00:00:00Z".to_string(),
        content_hash: "abc123".to_string(),
        origin_store: "store-1".to_string(),
        source_id: "src-1".to_string(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: "file:///docs/doc.md".to_string(),
        metadata: Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
    };
    handle.upsert_chunks(vec![record]).await.unwrap();

    // Corrupt the persisted metadata_json directly with syntactically
    // invalid JSON.
    let conn = backend.conn.conn().await;
    conn.execute(
        "UPDATE resources SET metadata_json = ? WHERE id = ?",
        libsql::params!["{not valid json".to_string(), "doc-1".to_string()],
    )
    .await
    .unwrap();
    drop(conn);

    let chunk = handle
        .get_chunk("chunk-1")
        .await
        .unwrap()
        .expect("chunk must still be found despite invalid metadata_json");
    assert_eq!(
        chunk.metadata,
        Metadata::default(),
        "invalid metadata_json must fall back to default metadata, not error the read"
    );
}
