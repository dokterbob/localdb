//! `tenant::write` tests.

use localdb_core::block::{Block, BlockKind};
use localdb_core::metadata::Metadata;
use localdb_core::types::Span;
use localdb_core::{ChunkRecord, StoreBackend};
use tempfile::tempdir;

use super::common::backend_with_store_and_source;
use crate::SqliteBackend;

/// A minimal, self-contained `ChunkRecord` fixture for `store-1`/`doc-1`
/// (the store/source seeded by `backend_with_store_and_source`), with
/// `fetched_at` and `modified_at` as explicit parameters so callers can
/// distinguish them.
fn chunk_record(fetched_at: &str, modified_at: &str) -> ChunkRecord {
    ChunkRecord {
        id: "chunk-1".to_string(),
        resource_id: "doc-1".to_string(),
        store_id: "store-1".to_string(),
        text: "some chunk text".to_string(),
        span: Span::new(0, 15),
        heading_path: vec![],
        embedding: vec![0.1, 0.2, 0.3, 0.4],
        policy_version: "v1".to_string(),
        fetched_at: fetched_at.to_string(),
        modified_at: modified_at.to_string(),
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
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    }
}

/// Read `added_at`, `modified_at`, and `index_updated_at` straight off the
/// `resources` row for `resource_id` — the columns `ChunkRecord`/`get_chunk`
/// don't (fully) expose, so these tests go around the `RetrievalStore` trait
/// for assertions.
async fn resource_row(
    backend: &SqliteBackend,
    resource_id: &str,
) -> (String, String, Option<String>) {
    let conn = backend.conn.reader();
    let mut rows = conn
        .query(
            "SELECT added_at, modified_at, index_updated_at FROM resources \
             WHERE store_id = 'store-1' AND id = ?",
            libsql::params![resource_id.to_string()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("resource row must exist");
    (
        row.get(0).unwrap(),
        row.get(1).unwrap(),
        row.get(2).unwrap(),
    )
}

/// Read `date_original`, `date_parsed`, `external_id`, and `external_etag`
/// straight off the `resources` row for `resource_id` — write-only
/// `ChunkRecord` stamps (`core::store::ChunkRecord`'s doc comment) that
/// `get_chunk`/`CHUNK_COLS` never read back, so these tests go around the
/// `RetrievalStore` trait for assertions, same posture as `resource_row`.
async fn resource_dates_and_external(
    backend: &SqliteBackend,
    resource_id: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let conn = backend.conn.reader();
    let mut rows = conn
        .query(
            "SELECT date_original, date_parsed, external_id, external_etag FROM resources \
             WHERE store_id = 'store-1' AND id = ?",
            libsql::params![resource_id.to_string()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("resource row must exist");
    (
        row.get(0).unwrap(),
        row.get(1).unwrap(),
        row.get(2).unwrap(),
        row.get(3).unwrap(),
    )
}

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
        modified_at: "2026-07-01T00:00:00Z".to_string(),
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
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
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
        modified_at: "2026-07-01T00:00:00Z".to_string(),
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
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
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

/// `resources.modified_at` must come from the resource's own claimed
/// modification time (`ChunkRecord::modified_at`), not `fetched_at`
/// (acquisition time) — the two used to be conflated (both bound from
/// `record.fetched_at`). See specs/02-domain-model.md §2.
#[tokio::test]
async fn upsert_resource_modified_at_reflects_resource_modified_at_not_fetched_at() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let record = chunk_record("2026-07-01T00:00:00Z", "2020-01-01T00:00:00Z");
    handle.upsert_chunks(vec![record]).await.unwrap();

    let (added_at, modified_at, _) = resource_row(&backend, "doc-1").await;
    assert_eq!(
        added_at, "2026-07-01T00:00:00Z",
        "added_at must still come from fetched_at"
    );
    assert_eq!(
        modified_at, "2020-01-01T00:00:00Z",
        "modified_at must come from the resource's own claimed modification time, \
         not fetched_at"
    );
}

/// `resources.index_updated_at` is a write-time clock: it must be set on
/// first insert and must not go backwards across a later upsert of the same
/// resource, while `added_at` is preserved by the ordinary `ON CONFLICT`
/// path (no replace involved here).
#[tokio::test]
async fn upsert_resource_index_updated_at_bumps_on_conflict_update() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    handle
        .upsert_chunks(vec![chunk_record(
            "2026-07-01T00:00:00Z",
            "2026-07-01T00:00:00Z",
        )])
        .await
        .unwrap();
    let (added_at_1, _, index_updated_at_1) = resource_row(&backend, "doc-1").await;
    let index_updated_at_1 =
        index_updated_at_1.expect("index_updated_at must be set on first insert");

    handle
        .upsert_chunks(vec![chunk_record(
            "2026-07-01T00:00:00Z",
            "2026-07-02T00:00:00Z",
        )])
        .await
        .unwrap();
    let (added_at_2, modified_at_2, index_updated_at_2) = resource_row(&backend, "doc-1").await;
    let index_updated_at_2 =
        index_updated_at_2.expect("index_updated_at must be set on the second upsert too");

    assert_eq!(
        added_at_1, added_at_2,
        "added_at must be preserved by ON CONFLICT"
    );
    assert_eq!(modified_at_2, "2026-07-02T00:00:00Z");
    assert!(
        index_updated_at_2.as_str() >= index_updated_at_1.as_str(),
        "index_updated_at must not go backwards: {index_updated_at_1} -> {index_updated_at_2}"
    );
}

/// A policy-only re-index deletes and reinserts the SAME resource_id inside
/// one transaction (`upsert_chunks_and_blocks` with
/// `replaces_resource_id == Some(resource_id)`). `added_at` must survive
/// that round trip even though the new scan carries a fresh `fetched_at` —
/// see specs/02-domain-model.md §2's added_at row.
#[tokio::test]
async fn upsert_resource_added_at_survives_policy_reindex() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let first = chunk_record("2020-01-01T00:00:00Z", "2020-01-01T00:00:00Z");
    handle
        .upsert_chunks_and_blocks("store-1", "doc-1", vec![first], &[], None)
        .await
        .unwrap();
    let (added_at_1, _, _) = resource_row(&backend, "doc-1").await;

    // A fresh scan of the same resource: a different fetched_at (this run's
    // acquisition time) and modified_at, same resource_id.
    let second = chunk_record("2026-08-26T00:00:00Z", "2026-08-20T00:00:00Z");
    handle
        .upsert_chunks_and_blocks("store-1", "doc-1", vec![second], &[], Some("doc-1"))
        .await
        .unwrap();
    let (added_at_2, modified_at_2, index_updated_at_2) = resource_row(&backend, "doc-1").await;

    assert_eq!(
        added_at_2, added_at_1,
        "added_at must survive a same-resource-id policy reindex"
    );
    assert_ne!(
        added_at_2, "2026-08-26T00:00:00Z",
        "added_at must not reset to the new scan's fetched_at"
    );
    assert_eq!(modified_at_2, "2026-08-20T00:00:00Z");
    assert!(
        index_updated_at_2.is_some(),
        "index_updated_at must be populated after the reindex"
    );
}

/// `ChunkRecord::date_original`/`date_parsed` are write-only stamps
/// (`core::store::ChunkRecord`'s doc comment): `upsert_chunks_inner` must
/// persist them to `resources.date_original`/`date_parsed` and refresh them
/// on a later upsert of the same resource, the same posture as
/// `metadata_json`. Read back via direct SQL since `get_chunk`/`CHUNK_COLS`
/// never expose these columns.
#[tokio::test]
async fn upsert_resource_persists_date_original_and_date_parsed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut record = chunk_record("2026-07-01T00:00:00Z", "2026-07-01T00:00:00Z");
    record.date_original = Some("2026-06-15T10:30:00Z".to_string());
    record.date_parsed = Some("2026-06-15".to_string());
    handle.upsert_chunks(vec![record]).await.unwrap();

    let (date_original, date_parsed, _, _) = resource_dates_and_external(&backend, "doc-1").await;
    assert_eq!(date_original.as_deref(), Some("2026-06-15T10:30:00Z"));
    assert_eq!(date_parsed.as_deref(), Some("2026-06-15"));

    // A later upsert of the same resource refreshes both columns, same as
    // `metadata_json`.
    let mut second = chunk_record("2026-07-01T00:00:00Z", "2026-07-01T00:00:00Z");
    second.date_original = Some("2026-07-20".to_string());
    second.date_parsed = Some("2026-07-20".to_string());
    handle.upsert_chunks(vec![second]).await.unwrap();

    let (date_original_2, date_parsed_2, _, _) =
        resource_dates_and_external(&backend, "doc-1").await;
    assert_eq!(date_original_2.as_deref(), Some("2026-07-20"));
    assert_eq!(date_parsed_2.as_deref(), Some("2026-07-20"));
}

/// Same contract as `upsert_resource_persists_date_original_and_date_parsed`,
/// for `external_id`/`external_etag`.
#[tokio::test]
async fn upsert_resource_persists_external_id_and_external_etag() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut record = chunk_record("2026-07-01T00:00:00Z", "2026-07-01T00:00:00Z");
    record.external_id = Some("urn:entry:1".to_string());
    record.external_etag = Some("\"etag-1\"".to_string());
    handle.upsert_chunks(vec![record]).await.unwrap();

    let (_, _, external_id, external_etag) = resource_dates_and_external(&backend, "doc-1").await;
    assert_eq!(external_id.as_deref(), Some("urn:entry:1"));
    assert_eq!(external_etag.as_deref(), Some("\"etag-1\""));

    let mut second = chunk_record("2026-07-01T00:00:00Z", "2026-07-01T00:00:00Z");
    second.external_id = Some("urn:entry:1".to_string());
    second.external_etag = Some("\"etag-2\"".to_string());
    handle.upsert_chunks(vec![second]).await.unwrap();

    let (_, _, external_id_2, external_etag_2) =
        resource_dates_and_external(&backend, "doc-1").await;
    assert_eq!(external_id_2.as_deref(), Some("urn:entry:1"));
    assert_eq!(
        external_etag_2.as_deref(),
        Some("\"etag-2\""),
        "external_etag must refresh on re-index, same as metadata_json"
    );
}
