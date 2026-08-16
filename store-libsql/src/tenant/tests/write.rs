//! `tenant::write` tests.

use localdb_core::block::{Block, BlockKind};
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
    let conn = backend.conn.writer().await;
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

/// Regression test for issue #217 step 5: `write::upsert_blocks` used to
/// INSERT each block as its own autocommit statement, with no surrounding
/// transaction — a mid-batch SQL failure left however many blocks had
/// already been inserted permanently persisted. `upsert_blocks` now runs
/// through `write_tx()`, so a failure anywhere in the batch must roll back
/// everything inserted so far, leaving zero rows.
///
/// Why this needs a test-injected trigger rather than a "natural" schema
/// constraint: `upsert_blocks`'s only realistic SQL failure surface is the
/// FK on `blocks(store_id, resource_id)` referencing `resources` — and
/// that's *uniform* across the whole batch, since `store_id`/`resource_id`
/// are single, batch-wide arguments, not per-block. It can only fail every
/// block identically (resource missing => every insert fails, including
/// the first => old and new code both already show zero rows, proving
/// nothing), never "block 2 fails but block 1 already succeeded". The
/// UNIQUE(store_id, resource_id, seq) constraint is resolved via `ON
/// CONFLICT ... DO UPDATE`, so it never errors; there's no CHECK constraint
/// or trigger on `blocks` in the real schema; and every NOT NULL column is
/// always populated by well-typed `Block` values (`metadata_json`'s
/// serialization essentially can't fail for real `BlockKind` data — no
/// NaN/Infinity floats, no non-string map keys reachable through the public
/// type). So there is no realistic, non-contorted way to make a *later*
/// block fail after an *earlier* one in the same call already succeeded.
/// To still exercise a genuine mid-batch SQL failure deterministically
/// (not via timing/concurrency), this test installs a `TEMP TRIGGER` via
/// raw SQL on the test's own connection — not a production code change —
/// that aborts specifically the second block's INSERT. This is the
/// standard SQL-level fault-injection technique for exactly this
/// situation.
#[tokio::test]
async fn upsert_blocks_is_now_transactional() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    // Seed a resource so the blocks(store_id, resource_id) FK is satisfied
    // for every block in the batch below.
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

    // Fault injection: abort the INSERT of the SECOND block (seq = 1)
    // specifically, after the first (seq = 0) has already gone through —
    // see the doc comment above for why this is necessary.
    {
        let conn = backend.conn.writer().await;
        conn.execute(
            "CREATE TEMP TRIGGER reject_second_block
             AFTER INSERT ON blocks
             WHEN NEW.seq = 1
             BEGIN
                 SELECT RAISE(ABORT, 'test-injected failure for seq=1');
             END",
            (),
        )
        .await
        .unwrap();
    }

    let blocks = vec![
        Block {
            seq: 0,
            kind: BlockKind::Text,
            text: "block zero".to_string(),
            location: None,
        },
        Block {
            seq: 1,
            kind: BlockKind::Text,
            text: "block one".to_string(),
            location: None,
        },
    ];

    let result = handle.upsert_blocks("store-1", "doc-1", &blocks).await;
    assert!(
        result.is_err(),
        "the second block's insert should fail (test-injected trigger)"
    );

    let persisted = handle.get_blocks_for_resource("doc-1").await.unwrap();
    assert!(
        persisted.is_empty(),
        "upsert_blocks must be all-or-nothing: a mid-batch failure must leave ZERO block rows \
         persisted, got {persisted:?}"
    );
}
