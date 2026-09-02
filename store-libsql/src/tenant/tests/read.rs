//! `tenant::read` tests.
//!
//! Split from `write.rs` alongside the production split: the `get_chunk` and
//! `get_resource_record` cases below are read-path behavior, and two of them
//! are the two halves of one decision — whether an undecodable
//! `metadata_json` column is a fallback or an error — which is only legible
//! read side by side.

use localdb_core::metadata::Metadata;
use localdb_core::types::Span;
use localdb_core::{Error, ResourceRecord, StoreBackend};
use tempfile::tempdir;

use super::common::{
    add_store_and_source, backend_with_store_and_source, chunk_record, resource_dates_and_external,
};
use crate::SqliteBackend;

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
        modified_at: Some("2026-07-01T00:00:00Z".to_string()),
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

// ---------------------------------------------------------------------------
// get_resource_record (the read counterpart to update_resource_metadata)
// ---------------------------------------------------------------------------

/// The whole reason `get_resource_record` exists, pinned end to end.
///
/// A caller that rewrites one field of a resource row must read every other
/// field back first, because `update_resource_metadata` rewrites all of them.
/// The obvious read — `get_chunks_for_resource` — cannot serve: `CHUNK_COLS`
/// omits `external_id`, `date_original` and `date_parsed` (write-only on
/// `ChunkRecord` by design), so a record rebuilt from a chunk carries `None`
/// for each and writes `NULL` over three real columns.
///
/// This drives the exact sequence `PipelineCallback::on_validators_refreshed`
/// performs on a 304 — read, rotate the ETag, write back — and asserts the
/// three columns survive. It has to live here rather than in `core`: the
/// `FakeStore` double denormalizes every `ResourceRecord` field onto its
/// chunks, so a chunk read there faithfully returns all three and the bug is
/// invisible.
#[tokio::test]
async fn validator_rotation_through_get_resource_record_preserves_the_columns_chunks_drop() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut seed = chunk_record("2026-07-01T00:00:00Z", Some("2026-07-01T00:00:00Z"));
    seed.external_id = Some("urn:entry:42".to_string());
    seed.external_etag = Some("\"v1\"".to_string());
    seed.date_original = Some("2026-06-15T10:30:00Z".to_string());
    seed.date_parsed = Some("2026-06-15".to_string());
    handle.upsert_chunks(vec![seed]).await.unwrap();

    // The projection gap this seam routes around: the chunk read reports
    // `None` for all three despite the row holding real values.
    let chunks = handle.get_chunks_for_resource("doc-1").await.unwrap();
    let chunk = chunks.first().expect("the seeded chunk must be readable");
    assert_eq!(chunk.external_id, None);
    assert_eq!(chunk.date_original, None);
    assert_eq!(chunk.date_parsed, None);

    let persisted = handle
        .get_resource_record("store-1", "doc-1")
        .await
        .unwrap()
        .expect("the seeded resource row must be readable");
    assert_eq!(persisted.external_id.as_deref(), Some("urn:entry:42"));
    assert_eq!(persisted.external_etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        persisted.date_original.as_deref(),
        Some("2026-06-15T10:30:00Z")
    );
    assert_eq!(persisted.date_parsed.as_deref(), Some("2026-06-15"));

    // The hook's write: rotate the ETag, carry everything else through.
    let rotated = ResourceRecord {
        external_etag: Some("\"v2\"".to_string()),
        ..persisted
    };
    handle
        .update_resource_metadata("store-1", "doc-1", &rotated)
        .await
        .unwrap();

    let (date_original, date_parsed, external_id, external_etag) =
        resource_dates_and_external(&backend, "doc-1").await;
    assert_eq!(
        external_etag.as_deref(),
        Some("\"v2\""),
        "the rotated validator must land"
    );
    assert_eq!(
        external_id.as_deref(),
        Some("urn:entry:42"),
        "a validator refresh must not null external_id"
    );
    assert_eq!(
        date_original.as_deref(),
        Some("2026-06-15T10:30:00Z"),
        "a validator refresh must not null date_original"
    );
    assert_eq!(
        date_parsed.as_deref(),
        Some("2026-06-15"),
        "a validator refresh must not null date_parsed"
    );
}

/// `Ok(None)`, not an error, for a resource this store has no row for — a
/// concurrent delete racing a refresh is ordinary, and the callers treat it
/// the same way they treat a `DocumentIndex` miss.
#[tokio::test]
async fn get_resource_record_returns_none_for_an_unknown_resource() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    assert!(handle
        .get_resource_record("store-1", "does-not-exist")
        .await
        .unwrap()
        .is_none());
}

/// The other half of the decision `get_chunk_tolerates_invalid_metadata_json`
/// pins, and the reason the two must differ.
///
/// `get_resource_record`'s output is not displayed — it is the payload of an
/// `update_resource_metadata` call, which rewrites *every* metadata column of
/// the row. A `Metadata::default()` fallback here would therefore not
/// misreport the corrupt row, it would overwrite it: a 304 that changed
/// nothing would replace whatever real metadata survived with an empty one.
/// So this read errors, and the row is left exactly as it stands.
///
/// It has to be a real store: `FakeStore` holds `Metadata` values in memory
/// and never round-trips them through JSON, so there is no corrupt column
/// for it to have an opinion about.
#[tokio::test]
async fn get_resource_record_errors_on_undecodable_metadata_json() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut seed = chunk_record("2026-07-01T00:00:00Z", Some("2026-07-01T00:00:00Z"));
    seed.external_id = Some("urn:entry:42".to_string());
    seed.external_etag = Some("\"v1\"".to_string());
    seed.date_original = Some("2026-06-15T10:30:00Z".to_string());
    seed.date_parsed = Some("2026-06-15".to_string());
    handle.upsert_chunks(vec![seed]).await.unwrap();

    // Real metadata that no longer decodes — corruption, or a shape from
    // some future/foreign writer. Either way it is not `Metadata::default()`.
    let corrupt = r#"{"dublin_core":{"title":"Real Title"},"#;
    let conn = backend.conn.writer().await;
    conn.execute(
        "UPDATE resources SET metadata_json = ? WHERE store_id = 'store-1' AND id = ?",
        libsql::params![corrupt.to_string(), "doc-1".to_string()],
    )
    .await
    .unwrap();
    drop(conn);

    let err = handle
        .get_resource_record("store-1", "doc-1")
        .await
        .expect_err("an undecodable metadata_json must not read back as a default");
    match err {
        Error::Internal { correlation_id, .. } => {
            assert_eq!(correlation_id, "store_handle_resource_metadata");
        }
        other => panic!("expected Error::Internal, got {other:?}"),
    }

    assert_eq!(
        metadata_json(&backend, "doc-1").await,
        corrupt,
        "the failed read must leave the column byte-identical"
    );
}

/// The refresh sequence `PipelineCallback::on_validators_refreshed` performs,
/// run over a row whose `metadata_json` no longer decodes: the read fails, so
/// no write follows, so the column is byte-identical afterwards. Before the
/// read was made strict this sequence completed "successfully" and left
/// `{"Document":{...}}` — an empty default — where the real metadata had been.
#[tokio::test]
async fn a_validator_rotation_over_a_corrupt_row_writes_nothing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut seed = chunk_record("2026-07-01T00:00:00Z", Some("2026-07-01T00:00:00Z"));
    seed.external_etag = Some("\"v1\"".to_string());
    handle.upsert_chunks(vec![seed]).await.unwrap();

    let corrupt = r#"{not valid json"#;
    let conn = backend.conn.writer().await;
    conn.execute(
        "UPDATE resources SET metadata_json = ? WHERE store_id = 'store-1' AND id = ?",
        libsql::params![corrupt.to_string(), "doc-1".to_string()],
    )
    .await
    .unwrap();
    drop(conn);

    // Step one of the hook: read the row it is about to rewrite in full.
    // It stops here, which is the whole point.
    assert!(handle
        .get_resource_record("store-1", "doc-1")
        .await
        .is_err());

    assert_eq!(
        metadata_json(&backend, "doc-1").await,
        corrupt,
        "a refresh that could not read must not have written"
    );
    let (_, _, _, external_etag) = resource_dates_and_external(&backend, "doc-1").await;
    assert_eq!(
        external_etag.as_deref(),
        Some("\"v1\""),
        "and the validator it meant to rotate stays as it was"
    );
}

/// A `TenantStore` is a handle on exactly one store, so the `store_id` its
/// trait methods carry is an assertion to check — never a value to forward
/// into a `WHERE store_id = ?`. `get_resource_record` is the only read taking
/// one at all (its signature mirrors the write it feeds), so it is the only
/// read that can get this wrong. Mirrors
/// `delete_by_store_rejects_foreign_store_id` on the write side.
#[tokio::test]
async fn get_resource_record_rejects_foreign_store_id() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_store_and_source(&backend, "store-2", "src-2", "/other").await;

    // store-2 holds the row the store-1 handle must not be able to reach.
    let other = backend.retrieval_store("store-2").await.unwrap();
    let mut seed = chunk_record("2026-07-01T00:00:00Z", Some("2026-07-01T00:00:00Z"));
    seed.id = "chunk-2".to_string();
    seed.resource_id = "doc-2".to_string();
    seed.store_id = "store-2".to_string();
    seed.origin_store = "store-2".to_string();
    seed.source_id = "src-2".to_string();
    seed.uri = "file:///other/doc.md".to_string();
    seed.external_id = Some("urn:entry:other".to_string());
    other.upsert_chunks(vec![seed]).await.unwrap();

    let handle = backend.retrieval_store("store-1").await.unwrap();
    let err = handle
        .get_resource_record("store-2", "doc-2")
        .await
        .expect_err("a store-1 handle must not read store-2's row");
    match err {
        Error::Internal { correlation_id, .. } => {
            assert_eq!(correlation_id, "store_handle_tenant_violation");
        }
        other => panic!("expected Error::Internal, got {other:?}"),
    }

    // Not merely refused: nothing about store-2's row leaked out. The same
    // handle sees `None` for that resource id under its own store.
    assert!(handle
        .get_resource_record("store-1", "doc-2")
        .await
        .unwrap()
        .is_none());
}

/// Read `metadata_json` straight off the `resources` row, byte for byte —
/// the point of the two tests above is that a failed read leaves it
/// untouched, which no typed accessor can show.
async fn metadata_json(backend: &SqliteBackend, resource_id: &str) -> String {
    let conn = backend.conn.reader();
    let mut rows = conn
        .query(
            "SELECT metadata_json FROM resources WHERE store_id = 'store-1' AND id = ?",
            libsql::params![resource_id.to_string()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("resource row must exist");
    row.get(0).unwrap()
}
