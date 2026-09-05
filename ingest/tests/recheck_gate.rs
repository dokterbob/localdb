//! T329: the feed entry recheck gate (specs/04-search-pipeline.md §1 "Recheck
//! gate"). Drives the real `run_source_ingestion` + `PipelineCallback`
//! machinery — not a bespoke test callback — across two or three runs
//! sharing one `DocumentIndex`, exactly like `conditional_get_replay.rs`: a
//! double that only recorded outcomes could not distinguish "the gate saved a
//! fetch" from "the gate is silently a no-op and every run just looks the
//! same because nothing changed," so every fetcher here counts calls per URL.
//!
//! Every test uses `SourceIngestionDeps::for_test`'s defaults (24h recheck
//! floor from `recheck_floor_secs`, since no `Source` here configures
//! `refresh_interval_secs`) and runs land well inside a second of each other,
//! so "inside the floor" always holds unless a test asks for `refetch: true`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ingest::{FeedIngestor, UrlIngestor};
use localdb_core::embedder::FakeEmbedder;
use localdb_core::error::Error;
use localdb_core::ingestion::{
    run_source_ingestion, DocumentIndex, FetchMetadata, FetchResult, IngestionConfig,
    SourceIngestionDeps, UrlFetcher,
};
use localdb_core::parser::{ParsedDocument, Parser, Probe};
use localdb_core::store::FakeStore;
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::ChunkerConfig;

/// Pass-through parser: every input is UTF-8 Markdown, no title, no metadata.
/// Mirrors the identical helper in `conditional_get_replay.rs` and
/// `feed_metadata_refresh_on_304.rs`.
struct PlainParser;
impl Parser for PlainParser {
    fn id(&self) -> &'static str {
        "plain"
    }
    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        Ok(Some(ParsedDocument {
            markdown: String::from_utf8_lossy(probe.bytes()).to_string(),
            title: None,
            metadata: localdb_core::metadata::DublinCoreMetadata::default(),
            page_starts: Vec::new(),
        }))
    }
}

/// One scripted response for one call to one URL. `FetchResult` is
/// deliberately not `Clone` in `core`, so the script holds owned primitives
/// and builds a fresh `FetchResult` per call rather than storing (and trying
/// to clone) the enum itself — same reasoning as `feed_metadata_refresh_on_304
/// .rs`'s `clone_result`, applied at construction time instead.
#[derive(Clone)]
enum Outcome {
    Downloaded {
        bytes: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    Error,
}

fn downloaded(bytes: impl Into<Vec<u8>>, etag: &str) -> Outcome {
    Outcome::Downloaded {
        bytes: bytes.into(),
        etag: Some(etag.to_string()),
        last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
    }
}

/// A fetcher scripted per URL: a fixed sequence of [`Outcome`]s consumed in
/// call order, the last one repeating once the script runs dry (matching
/// `feed_metadata_refresh_on_304.rs`'s `ScriptedFetcher`). Records a plain
/// per-URL call count — the thing every test in this file actually asserts
/// on, since a gate-skipped entry makes no call at all — plus the
/// `FetchMetadata` each call received, for the one test that must prove
/// validators were replayed, not just that a fetch happened.
#[derive(Default)]
struct CountingFetcher {
    script: Mutex<HashMap<String, Vec<Outcome>>>,
    calls: Mutex<HashMap<String, usize>>,
    received: Mutex<HashMap<String, Vec<FetchMetadata>>>,
}

impl CountingFetcher {
    fn new(script: HashMap<String, Vec<Outcome>>) -> Self {
        Self {
            script: Mutex::new(script),
            calls: Mutex::new(HashMap::new()),
            received: Mutex::new(HashMap::new()),
        }
    }

    fn call_count(&self, url: &str) -> usize {
        self.calls.lock().unwrap().get(url).copied().unwrap_or(0)
    }

    fn received_for(&self, url: &str) -> Vec<FetchMetadata> {
        self.received
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl UrlFetcher for CountingFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        let call_idx = {
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(url.to_string()).or_insert(0);
            let idx = *n;
            *n += 1;
            idx
        };
        self.received
            .lock()
            .unwrap()
            .entry(url.to_string())
            .or_default()
            .push(meta.clone());
        let script = self.script.lock().unwrap();
        let outcomes = script.get(url).ok_or_else(|| Error::Internal {
            message: format!("no scripted outcome for {url}"),
            correlation_id: "recheck_gate_test_no_script".to_string(),
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
            Outcome::Error => Err(Error::Internal {
                message: format!("scripted fetch error for {url}"),
                correlation_id: "recheck_gate_test_scripted_error".to_string(),
            }),
        }
    }
}

/// Shares one `CountingFetcher` between the two `Box<dyn UrlFetcher>` slots
/// `FeedIngestor` owns (feed document and entry links) and a handle the test
/// keeps to inspect afterward — mirrors every other test file's `ArcFetcher`.
struct ArcFetcher(Arc<CountingFetcher>);
#[async_trait]
impl UrlFetcher for ArcFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.0.fetch(url, meta).await
    }
}

const FEED_URL: &str = "https://feed.example.com/feed.xml";

/// One Atom entry: `(id, link, published, updated)`. `published` is held
/// constant across every test run here; bumping only `updated` changes
/// `ResourceEnrichment::modified_at_override` (which prefers `updated`)
/// without touching `date` (which prefers `published`) — an isolated,
/// single-field way to make one entry's claim stop reproducing its stored
/// `metadata_hash` (recheck gate check (b)) without disturbing the others.
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

fn feed_source(store_id: &str) -> Source {
    Source {
        id: "source-1".to_string(),
        store_id: store_id.to_string(),
        kind: SourceKind::Feed,
        spec: SourceSpec::Feed {
            url: FEED_URL.to_string(),
            max_entries: None,
            fetch_full_content: true,
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

/// The three entries every gate test starts from: distinct URLs, identical
/// dates, so nothing in the feed's claim moves unless a test deliberately
/// bumps one entry's `<updated>` below.
const BASELINE_DATE: &str = "2026-01-05T00:00:00Z";

fn entry_url(n: usize) -> String {
    format!("https://feed.example.com/entry-{n}")
}

fn baseline_entries() -> Vec<(String, String, String, String)> {
    (0..3)
        .map(|n| {
            (
                format!("e{n}"),
                entry_url(n),
                BASELINE_DATE.to_string(),
                BASELINE_DATE.to_string(),
            )
        })
        .collect()
}

fn feed_bytes(entries: &[(String, String, String, String)]) -> Vec<u8> {
    let refs: Vec<(&str, &str, &str, &str)> = entries
        .iter()
        .map(|(id, link, p, u)| (id.as_str(), link.as_str(), p.as_str(), u.as_str()))
        .collect();
    atom_feed(&refs)
}

// ---------------------------------------------------------------------------
// 1. Two runs of an unchanged feed: the gate saves every entry request.
// ---------------------------------------------------------------------------

/// Nothing about the feed changed between the two runs. Run 2 must make
/// exactly one HTTP request in total — the feed document itself, which has
/// no gate of its own (only the per-entry recheck gate is under test) — and
/// zero entry requests. Also pins the plain counters a gate-only run must
/// leave alone: no fetch means no `on_resource` call, so neither a full index
/// nor a metadata-only write can have happened.
#[tokio::test]
async fn two_runs_of_an_unchanged_feed_gate_skips_every_entry() {
    let store_id = "store-1";
    let entries = baseline_entries();
    let feed = feed_bytes(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![
            downloaded(feed.clone(), "\"feed-v1\""),
            downloaded(feed, "\"feed-v1\""),
        ],
    );
    for (_, link, _, _) in &entries {
        script.insert(
            link.clone(),
            vec![downloaded(b"# Entry\n\nBody.".to_vec(), "\"entry-v1\"")],
        );
    }
    let fetcher = Arc::new(CountingFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id);
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
    assert_eq!(first.docs_indexed, 3, "run 1 indexes all three entries");

    let resource_ids: Vec<String> = entries
        .iter()
        .map(|(_, link, _, _)| doc_index.get(link).unwrap().resource_id.clone())
        .collect();
    let last_checked_after_first: Vec<Option<String>> = {
        let mut v = Vec::new();
        for id in &resource_ids {
            let checked = store.last_checked_at(id).await;
            assert!(
                checked.is_some(),
                "run 1 must touch every entry it just indexed"
            );
            v.push(checked);
        }
        v
    };

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(fetcher.call_count(FEED_URL), 2, "one feed request per run");
    for (_, link, _, _) in &entries {
        assert_eq!(
            fetcher.call_count(link),
            1,
            "the entry link must not be fetched again on run 2: {link}"
        );
    }

    assert_eq!(second.docs_seen, 3);
    assert_eq!(second.docs_skipped, 3);
    assert_eq!(second.docs_recheck_deferred, 3);
    assert_eq!(
        second.docs_indexed, 0,
        "a gate-only run performs no fetch, so nothing can have been (re)indexed"
    );
    assert_eq!(
        second.docs_metadata_updated, 0,
        "a gate-only run performs no fetch, so no metadata write can have happened either"
    );

    for (id, before) in resource_ids.iter().zip(last_checked_after_first.iter()) {
        assert_eq!(
            &store.last_checked_at(id).await,
            before,
            "a gate-skip is not a touch: last_checked_at must be unchanged after run 2"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. One entry's claim moves: only that entry is fetched.
// ---------------------------------------------------------------------------

/// The feed bumps one entry's `<updated>` between run 1 and run 2. Gate check
/// (b) fails for exactly that entry (its merged claim no longer reproduces
/// the stored `metadata_hash`), so it alone falls through to a real
/// conditional GET — replaying the validator its own run-1 response
/// captured. The other two entries' claims are untouched, so they gate-skip.
#[tokio::test]
async fn one_entrys_updated_bump_fetches_only_that_entry() {
    let store_id = "store-1";
    let mut entries = baseline_entries();
    let bumped_link = entries[1].1.clone();
    let feed_run1 = feed_bytes(&entries);
    entries[1].3 = "2026-02-09T00:00:00Z".to_string(); // bump entry 1's <updated> only
    let feed_run2 = feed_bytes(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![
            downloaded(feed_run1, "\"feed-v1\""),
            downloaded(feed_run2, "\"feed-v2\""),
        ],
    );
    for (n, (_, link, _, _)) in entries.iter().enumerate() {
        script.insert(
            link.clone(),
            vec![downloaded(
                format!("# Entry {n}\n\nBody.").into_bytes(),
                &format!("\"entry-{n}-v1\""),
            )],
        );
    }
    let fetcher = Arc::new(CountingFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id);
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

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    for (n, (_, link, _, _)) in entries.iter().enumerate() {
        let expected = if n == 1 { 2 } else { 1 };
        assert_eq!(
            fetcher.call_count(link),
            expected,
            "entry {n} ({link}) fetch count"
        );
    }
    assert_eq!(
        second.docs_recheck_deferred, 2,
        "the two untouched entries gate-skip"
    );

    // The one entry that did fetch replayed exactly the validator its own
    // run-1 response captured — proof this is a real conditional GET, not a
    // fresh unconditional fetch that happens to be scripted the same way.
    let received = fetcher.received_for(&bumped_link);
    assert_eq!(received.len(), 2);
    assert_eq!(
        received[1],
        FetchMetadata {
            etag: Some("\"entry-1-v1\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
        "the gate-reopened entry must replay the validator run 1 captured for it"
    );
}

// ---------------------------------------------------------------------------
// 3. `refetch: true` bypasses the gate for every entry.
// ---------------------------------------------------------------------------

/// A feed fetcher that answers a bare 304 whenever the request carries any
/// validator, and a full 200 only when it carries none — standing in for a
/// real origin honoring `If-None-Match`. Used only for the feed document URL
/// in the `refetch` test: it is what makes "the feed document would 304"
/// true in the absence of `--refetch`, so the test can show that
/// `--refetch`'s suppression of `document_validators`
/// (`core::ingestion::run_source_ingestion`) is what avoids that 304 and
/// lets the entry loop run at all — on top of `refetch` also bypassing the
/// per-entry gate's floor check (c) for every entry the loop then reaches.
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
            correlation_id: "recheck_gate_refetch_test_no_body".to_string(),
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

#[tokio::test]
async fn refetch_true_fetches_every_entry_despite_the_feed_document_would_be_a_304() {
    let store_id = "store-1";
    let entries = baseline_entries();
    let feed = feed_bytes(&entries);
    let mut entry_bodies = HashMap::new();
    for (_, link, _, _) in &entries {
        entry_bodies.insert(link.clone(), b"# Entry\n\nBody.".to_vec());
    }
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
    let source = feed_source(store_id);
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

    // Run 2 carries `document_validators` as if a prior run had persisted
    // the feed's captured etag (`job_exec`'s job in production) — the exact
    // condition under which `FeedConditionalFetcher` would 304 the feed
    // document absent `--refetch`, dead-ending the entry loop before it ever
    // ran, however due each entry was.
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
        "--refetch must suppress the stored feed-document validators, or the \
         request above would have carried them and gotten a 304"
    );
    for (_, link, _, _) in &entries {
        assert_eq!(
            *fetcher.calls.lock().unwrap().get(link).unwrap_or(&0),
            2,
            "every entry must be fetched on run 2 despite an unchanged, in-floor claim: {link}"
        );
    }
    assert_eq!(
        second.docs_recheck_deferred, 0,
        "--refetch bypasses the gate for every entry, so nothing is deferred"
    );
    assert_eq!(second.docs_seen, 3);
}

// ---------------------------------------------------------------------------
// 4. A fetch error leaves the gate open; other entries still gate-skip.
// ---------------------------------------------------------------------------

/// One entry's link errors on run 2. Its feed-supplied claim was bumped
/// starting run 2 (and stays bumped through run 3) so the gate lets the
/// fetch through in the first place (check (b) fails against the still-run-1
/// persisted claim) — the point under test is what happens *after* the
/// error: since `SkipReason::Error` never touches `last_checked_at` or the
/// cached `metadata_hash`, the persisted claim is still run 1's on run 3, so
/// the bumped feed claim keeps failing check (b) and the gate stays open
/// until a fetch actually succeeds. The other two entries' claims never
/// move, so they gate-skip on both run 2 and run 3.
#[tokio::test]
async fn a_fetch_error_reopens_the_gate_for_the_next_run() {
    let store_id = "store-1";
    let mut entries = baseline_entries();
    let error_link = entries[2].1.clone();
    let feed_run1 = feed_bytes(&entries);
    entries[2].3 = "2026-02-09T00:00:00Z".to_string(); // bump entry 2's <updated> from run 2 on
    let feed_run2_and_3 = feed_bytes(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![
            downloaded(feed_run1, "\"feed-v1\""),
            downloaded(feed_run2_and_3.clone(), "\"feed-v2\""),
            downloaded(feed_run2_and_3, "\"feed-v2\""),
        ],
    );
    for (n, (_, link, _, _)) in entries.iter().enumerate() {
        let mut outcomes = vec![downloaded(
            format!("# Entry {n}\n\nBody.").into_bytes(),
            &format!("\"entry-{n}-v1\""),
        )];
        if link == &error_link {
            outcomes.push(Outcome::Error);
            outcomes.push(downloaded(
                format!("# Entry {n}\n\nBody.").into_bytes(),
                &format!("\"entry-{n}-v2\""),
            ));
        }
        script.insert(link.clone(), outcomes);
    }
    let fetcher = Arc::new(CountingFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id);
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

    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();
    assert_eq!(
        second.error_count, 1,
        "the errored entry counts as an error"
    );
    assert_eq!(
        second.docs_recheck_deferred, 2,
        "the two entries whose claim never moved still gate-skip on run 2"
    );
    assert_eq!(
        fetcher.call_count(&error_link),
        2,
        "run 2 must have fetched the erroring entry"
    );

    let third = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();
    assert_eq!(
        fetcher.call_count(&error_link),
        3,
        "run 3 must fetch the errored entry again: the error never touched \
         last_checked_at or the cached metadata_hash, so the still-bumped \
         claim keeps failing the gate's claim check"
    );
    assert_eq!(
        third.docs_recheck_deferred, 2,
        "the two untouched entries gate-skip again on run 3"
    );
    assert_eq!(third.error_count, 0, "run 3's fetch succeeds this time");

    for (n, (_, link, _, _)) in entries.iter().enumerate() {
        if link != &error_link {
            assert_eq!(
                fetcher.call_count(link),
                1,
                "entry {n} ({link}) must never be fetched past run 1"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Withheld feed-document validators: the feed always fetches in full,
//    but the per-entry gate saves the entry requests regardless.
// ---------------------------------------------------------------------------

/// Simulates `server/src/job_exec.rs`'s withholding rule (a feed with
/// `error_count > 0` never gets its validators persisted, so every run's
/// `document_validators` is empty) by passing `FetchMetadata::default()`
/// explicitly rather than relying on `for_test`'s default. Before T329, a
/// feed in this state had no short-circuit at all — its whole entry loop ran
/// on every index, forever. This is the regression the per-entry gate is
/// for: it is a source-visible property that the feed document itself can
/// never 304, not a workaround for it.
#[tokio::test]
async fn withheld_feed_document_validators_still_gate_skip_unchanged_entries() {
    let store_id = "store-1";
    let entries = baseline_entries();
    let feed = feed_bytes(&entries);

    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![
            downloaded(feed.clone(), "\"feed-v1\""),
            downloaded(feed, "\"feed-v1\""),
        ],
    );
    for (_, link, _, _) in &entries {
        script.insert(
            link.clone(),
            vec![downloaded(b"# Entry\n\nBody.".to_vec(), "\"entry-v1\"")],
        );
    }
    let fetcher = Arc::new(CountingFetcher::new(script));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = feed_source(store_id);
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = make_config(store_id);
    let mut doc_index = DocumentIndex::new();

    for _ in 0..2 {
        run_source_ingestion(
            &source,
            &ingestor,
            SourceIngestionDeps {
                // Explicit, not `for_test`'s implied default: this is the
                // withheld-validators condition itself, not an incidental
                // property of the test harness.
                document_validators: FetchMetadata::default(),
                ..SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config)
            },
        )
        .await
        .unwrap();
    }

    assert_eq!(
        fetcher.call_count(FEED_URL),
        2,
        "withheld validators mean the feed document is fetched in full every run"
    );
    for (_, link, _, _) in &entries {
        assert_eq!(
            fetcher.call_count(link),
            1,
            "the per-entry gate must still save this entry's request on run 2: {link}"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. `url` sources are never gated.
// ---------------------------------------------------------------------------

/// `IngestCallback::recheck_is_due` is asked only by `FeedIngestor::
/// process_discovery_entry`; `UrlIngestor` never calls it. Two runs against
/// an unchanged `url` source must therefore fetch twice — the second a
/// conditional GET replaying the first run's captured validator — never a
/// gate-skip.
#[tokio::test]
async fn url_source_is_never_gated() {
    let store_id = "store-1";
    let url = "https://example.com/doc";
    let mut script = HashMap::new();
    script.insert(
        url.to_string(),
        vec![downloaded(b"# Doc\n\nBody text.".to_vec(), "\"doc-v1\"")],
    );
    let fetcher = Arc::new(CountingFetcher::new(script));

    let ingestor = UrlIngestor::new(Box::new(PlainParser), Box::new(ArcFetcher(fetcher.clone())));
    let source = Source {
        id: "source-1".to_string(),
        store_id: store_id.to_string(),
        kind: SourceKind::Url,
        spec: SourceSpec::Url {
            url: url.to_string(),
            refresh_interval_secs: None,
        },
        source_preset: "prose".to_string(),
    };
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
    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(
        fetcher.call_count(url),
        2,
        "a url source must fetch on every run"
    );
    let received = fetcher.received_for(url);
    assert_eq!(
        received[1],
        FetchMetadata {
            etag: Some("\"doc-v1\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
        "run 2 must still replay run 1's captured validator — ungated, not unconditional"
    );
}
