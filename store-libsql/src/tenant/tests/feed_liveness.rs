//! `tenant::read::list_stale_feed_resources` /
//! `tenant::write::touch_resource_liveness` tests — the store seam behind
//! the feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out feed
//! entries: the liveness sweep").

use localdb_core::types::SourceKind;
use localdb_core::{ChunkRecord, Error, Metadata, SourceRow, StoreBackend};
use tempfile::tempdir;

use super::common::backend_with_store_and_source;
use crate::SqliteBackend;

/// `backend_with_store_and_source` seeds only a path source (`src-1`); feed
/// liveness candidates need `ingestor_kind = 'feed'` and a feed source to
/// scope by, so this adds a second source (`src-feed`) to the same store.
async fn add_feed_source(backend: &SqliteBackend) {
    backend
        .upsert_source(&SourceRow {
            id: "src-feed".to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Feed,
            root: None,
            url: Some("https://feed.example.com/feed.xml".to_string()),
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: "2026-07-01T00:00:00Z".to_string(),
            config_json: None,
            feed_etag: None,
            feed_last_modified: None,
            feed_inputs_digest: None,
        })
        .await
        .unwrap();
}

/// A minimal feed-entry `ChunkRecord` fixture, owned by `src-feed`.
fn feed_chunk_record(resource_id: &str, uri: &str) -> ChunkRecord {
    ChunkRecord {
        id: format!("{resource_id}-chunk"),
        resource_id: resource_id.to_string(),
        store_id: "store-1".to_string(),
        text: "entry body".to_string(),
        span: localdb_core::types::Span::new(0, 10),
        heading_path: vec![],
        embedding: vec![0.1, 0.2, 0.3, 0.4],
        policy_version: "v1".to_string(),
        fetched_at: "2026-07-01T00:00:00Z".to_string(),
        modified_at: None,
        content_hash: format!("hash-{resource_id}"),
        origin_store: "store-1".to_string(),
        source_id: "src-feed".to_string(),
        ingestor_kind: "feed".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.to_string(),
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

/// Set `resources.last_checked_at` directly — the column has no
/// `ChunkRecord`/`RetrievalStore` write path other than
/// `touch_resource_liveness` itself, so seeding a specific prior value for a
/// test goes around the trait, same posture as `tests::write`'s
/// `resource_row` helper.
async fn set_last_checked_at(backend: &SqliteBackend, resource_id: &str, value: Option<&str>) {
    let conn = backend.conn.writer().await;
    conn.execute(
        "UPDATE resources SET last_checked_at = ? WHERE store_id = 'store-1' AND id = ?",
        libsql::params![value, resource_id.to_string()],
    )
    .await
    .unwrap();
}

/// Read `external_etag, external_last_modified, last_checked_at,
/// index_updated_at` straight off the `resources` row.
async fn resource_liveness_columns(
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
            "SELECT external_etag, external_last_modified, last_checked_at, index_updated_at \
             FROM resources WHERE store_id = 'store-1' AND id = ?",
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

/// NULL-first, then oldest-first, and `checked_before` excludes a
/// still-fresh row — the three properties `run_feed_liveness_sweep` in
/// `core::ingestion` relies on `list_stale_feed_resources` to provide.
/// Also proves the query scopes strictly by `ingestor_kind = 'feed'`: a
/// `path` resource owned by the same store never comes back regardless of
/// its own `last_checked_at`.
#[tokio::test]
async fn list_stale_feed_resources_orders_null_first_then_oldest_and_excludes_fresh() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    handle
        .upsert_chunks(vec![feed_chunk_record(
            "feed-null",
            "https://a.example.com/",
        )])
        .await
        .unwrap();
    handle
        .upsert_chunks(vec![feed_chunk_record(
            "feed-old",
            "https://b.example.com/",
        )])
        .await
        .unwrap();
    set_last_checked_at(&backend, "feed-old", Some("2020-01-01T00:00:00Z")).await;
    handle
        .upsert_chunks(vec![feed_chunk_record(
            "feed-fresh",
            "https://c.example.com/",
        )])
        .await
        .unwrap();
    set_last_checked_at(&backend, "feed-fresh", Some("2099-01-01T00:00:00Z")).await;

    // A path resource, same store — must never be returned no matter its
    // own last_checked_at (it has none, so this also re-covers the
    // NULL-leading case for a non-candidate row that must still be
    // excluded).
    let mut path_record = feed_chunk_record("doc-path", "file:///docs/doc.md");
    path_record.source_id = "src-1".to_string();
    path_record.ingestor_kind = "path".to_string();
    handle.upsert_chunks(vec![path_record]).await.unwrap();

    let candidates = handle
        .list_stale_feed_resources("store-1", "src-feed", "2026-01-01T00:00:00Z", 10)
        .await
        .unwrap();

    let ids: Vec<&str> = candidates.iter().map(|c| c.resource_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["feed-null", "feed-old"],
        "never-checked (NULL) must lead, checked-but-stale follows, and a still-fresh \
         (2099) or non-feed row must not appear at all"
    );
}

/// The batch cap: `LIMIT` truncates to the oldest N, not an arbitrary N.
#[tokio::test]
async fn list_stale_feed_resources_respects_limit_oldest_first() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    for (id, checked_at) in [
        ("feed-1", "2020-01-01T00:00:00Z"),
        ("feed-2", "2021-01-01T00:00:00Z"),
        ("feed-3", "2022-01-01T00:00:00Z"),
    ] {
        handle
            .upsert_chunks(vec![feed_chunk_record(
                id,
                &format!("https://{id}.example.com/"),
            )])
            .await
            .unwrap();
        set_last_checked_at(&backend, id, Some(checked_at)).await;
    }

    let candidates = handle
        .list_stale_feed_resources("store-1", "src-feed", "2026-01-01T00:00:00Z", 2)
        .await
        .unwrap();
    let ids: Vec<&str> = candidates.iter().map(|c| c.resource_id.as_str()).collect();
    assert_eq!(ids, vec!["feed-1", "feed-2"]);
}

/// Scoped by `source_id`, not just `store_id`: a second feed source's own
/// stale resources must never leak into another feed source's candidate
/// list.
#[tokio::test]
async fn list_stale_feed_resources_scopes_by_source_id() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    backend
        .upsert_source(&SourceRow {
            id: "src-feed-2".to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Feed,
            root: None,
            url: Some("https://other-feed.example.com/feed.xml".to_string()),
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: "2026-07-01T00:00:00Z".to_string(),
            config_json: None,
            feed_etag: None,
            feed_last_modified: None,
            feed_inputs_digest: None,
        })
        .await
        .unwrap();
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut other_source_record = feed_chunk_record("feed-other", "https://other.example.com/");
    other_source_record.source_id = "src-feed-2".to_string();
    handle
        .upsert_chunks(vec![other_source_record])
        .await
        .unwrap();

    let candidates = handle
        .list_stale_feed_resources("store-1", "src-feed", "2026-01-01T00:00:00Z", 10)
        .await
        .unwrap();
    assert!(
        candidates.is_empty(),
        "a resource owned by a different source must never appear: {candidates:?}"
    );
}

/// The round-trip proper: validators and `resource_id`/`uri` come back
/// exactly as stored.
#[tokio::test]
async fn list_stale_feed_resources_round_trips_validators() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut record = feed_chunk_record("feed-1", "https://a.example.com/");
    record.external_etag = Some("\"v1\"".to_string());
    handle.upsert_chunks(vec![record]).await.unwrap();
    // `external_last_modified` is not a `ChunkRecord` column (see
    // `RetrievalStore::upsert_chunks_and_blocks`'s doc comment); set it via
    // `touch_resource_liveness` itself, exercised together with the read
    // side below rather than going around the trait for a write only
    // `touch_resource_liveness` can perform.
    handle
        .touch_resource_liveness(
            "store-1",
            "feed-1",
            Some("\"v2\""),
            Some("Wed, 21 Oct 2015 07:28:00 GMT"),
        )
        .await
        .unwrap();

    let candidates = handle
        .list_stale_feed_resources("store-1", "src-feed", "2099-01-01T00:00:00Z", 10)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    let c = &candidates[0];
    assert_eq!(c.resource_id, "feed-1");
    assert_eq!(c.uri, "https://a.example.com/");
    assert_eq!(c.external_etag.as_deref(), Some("\"v2\""));
    assert_eq!(
        c.external_last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
}

/// The critical invariant: a liveness probe writes `last_checked_at` and
/// the validators, and leaves `index_updated_at` exactly as it was —
/// `index_updated_at` means "we last wrote this resource's stored state"
/// (`DocumentInfo::index_updated_at`), and a probe writes no content and no
/// metadata.
#[tokio::test]
async fn touch_resource_liveness_sets_last_checked_at_and_leaves_index_updated_at_untouched() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    handle
        .upsert_chunks(vec![feed_chunk_record("feed-1", "https://a.example.com/")])
        .await
        .unwrap();
    let (_, _, last_checked_before, index_updated_before) =
        resource_liveness_columns(&backend, "feed-1").await;
    assert_eq!(
        last_checked_before, None,
        "a freshly-inserted resource has never been liveness-checked"
    );
    let index_updated_before = index_updated_before.expect("set on first insert");

    handle
        .touch_resource_liveness(
            "store-1",
            "feed-1",
            Some("\"etag-9\""),
            Some("Wed, 21 Oct 2015 07:28:00 GMT"),
        )
        .await
        .unwrap();

    let (etag_after, last_modified_after, last_checked_after, index_updated_after) =
        resource_liveness_columns(&backend, "feed-1").await;
    assert_eq!(etag_after.as_deref(), Some("\"etag-9\""));
    assert_eq!(
        last_modified_after.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert!(
        last_checked_after.is_some(),
        "touch_resource_liveness must set last_checked_at"
    );
    assert_eq!(
        index_updated_after.as_deref(),
        Some(index_updated_before.as_str()),
        "a liveness probe must never bump index_updated_at — it writes no content, no metadata"
    );
}

/// Zero-rows semantics mirror `update_resource_metadata`'s own guard: a
/// concurrent delete racing the probe must be a reported error, never a
/// silent no-op.
#[tokio::test]
async fn touch_resource_liveness_errors_when_no_row_matches() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let err = handle
        .touch_resource_liveness("store-1", "does-not-exist", Some("\"etag\""), None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::ResourceNotFound {
            id: "does-not-exist".to_string()
        }
    );
}
