//! `tenant::read::list_stale_feed_resources` /
//! `tenant::write::touch_resource_liveness` tests — the store seam behind
//! the feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out feed
//! entries: the liveness sweep").

use localdb_core::types::SourceKind;
use localdb_core::{ChunkRecord, Error, Metadata, SourceRow, StoreBackend};
use tempfile::tempdir;

use super::common::{add_store_and_source, backend_with_store_and_source, chunk_record};
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

/// A minimal **discovered feed entry** `ChunkRecord` fixture, owned by
/// `src-feed` and stamped with an `external_id` the way `FeedIngestor`
/// stamps every entry it yields — which is what makes it a liveness
/// candidate at all. See `feed_root_chunk_record` for the one feed resource
/// that carries none.
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
        external_id: Some(format!("urn:entry:{resource_id}")),
        external_etag: None,
    }
}

/// The feed's own document as single-document mode
/// (`fetch_full_content: false`) stores it: a `feed` resource under the feed
/// URL, and the only one with no `external_id`.
fn feed_root_chunk_record(resource_id: &str, uri: &str) -> ChunkRecord {
    ChunkRecord {
        external_id: None,
        ..feed_chunk_record(resource_id, uri)
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

/// A link-less feed entry is stored under a synthetic
/// `{feed_url}#entry:{id}` URI (specs/02-domain-model.md's "General
/// connector pattern"). HTTP never sends a fragment on the wire, so probing
/// that URI verbatim would actually request the feed root rather than the
/// entry — a 404/410 there would delete the entry's resource on a signal
/// that has nothing to do with it. The candidate query must exclude it even
/// though it is the oldest (NULL `last_checked_at`) row in the table, so it
/// never consumes a batch-cap slot either.
#[tokio::test]
async fn list_stale_feed_resources_excludes_fragment_uris() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    handle
        .upsert_chunks(vec![feed_chunk_record(
            "feed-fragment",
            "https://feed.example.com/feed.xml#entry:entry-1",
        )])
        .await
        .unwrap();
    handle
        .upsert_chunks(vec![feed_chunk_record(
            "feed-real-link",
            "https://a.example.com/",
        )])
        .await
        .unwrap();

    let candidates = handle
        .list_stale_feed_resources("store-1", "src-feed", "2099-01-01T00:00:00Z", 10)
        .await
        .unwrap();

    let ids: Vec<&str> = candidates.iter().map(|c| c.resource_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["feed-real-link"],
        "a URI carrying a fragment must never be returned as a candidate, even though it \
         is the oldest (never-checked) row: {candidates:?}"
    );
}

/// In single-document mode the feed document itself is stored as a resource,
/// under the feed URL, with `ingestor_kind = 'feed'` — so it matches every
/// other predicate in the candidate query. Were it a candidate, a 404/410 on
/// the feed URL would delete the source's entire index through a mechanism
/// meant to prune a single entry. `external_id IS NOT NULL` is what separates
/// it from the entries: every discovered entry is stamped with the entry's own
/// id, the feed root with none. The filter is SQL, so only a real-DB test can
/// prove it.
#[tokio::test]
async fn list_stale_feed_resources_excludes_the_feed_root() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    handle
        .upsert_chunks(vec![feed_root_chunk_record(
            "feed-root",
            "https://feed.example.com/feed.xml",
        )])
        .await
        .unwrap();
    handle
        .upsert_chunks(vec![feed_chunk_record(
            "feed-entry",
            "https://a.example.com/",
        )])
        .await
        .unwrap();

    let candidates = handle
        .list_stale_feed_resources("store-1", "src-feed", "2099-01-01T00:00:00Z", 10)
        .await
        .unwrap();

    let ids: Vec<&str> = candidates.iter().map(|c| c.resource_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["feed-entry"],
        "the feed's own document must never be a liveness candidate: {candidates:?}"
    );
}

/// A feed entry's `<link>` need not be an HTTP URL. `Uri::parse` accepts
/// `mailto:` and `ftp:`, and the feed ingestor indexes such an entry from its
/// embedded content under that very URI. Handing one to the HTTP fetcher can
/// only fail — never a 404/410, so never a wrong delete — but it burns one of
/// the run's 25 probe slots, every run, on a request that could not have told
/// us anything. The query excludes them by scheme.
#[tokio::test]
async fn list_stale_feed_resources_excludes_non_http_schemes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    for (id, uri) in [
        ("feed-mailto", "mailto:someone@example.com"),
        ("feed-ftp", "ftp://ftp.example.com/pub/paper.txt"),
        ("feed-file", "file:///home/user/notes.md"),
        ("feed-http", "http://a.example.com/"),
        ("feed-https", "https://b.example.com/"),
    ] {
        handle
            .upsert_chunks(vec![feed_chunk_record(id, uri)])
            .await
            .unwrap();
    }

    let candidates = handle
        .list_stale_feed_resources("store-1", "src-feed", "2099-01-01T00:00:00Z", 10)
        .await
        .unwrap();

    let mut ids: Vec<&str> = candidates.iter().map(|c| c.resource_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["feed-http", "feed-https"],
        "only URIs an HTTP probe can actually resolve may become candidates: {candidates:?}"
    );
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

// ---------------------------------------------------------------------------
// touch_resource_checked — the entry-loop/`url`-source/single-doc-feed
// counterpart to `touch_resource_liveness` above (specs/04-search-pipeline.md
// §1 "What a probe writes"): a single-column write, outside the sweep.
// ---------------------------------------------------------------------------

/// The core contract: `touch_resource_checked` advances `last_checked_at` to
/// a recent timestamp, leaves the validators and `index_updated_at`
/// byte-for-byte as they were, and the write is visible through
/// `list_indexed_documents`.
#[tokio::test]
async fn touch_resource_checked_sets_last_checked_at_and_writes_nothing_else() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let mut record = feed_chunk_record("feed-1", "https://a.example.com/");
    record.external_etag = Some("\"v1\"".to_string());
    handle.upsert_chunks(vec![record]).await.unwrap();
    // A prior liveness touch seeds `external_last_modified` — a column
    // `upsert_chunks` alone cannot set (see `list_stale_feed_resources_
    // round_trips_validators` above) — so the "leaves validators untouched"
    // assertion below has something in both validator columns to disturb.
    handle
        .touch_resource_liveness(
            "store-1",
            "feed-1",
            Some("\"v1\""),
            Some("Wed, 21 Oct 2015 07:28:00 GMT"),
        )
        .await
        .unwrap();

    let (etag_before, last_modified_before, last_checked_before, index_updated_before) =
        resource_liveness_columns(&backend, "feed-1").await;
    assert!(
        last_checked_before.is_some(),
        "seeded by touch_resource_liveness above"
    );

    let before = chrono::Utc::now();
    handle
        .touch_resource_checked("store-1", "feed-1")
        .await
        .unwrap();
    let after = chrono::Utc::now();

    let (etag_after, last_modified_after, last_checked_after, index_updated_after) =
        resource_liveness_columns(&backend, "feed-1").await;
    assert_eq!(
        etag_after, etag_before,
        "touch_resource_checked must never write external_etag"
    );
    assert_eq!(
        last_modified_after, last_modified_before,
        "touch_resource_checked must never write external_last_modified"
    );
    assert_eq!(
        index_updated_after, index_updated_before,
        "touch_resource_checked must never bump index_updated_at"
    );
    let last_checked_after = last_checked_after.expect("touch_resource_checked must set it");
    let parsed = chrono::DateTime::parse_from_rfc3339(&last_checked_after)
        .expect("last_checked_at must be a valid RFC 3339 timestamp")
        .with_timezone(&chrono::Utc);
    assert!(
        parsed >= before - chrono::Duration::seconds(1) && parsed <= after,
        "last_checked_at ({parsed}) must be a recent timestamp, between {before} and {after}"
    );

    let documents = handle.list_indexed_documents().await.unwrap();
    let doc = documents
        .iter()
        .find(|d| d.resource_id == "feed-1")
        .expect("the touched resource must still be listed");
    assert_eq!(
        doc.last_checked_at.as_deref(),
        Some(last_checked_after.as_str()),
        "list_indexed_documents must surface the touched last_checked_at"
    );
}

/// Zero-rows semantics mirror `touch_resource_liveness`'s own guard: a
/// concurrent delete racing the check must be a reported error, never a
/// silent no-op.
#[tokio::test]
async fn touch_resource_checked_errors_when_no_row_matches() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let err = handle
        .touch_resource_checked("store-1", "does-not-exist")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::ResourceNotFound {
            id: "does-not-exist".to_string()
        }
    );
}

/// A `TenantStore` handle rejects a `store_id` argument that is not its own
/// — the same tenant-boundary check every other write on the trait performs
/// — before it ever reaches the database.
#[tokio::test]
async fn touch_resource_checked_rejects_foreign_store_id() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_feed_source(&backend).await;
    add_store_and_source(&backend, "store-2", "src-2", "/other").await;
    let handle = backend.retrieval_store("store-1").await.unwrap();

    let err = handle
        .touch_resource_checked("store-2", "anything")
        .await
        .unwrap_err();
    match err {
        Error::Internal { correlation_id, .. } => {
            assert_eq!(correlation_id, "store_handle_tenant_violation");
        }
        other => panic!("expected Error::Internal, got {other:?}"),
    }
}

/// Even when the `store_id` argument matches the handle's own store (so the
/// tenant-boundary check above passes), a resource that actually belongs to
/// a different store must not be touchable: the `UPDATE ... WHERE store_id =
/// ? AND id = ?` matches zero rows, which is the same `ResourceNotFound` a
/// genuinely unknown resource id produces.
#[tokio::test]
async fn touch_resource_checked_cannot_touch_a_resource_owned_by_another_store() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = backend_with_store_and_source(&path).await;
    add_store_and_source(&backend, "store-2", "src-2", "/other").await;

    let other = backend.retrieval_store("store-2").await.unwrap();
    let mut seed = chunk_record("2026-07-01T00:00:00Z", Some("2026-07-01T00:00:00Z"));
    seed.id = "chunk-2".to_string();
    seed.resource_id = "doc-2".to_string();
    seed.store_id = "store-2".to_string();
    seed.origin_store = "store-2".to_string();
    seed.source_id = "src-2".to_string();
    seed.uri = "file:///other/doc.md".to_string();
    other.upsert_chunks(vec![seed]).await.unwrap();

    let handle = backend.retrieval_store("store-1").await.unwrap();
    let err = handle
        .touch_resource_checked("store-1", "doc-2")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::ResourceNotFound {
            id: "doc-2".to_string()
        },
        "a store-1 handle must not be able to touch store-2's resource, even under store-1's own id"
    );
}
