//! T329: the due-entry revisit loop on a feed-level 304
//! (specs/04-search-pipeline.md §1 "Due-entry revisit on a feed 304"). Drives
//! the real `run_source_ingestion` + `PipelineCallback` machinery — not a
//! bespoke test callback — across two runs sharing one `DocumentIndex`,
//! exactly like `recheck_gate.rs`. `PipelineCallback::due_entries_for_source`
//! (the candidate-selection logic itself) is already covered by `core`'s own
//! tests, so every test here is about the *loop*'s wiring: whether a due
//! entry's link actually gets fetched on a feed 304, through which fetcher,
//! and how each outcome gets reported.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Duration, SecondsFormat, Utc};
use ingest::FeedIngestor;
use localdb_core::embedder::FakeEmbedder;
use localdb_core::error::Error;
use localdb_core::ingestion::{
    run_source_ingestion, DocumentIndex, FetchMetadata, FetchResult, IngestionConfig,
    SourceIngestionDeps, UrlFetcher,
};
use localdb_core::parser::{ParsedDocument, Parser, Probe};
use localdb_core::store::{FakeStore, RetrievalStore};
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::ChunkerConfig;

const FEED_URL: &str = "https://feed.example.com/feed.xml";

fn entry_url(n: usize) -> String {
    format!("https://feed.example.com/entry-{n}")
}

/// Pass-through parser: every input is UTF-8 Markdown, no title, no
/// metadata — mirrors the identical helper in `recheck_gate.rs` and
/// `feed_metadata_refresh_on_304.rs`. Declines (`Ok(None)`) for the one
/// fixture body that opts into the `UNSUPPORTED_MARKER` sentinel, standing in
/// for a format this parser chain does not handle.
struct PlainParser;
impl Parser for PlainParser {
    fn id(&self) -> &'static str {
        "plain"
    }
    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        if probe.bytes() == b"UNSUPPORTED_MARKER" {
            return Ok(None);
        }
        Ok(Some(ParsedDocument {
            markdown: String::from_utf8_lossy(probe.bytes()).to_string(),
            title: None,
            metadata: localdb_core::metadata::DublinCoreMetadata::default(),
            page_starts: Vec::new(),
        }))
    }
}

/// One scripted response for one call to one URL — mirrors `recheck_gate
/// .rs`'s `Outcome`/`CountingFetcher` shape, extended with `NotModified` and
/// `Gone` (`feed_metadata_refresh_on_304.rs`'s `ScriptedFetcher` covers those,
/// but keys its script by `FetchResult` rather than call-counting cleanly per
/// URL, which every test here needs).
#[derive(Clone)]
enum Outcome {
    Downloaded {
        bytes: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    NotModified {
        etag: Option<String>,
        last_modified: Option<String>,
    },
    Gone,
}

fn downloaded(bytes: impl Into<Vec<u8>>) -> Outcome {
    Outcome::Downloaded {
        bytes: bytes.into(),
        etag: Some("\"v1\"".to_string()),
        last_modified: None,
    }
}

fn not_modified() -> Outcome {
    Outcome::NotModified {
        etag: None,
        last_modified: None,
    }
}

/// A fetcher scripted per URL: a fixed sequence of [`Outcome`]s consumed in
/// call order, the last one repeating once the script runs dry. Records a
/// plain per-URL call count — what every test here actually asserts on,
/// since a URI the loop never revisits makes no call at all.
#[derive(Default)]
struct ScriptedFetcher {
    script: Mutex<HashMap<String, Vec<Outcome>>>,
    calls: Mutex<HashMap<String, usize>>,
}

impl ScriptedFetcher {
    fn new(script: HashMap<String, Vec<Outcome>>) -> Self {
        Self {
            script: Mutex::new(script),
            calls: Mutex::new(HashMap::new()),
        }
    }

    fn call_count(&self, url: &str) -> usize {
        self.calls.lock().unwrap().get(url).copied().unwrap_or(0)
    }
}

#[async_trait]
impl UrlFetcher for ScriptedFetcher {
    async fn fetch(&self, url: &str, _meta: &FetchMetadata) -> Result<FetchResult, Error> {
        let call_idx = {
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(url.to_string()).or_insert(0);
            let idx = *n;
            *n += 1;
            idx
        };
        let script = self.script.lock().unwrap();
        let outcomes = script.get(url).ok_or_else(|| Error::Internal {
            message: format!("no scripted outcome for {url}"),
            correlation_id: "recheck_due_entry_loop_test_no_script".to_string(),
        })?;
        match &outcomes[call_idx.min(outcomes.len().saturating_sub(1))] {
            Outcome::Downloaded {
                bytes,
                etag,
                last_modified,
            } => Ok(FetchResult::Downloaded {
                bytes: bytes.clone(),
                content_type: Some("text/markdown".to_string()),
                etag: etag.clone(),
                last_modified: last_modified.clone(),
                final_url: None,
            }),
            Outcome::NotModified {
                etag,
                last_modified,
            } => Ok(FetchResult::NotModified {
                etag: etag.clone(),
                last_modified: last_modified.clone(),
            }),
            Outcome::Gone => Ok(FetchResult::Gone),
        }
    }
}

/// Shares one `ScriptedFetcher` between the two `Box<dyn UrlFetcher>` slots
/// `FeedIngestor` owns (feed document and entry links) — mirrors every other
/// test file's `ArcFetcher`.
struct ArcFetcher(Arc<ScriptedFetcher>);
#[async_trait]
impl UrlFetcher for ArcFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.0.fetch(url, meta).await
    }
}

/// One Atom entry: `(id, link, published, updated)` — mirrors
/// `recheck_gate.rs`'s identical helper.
fn atom_feed(entries: &[(&str, &str, &str, &str)]) -> Vec<u8> {
    let items: String = entries
        .iter()
        .map(|(id, link, published, updated)| {
            format!(
                r#"<entry><title>{id}</title><id>urn:{id}</id><published>{published}</published><updated>{updated}</updated><link href="{link}" rel="alternate"/><summary>Summary for {id}</summary></entry>"#
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom"><title>Test Feed</title><id>urn:feed</id><updated>2026-01-05T00:00:00Z</updated><link href="{FEED_URL}" rel="self"/>{items}</feed>"#
    )
    .into_bytes()
}

fn feed_source(store_id: &str, fetch_full_content: bool) -> Source {
    Source {
        id: "source-1".to_string(),
        store_id: store_id.to_string(),
        kind: SourceKind::Feed,
        spec: SourceSpec::Feed {
            url: FEED_URL.to_string(),
            max_entries: None,
            fetch_full_content,
            refresh_interval_secs: None,
        },
        source_preset: "prose".to_string(),
    }
}

fn make_config(store_id: &str) -> IngestionConfig {
    IngestionConfig {
        store_id: store_id.to_string(),
        policy_version: "policy-v1".to_string(),
        chunker: ChunkerConfig::prose(),
    }
}

/// Push `uri`'s cached `last_checked_at` back past the recheck floor (24h,
/// the default with no configured `refresh_interval_secs`) — the same
/// backdating `core`'s own `seed_due_candidate` test helper performs one
/// layer down, done here through the public `DocumentIndex` API since these
/// tests drive the real `run_source_ingestion` + `PipelineCallback`
/// machinery rather than constructing a `PipelineCallback` directly.
fn backdate_past_floor(doc_index: &mut DocumentIndex, uri: &str) {
    let mut record = doc_index
        .get(uri)
        .expect("uri must already be indexed before it can be backdated")
        .clone();
    record.last_checked_at =
        Some((Utc::now() - Duration::hours(30)).to_rfc3339_opts(SecondsFormat::Secs, true));
    doc_index.upsert(record);
}

// ---------------------------------------------------------------------------
// 1. Starvation regression: the due-entry loop is what makes the floor an
//    actual ceiling.
// ---------------------------------------------------------------------------

/// Run 1 indexes a two-entry feed. Every later run's feed document is a bare
/// 304. One entry's `last_checked_at` is backdated past the recheck floor,
/// the other stays fresh. Before this loop existed, a 304 fired zero entry
/// callbacks and the stale entry would never be re-verified however long the
/// feed kept quiet (specs/04-search-pipeline.md §1 "Due-entry revisit on a
/// feed 304").
#[tokio::test]
async fn a_stale_entrys_link_is_refetched_on_a_feed_304_while_a_fresh_entrys_is_not() {
    let store_id = "store-1";
    let stale_link = entry_url(0);
    let fresh_link = entry_url(1);
    let entries = [
        (
            "e0",
            stale_link.as_str(),
            "2026-01-05T00:00:00Z",
            "2026-01-05T00:00:00Z",
        ),
        (
            "e1",
            fresh_link.as_str(),
            "2026-01-05T00:00:00Z",
            "2026-01-05T00:00:00Z",
        ),
    ];
    let feed_v1 = atom_feed(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![downloaded(feed_v1), not_modified()],
    );
    script.insert(
        stale_link.clone(),
        vec![
            downloaded(b"# Entry 0 v1\n\nBody.".to_vec()),
            downloaded(b"# Entry 0 v2\n\nBody changed.".to_vec()),
        ],
    );
    script.insert(
        fresh_link.clone(),
        vec![downloaded(b"# Entry 1 v1\n\nBody.".to_vec())],
    );
    let fetcher = Arc::new(ScriptedFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id, true);
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = make_config(store_id);
    let mut doc_index = DocumentIndex::new();

    let first = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();
    assert_eq!(first.docs_indexed, 2, "run 1 indexes both entries");

    // Backdate only the stale entry past the floor; the fresh entry keeps
    // whatever last_checked_at run 1 just stamped.
    backdate_past_floor(&mut doc_index, &stale_link);

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(fetcher.call_count(FEED_URL), 2, "one feed request per run");
    assert_eq!(
        fetcher.call_count(&stale_link),
        2,
        "the stale entry's link must be fetched again on the feed-304 run"
    );
    assert_eq!(
        fetcher.call_count(&fresh_link),
        1,
        "the fresh entry's link must never be fetched again"
    );

    assert_eq!(
        second.docs_indexed, 1,
        "the stale entry's changed body must be reindexed — a real fetch \
         outcome, not a gate deferral"
    );
    assert_eq!(
        second.docs_recheck_deferred, 0,
        "a due-entry revisit is never counted as a gate deferral — it made a \
         real conditional GET"
    );
    assert_eq!(
        second.docs_seen, 1,
        "only the due entry is visited this run: the feed's 304 short-circuits \
         the ordinary entry loop entirely, so the fresh entry is never touched"
    );
}

// ---------------------------------------------------------------------------
// 2. The due-entry loop uses the restricted, public-only entry fetcher.
// ---------------------------------------------------------------------------

/// Entry links are third-party content inside the feed document (the SSRF
/// trust boundary `FeedIngestor::new`'s doc comment describes); the due-entry
/// revisit must go through the same restricted fetcher the ordinary entry
/// loop uses, never the unrestricted feed-document fetcher. Uses two
/// genuinely distinct fetcher instances so each one's own call count is
/// unambiguous proof of which one actually served the request.
#[tokio::test]
async fn due_entry_loop_uses_the_public_only_entry_fetcher() {
    let store_id = "store-1";
    let link = entry_url(0);
    let entries = [(
        "e0",
        link.as_str(),
        "2026-01-05T00:00:00Z",
        "2026-01-05T00:00:00Z",
    )];
    let feed_v1 = atom_feed(&entries);

    let mut feed_script = HashMap::new();
    feed_script.insert(
        FEED_URL.to_string(),
        vec![downloaded(feed_v1), not_modified()],
    );
    let feed_fetcher = Arc::new(ScriptedFetcher::new(feed_script));

    let mut entry_script = HashMap::new();
    entry_script.insert(
        link.clone(),
        vec![
            downloaded(b"# Entry\n\nBody.".to_vec()),
            downloaded(b"# Entry v2\n\nBody changed.".to_vec()),
        ],
    );
    let entry_fetcher = Arc::new(ScriptedFetcher::new(entry_script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(feed_fetcher.clone())),
        Box::new(ArcFetcher(entry_fetcher.clone())),
    );
    let source = feed_source(store_id, true);
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = make_config(store_id);
    let mut doc_index = DocumentIndex::new();

    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    backdate_past_floor(&mut doc_index, &link);

    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(
        entry_fetcher.call_count(&link),
        2,
        "the entry (public-only) fetcher must receive the due-entry revisit request"
    );
    assert_eq!(
        feed_fetcher.call_count(&link),
        0,
        "the feed (unrestricted) fetcher must never receive an entry-link request"
    );
}

// ---------------------------------------------------------------------------
// 3. A due entry's link answers Gone: the row stays untouched under Retain.
// ---------------------------------------------------------------------------

/// `process_url`'s `Gone` arm self-reports nothing at all (no `on_resource`,
/// no `on_skipped`) — reclaiming a confirmed-gone entry stays the liveness
/// sweep's job under `--delete`. Under the default `Retain` policy (and the
/// feed source's blanket delete-sweep exemption either way), the row must
/// simply survive with its `last_checked_at` unmoved.
#[tokio::test]
async fn due_entry_loop_gone_leaves_the_row_untouched_under_retain() {
    let store_id = "store-1";
    let link = entry_url(0);
    let entries = [(
        "e0",
        link.as_str(),
        "2026-01-05T00:00:00Z",
        "2026-01-05T00:00:00Z",
    )];
    let feed_v1 = atom_feed(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![downloaded(feed_v1), not_modified()],
    );
    script.insert(
        link.clone(),
        vec![downloaded(b"# Entry\n\nBody.".to_vec()), Outcome::Gone],
    );
    let fetcher = Arc::new(ScriptedFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id, true);
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = make_config(store_id);
    let mut doc_index = DocumentIndex::new();

    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    backdate_past_floor(&mut doc_index, &link);
    let resource_id = doc_index.get(&link).unwrap().resource_id.clone();
    let last_checked_before = store.last_checked_at(&resource_id).await;

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(
        fetcher.call_count(&link),
        2,
        "the due entry's link must still be fetched"
    );
    assert_eq!(
        second.docs_seen, 0,
        "Gone is silent on this path: no on_resource, no on_skipped"
    );
    assert_eq!(second.error_count, 0);
    let chunks = store.get_chunks_for_resource(&resource_id).await.unwrap();
    assert!(
        !chunks.is_empty(),
        "the row must not be deleted: Retain (and the feed source's delete-sweep \
         exemption either way) leaves it in place"
    );
    assert_eq!(
        store.last_checked_at(&resource_id).await,
        last_checked_before,
        "a Gone due-entry revisit must never advance last_checked_at — no origin \
         contact was reported for it"
    );
}

// ---------------------------------------------------------------------------
// 4. A due entry's link answers with unsupported content: reported as an
//    error, since no embedded-content fallback exists on this path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn due_entry_loop_reports_error_on_unsupported_content() {
    let store_id = "store-1";
    let link = entry_url(0);
    let entries = [(
        "e0",
        link.as_str(),
        "2026-01-05T00:00:00Z",
        "2026-01-05T00:00:00Z",
    )];
    let feed_v1 = atom_feed(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![downloaded(feed_v1), not_modified()],
    );
    script.insert(
        link.clone(),
        vec![
            downloaded(b"# Entry\n\nBody.".to_vec()),
            downloaded(b"UNSUPPORTED_MARKER".to_vec()),
        ],
    );
    let fetcher = Arc::new(ScriptedFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id, true);
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = make_config(store_id);
    let mut doc_index = DocumentIndex::new();

    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    backdate_past_floor(&mut doc_index, &link);

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(
        fetcher.call_count(&link),
        2,
        "the due entry's link must still be fetched"
    );
    assert_eq!(
        second.error_count, 1,
        "no embedded fallback exists on this path, so unfetchable content must \
         surface as an error rather than silently degrade the stored resource"
    );
    assert_eq!(
        second.docs_seen, 1,
        "on_skipped(Error(_)) still marks the URI seen"
    );
}

// ---------------------------------------------------------------------------
// 5. `--refetch` bypasses the due-entry loop entirely.
// ---------------------------------------------------------------------------

/// Stands in for a real origin honoring `If-None-Match`: 304s the feed
/// document whenever the request carries a validator, 200s it otherwise.
/// Mirrors `recheck_gate.rs`'s `FeedConditionalFetcher`.
struct FeedConditionalFetcher {
    feed_body: Vec<u8>,
    entry_bodies: HashMap<String, Vec<u8>>,
    calls: Mutex<HashMap<String, usize>>,
    feed_received: Mutex<Vec<FetchMetadata>>,
}

#[async_trait]
impl UrlFetcher for FeedConditionalFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        *self
            .calls
            .lock()
            .unwrap()
            .entry(url.to_string())
            .or_insert(0) += 1;
        if url == FEED_URL {
            self.feed_received.lock().unwrap().push(meta.clone());
            if meta.etag.is_some() || meta.last_modified.is_some() {
                return Ok(FetchResult::NotModified {
                    etag: None,
                    last_modified: None,
                });
            }
            return Ok(FetchResult::Downloaded {
                bytes: self.feed_body.clone(),
                content_type: Some("application/atom+xml".to_string()),
                etag: Some("\"feed-v1\"".to_string()),
                last_modified: None,
                final_url: None,
            });
        }
        let bytes = self.entry_bodies.get(url).ok_or_else(|| Error::Internal {
            message: format!("no scripted body for {url}"),
            correlation_id: "due_entry_loop_refetch_test_no_body".to_string(),
        })?;
        Ok(FetchResult::Downloaded {
            bytes: bytes.clone(),
            content_type: Some("text/markdown".to_string()),
            etag: Some("\"entry-v1\"".to_string()),
            last_modified: None,
            final_url: None,
        })
    }
}

/// `--refetch` clears `document_validators` before the feed fetch
/// (`core::ingestion::run_source_ingestion`), so the request below carries
/// none and the origin — scripted to 304 only when it sees a validator —
/// answers a fresh 200 instead: the ordinary entry loop runs, and the
/// `FetchResult::NotModified` arm that calls `due_entries_for_source` (the
/// due-entry loop's sole call site) is never entered at all. Backdating the
/// entry past the floor first proves this isn't an accident of the entry
/// happening to be fresh — it would already be a due candidate if the loop
/// were ever reached.
#[tokio::test]
async fn refetch_bypasses_the_due_entry_loop_entirely() {
    let store_id = "store-1";
    let link = entry_url(0);
    let entries = [(
        "e0",
        link.as_str(),
        "2026-01-05T00:00:00Z",
        "2026-01-05T00:00:00Z",
    )];
    let feed = atom_feed(&entries);
    let mut entry_bodies = HashMap::new();
    entry_bodies.insert(link.clone(), b"# Entry\n\nBody.".to_vec());
    let fetcher = Arc::new(FeedConditionalFetcher {
        feed_body: feed,
        entry_bodies,
        calls: Mutex::new(HashMap::new()),
        feed_received: Mutex::new(Vec::new()),
    });

    struct Arc2(Arc<FeedConditionalFetcher>);
    #[async_trait]
    impl UrlFetcher for Arc2 {
        async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
            self.0.fetch(url, meta).await
        }
    }

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(Arc2(fetcher.clone())),
        Box::new(Arc2(fetcher.clone())),
    );
    let source = feed_source(store_id, true);
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = make_config(store_id);
    let mut doc_index = DocumentIndex::new();

    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    backdate_past_floor(&mut doc_index, &link);

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps {
            document_validators: FetchMetadata {
                etag: Some("\"feed-v1\"".to_string()),
                last_modified: None,
            },
            refetch: true,
            ..SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config)
        },
    )
    .await
    .unwrap();

    assert_eq!(
        fetcher.feed_received.lock().unwrap().last(),
        Some(&FetchMetadata::default()),
        "--refetch must suppress the stored feed-document validators, or the feed \
         fetch above would have carried them and gotten a 304"
    );
    assert_eq!(
        *fetcher.calls.lock().unwrap().get(&link).unwrap_or(&0),
        2,
        "the ordinary entry loop must run and fetch the entry despite it being \
         backdated past the floor — proof the feed's 304 branch (where the \
         due-entry loop lives) was never entered"
    );
    assert_eq!(
        second.docs_recheck_deferred, 0,
        "--refetch bypasses the gate for every entry, so nothing is deferred"
    );
}

// ---------------------------------------------------------------------------
// 6. Single-document mode's 304 short-circuit is unchanged.
// ---------------------------------------------------------------------------

/// Single-document mode has no per-entry concept at all, so it must keep
/// short-circuiting a feed 304 exactly as it did before this loop existed:
/// one plain unchanged skip, no entry links ever fetched, no due-entry loop
/// engaged.
#[tokio::test]
async fn single_document_mode_304_behavior_unchanged() {
    let store_id = "store-1";
    let unused_entry_link = entry_url(0);
    let entries = [(
        "e0",
        unused_entry_link.as_str(),
        "2026-01-05T00:00:00Z",
        "2026-01-05T00:00:00Z",
    )];
    let feed_v1 = atom_feed(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![downloaded(feed_v1), not_modified()],
    );
    let fetcher = Arc::new(ScriptedFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id, false);
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = make_config(store_id);
    let mut doc_index = DocumentIndex::new();

    let first = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();
    assert_eq!(
        first.docs_indexed, 1,
        "run 1 indexes the feed as a single document"
    );

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(fetcher.call_count(FEED_URL), 2, "one feed request per run");
    assert_eq!(second.docs_seen, 1);
    assert_eq!(
        second.docs_skipped, 1,
        "single-document mode's 304 still short-circuits as a plain unchanged skip"
    );
    assert_eq!(
        second.docs_indexed, 0,
        "no reindex on an unchanged single-document 304"
    );
    assert_eq!(
        second.docs_recheck_deferred, 0,
        "the due-entry loop is a discovery-mode-only concept"
    );
    assert_eq!(second.error_count, 0);
    assert_eq!(
        fetcher.call_count(&entry_url(0)),
        0,
        "single-document mode never fetches an entry link at all"
    );
}
