//! Regression tests for bugs that were fixed after code review.
//!
//! Each test should document:
//!   - The original bug / finding reference
//!   - Why the fix is correct
//!   - What the observable failure was before the fix

use tempfile::tempdir;

use localdb_core::metadata::Metadata;
use localdb_core::store::ChunkRecord;
use localdb_core::types::{Source, SourceKind, SourceSpec, Span, StoreVisibility};
use localdb_core::{
    content_hash, index_resource, resource_id, Block, BlockKind, ChunkerConfig, FakeEmbedder,
    IndexResourceDeps, IngestionConfig, IngestorKind, Resource, ResourceKind, SourceRow,
    StoreBackend, StoreBackendConfig, StoreRow, Uri, VectorEncoding,
};
use store_libsql::SqliteBackend;

async fn open_db_2d() -> (tempfile::TempDir, SqliteBackend) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        2,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    (dir, backend)
}

async fn seed_store(db: &SqliteBackend, store_id: &str, chunks: Vec<ChunkRecord>) {
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

    db.upsert_source(&SourceRow {
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

    db.retrieval_store(store_id)
        .await
        .unwrap()
        .upsert_chunks(chunks)
        .await
        .unwrap();
}

fn make_chunk(id: &str, doc_id: &str, store_id: &str, embedding: Vec<f32>) -> ChunkRecord {
    let text = format!("text for {id}");
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
        source_id: format!("src-{store_id}"),
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: format!("file:///data/{store_id}/{doc_id}.md"),
        metadata: Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
    }
}

/// Regression test for finding F1: the overfetch/widen loop in `dense_search`
/// must always treat the implicit `store_id` WHERE clause as a filter.
///
/// **Bug**: When `filters` is empty (`has_filters = false`), the old code set
/// `fetch_k = limit` (instead of `limit * 3`) *and* exited the widening loop
/// unconditionally after the first pass via `|| !has_filters`.  If all globally
/// nearest chunks belonged to a different store, the search returned 0 results
/// for the queried store even though matching chunks existed.
///
/// **Setup**:
/// - Store-B holds 5 chunks that are the globally nearest neighbours to the
///   query vector `[1.0, 0.0]`.
/// - Store-A holds 1 chunk that ranks 6th globally.
/// - `limit = 1`, no user filters.
///
/// **Expected**: the widening loop must expand `fetch_k` until it captures
/// store-A's chunk and return it (1 result).  Before the fix this returned 0.
#[tokio::test]
async fn dense_search_no_user_filters_returns_results_when_dominated_by_other_store() {
    let (_dir, db) = open_db_2d().await;

    // Store-B: 5 chunks very close to query [1.0, 0.0] — they dominate the
    // global ANN top-k.  Vectors differ only in the tiny y-component so they
    // all have cosine-distance ≈ 0 to the query.
    let b_chunks: Vec<ChunkRecord> = (0..5)
        .map(|i| {
            make_chunk(
                &format!("b-chunk-{i}"),
                &format!("b-doc-{i}"),
                "store-B",
                vec![1.0, 0.001 * (i as f32 + 1.0)],
            )
        })
        .collect();
    seed_store(&db, "store-B", b_chunks).await;

    // Store-A: 1 chunk farther from the query — ranks below all store-B chunks
    // globally (cosine distance ≈ 0.29 vs ≈ 0 for store-B).
    let a_chunks = vec![make_chunk(
        "a-chunk-0",
        "a-doc-0",
        "store-A",
        vec![0.7, 0.7],
    )];
    seed_store(&db, "store-A", a_chunks).await;

    let handle_a = db.retrieval_store("store-A").await.unwrap();

    // Search store-A with no user filters (only the implicit store_id predicate).
    let results = handle_a.dense_search(&[1.0, 0.0], 1, &[]).await.unwrap();

    assert_eq!(
        results.len(),
        1,
        "dense_search should return store-A's chunk even though all store-B \
         chunks rank higher in the global ANN index. Got: {results:?}"
    );
    assert_eq!(
        results[0].chunk.id, "a-chunk-0",
        "the returned chunk should be the one belonging to store-A"
    );
}

/// Regression: fallback gating — a store with fewer chunks than `limit` and no
/// competing tenants must return all its chunks without error and in distance
/// order. Confirms the exact-scan fallback handles legitimate partial results
/// gracefully (fires because ANN saturates at max_fetch, but returns correctly).
#[tokio::test]
async fn dense_search_small_store_returns_partial_results_correctly() {
    let (_dir, db) = open_db_2d().await;

    // Only 2 chunks in the store, searched with limit=5.
    seed_store(
        &db,
        "small-store",
        vec![
            make_chunk("chunk-0", "doc-0", "small-store", vec![0.7, 0.7]),
            make_chunk("chunk-1", "doc-1", "small-store", vec![0.9, 0.1]),
        ],
    )
    .await;

    let handle = db.retrieval_store("small-store").await.unwrap();
    let results = handle.dense_search(&[1.0, 0.0], 5, &[]).await.unwrap();

    assert_eq!(
        results.len(),
        2,
        "should return all 2 chunks even though limit=5; got {results:?}"
    );
    // chunk-1 ([0.9, 0.1]) is closer to query [1.0, 0.0] than chunk-0 ([0.7, 0.7]).
    assert_eq!(results[0].chunk.id, "chunk-1");
    assert_eq!(results[1].chunk.id, "chunk-0");
}

/// Regression: WS4 — when another store has > limit*20 chunks all closer to
/// the query vector, the ANN widen loop saturates without returning the target
/// store's chunk. The exact-scan fallback must rescue it.
///
/// Bug: dense_search returned 0 results for store-A when store-B had 25 chunks
/// (> limit*20 = 20) all ranked higher globally. Proven red→green.
#[tokio::test]
async fn dense_search_exact_fallback_when_ann_cap_saturated() {
    let (_dir, db) = open_db_2d().await;

    // Store-B: 25 chunks all very close to query [1.0, 0.0] — more than
    // limit*20 = 20, so the widen loop can never surface store-A's chunk.
    let b_chunks: Vec<ChunkRecord> = (0..25)
        .map(|i| {
            make_chunk(
                &format!("b-chunk-{i}"),
                &format!("b-doc-{i}"),
                "store-B",
                vec![1.0, 0.0001 * (i as f32 + 1.0)],
            )
        })
        .collect();
    seed_store(&db, "store-B", b_chunks).await;

    // Store-A: 1 chunk farther from query — globally ranked below all store-B chunks.
    seed_store(
        &db,
        "store-A",
        vec![make_chunk(
            "a-chunk-0",
            "a-doc-0",
            "store-A",
            vec![0.7, 0.7],
        )],
    )
    .await;

    let handle_a = db.retrieval_store("store-A").await.unwrap();
    let results = handle_a.dense_search(&[1.0, 0.0], 1, &[]).await.unwrap();

    assert_eq!(
        results.len(),
        1,
        "dense_search must return store-A's chunk via exact-scan fallback \
         when ANN is saturated by store-B's 25 chunks. Got: {results:?}"
    );
    assert_eq!(results[0].chunk.id, "a-chunk-0");
}

/// Regression for Codex round-3 finding R2: a resource's *ingestion* time must
/// be what lands in `resources.added_at`, not the date its source claims.
///
/// **Bug**: `index_resource` built `Provenance { fetched_at: resource.modified_at }`
/// and `upsert_chunks` binds `fetched_at` to the `added_at` column, whose
/// `ON CONFLICT` clause never updates it. A feed entry dated 2020 and ingested
/// today therefore recorded 2020 as its acquisition time — permanently.
///
/// **Observable failure**: `find_document` maps `resources.added_at` →
/// `DocumentInfo.fetched_at`, so every citation reported a 2020 acquisition
/// time, and `MetadataFilter::FetchedAfter` (which filters on `r.added_at`)
/// excluded the entry from any "fetched since last week" query.
///
/// **Why the fix is correct**: `Provenance.fetched_at` is defined as
/// acquisition time (specs/02-domain-model.md §4), and `added_at` is spec'd as
/// "always ingestion-time `now()` … never a feed-claimed date"
/// (specs/02-domain-model.md, Feed connector "Timestamps"). Reading
/// `resource.added_at` is the only value consistent with both.
///
/// Only feed sources make the two fields differ — `file` and `url` set both to
/// the same value — which is why no unit test caught it: `FakeStore` has no
/// separate `added_at`/`modified_at` columns, so the collapse is invisible
/// without the real backend.
#[tokio::test]
async fn indexed_resource_persists_ingestion_time_not_feed_date_in_added_at() {
    const INGESTED_AT: &str = "2026-08-05T09:30:00Z";
    const FEED_CLAIMED: &str = "2020-01-01T00:00:00Z";

    let (_dir, db) = open_db_2d().await;
    let store_id = "feed-store";
    // Seeds the store + source rows the `resources` insert refers to.
    seed_store(&db, store_id, vec![]).await;

    let source = Source {
        id: format!("src-{store_id}"),
        store_id: store_id.to_string(),
        kind: SourceKind::Feed,
        spec: SourceSpec::Feed {
            url: "https://blog.example.com/feed.xml".to_string(),
            max_entries: None,
            fetch_full_content: false,
            refresh_interval_secs: None,
        },
        source_preset: "prose".to_string(),
    };

    let uri = "https://blog.example.com/2020/old-post";
    let body = "An old post that the feed is only surfacing to us today.";
    let hash = content_hash(body);
    let resource = Resource {
        id: resource_id(uri, &hash),
        store_id: store_id.to_string(),
        source_id: source.id.clone(),
        ingestor_kind: IngestorKind::Feed,
        resource_kind: ResourceKind::Document,
        uri: Uri::parse(uri).unwrap(),
        external_id: None,
        external_etag: None,
        content_hash: hash,
        title: Some("Old post".to_string()),
        mime: Some("text/markdown".to_string()),
        metadata: Metadata::default(),
        // The feed connector's shape: acquisition is now, the feed claims 2020.
        added_at: INGESTED_AT.to_string(),
        modified_at: FEED_CLAIMED.to_string(),
        thread_id: None,
        channel: None,
        participants: vec![],
        origin_store: store_id.to_string(),
        policy_version: "v1".to_string(),
        share_path: None,
        extractor_version: "1".to_string(),
        blocks: vec![Block {
            seq: 0,
            kind: BlockKind::Text,
            text: body.to_string(),
            location: None,
        }],
    };

    let handle = db.retrieval_store(store_id).await.unwrap();
    let embedder = FakeEmbedder::new(2);
    let config = IngestionConfig {
        store_id: store_id.to_string(),
        policy_version: "v1".to_string(),
        chunker: ChunkerConfig::prose(),
    };
    let deps = IndexResourceDeps {
        store: handle.as_ref(),
        embedder: &embedder,
        config: &config,
    };
    let written = index_resource(&resource, &source, None, &deps)
        .await
        .unwrap();
    assert!(written > 0, "the resource must produce at least one chunk");

    let info = db
        .find_document(&resource.id)
        .await
        .unwrap()
        .expect("the indexed resource must be readable back");
    assert_eq!(
        info.fetched_at, INGESTED_AT,
        "resources.added_at must hold the ingestion time, not the feed's date"
    );
}
