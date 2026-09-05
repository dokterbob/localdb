//! A feed's stored conditional-GET validators must not be replayed once our
//! own inputs have moved (specs/02-domain-model.md, Feed connector,
//! "Conditional GET and pruning").
//!
//! RFC 9110 binds the origin: it rotates its validator when *its*
//! representation changes. It has no way to know our indexing policy
//! changed, or that we stopped following entry links, or that we narrowed
//! `max_entries`. Replaying validators across such a change yields a 304,
//! the entry loop never runs, and not one entry is reprocessed under the new
//! inputs — indefinitely, for as long as the feed XML itself holds still.
//!
//! These tests assert on the `FetchMetadata` the feed document's own fetch
//! actually received, which is the only place the suppression is visible.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ingest::FeedIngestor;
use localdb_core::embedder::FakeEmbedder;
use localdb_core::error::Error;
use localdb_core::ids::compute_feed_inputs_digest;
use localdb_core::ingestion::{
    run_source_ingestion, DeletionPolicy, DocumentIndex, FetchMetadata, FetchResult,
    IngestionConfig, IngestionResult, SourceIngestionDeps, UnreachableFetcher, UrlFetcher,
};
use localdb_core::metadata::DublinCoreMetadata;
use localdb_core::parser::{ParsedDocument, Parser, Probe};
use localdb_core::store::FakeStore;
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::ChunkerConfig;

const FEED_URL: &str = "https://feed.example.com/feed.xml";
const STORED_ETAG: &str = "\"stored-v1\"";

struct PlainParser;
impl Parser for PlainParser {
    fn id(&self) -> &'static str {
        "plain"
    }
    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        Ok(Some(ParsedDocument {
            markdown: String::from_utf8_lossy(probe.bytes()).to_string(),
            title: None,
            metadata: DublinCoreMetadata::default(),
            page_starts: Vec::new(),
        }))
    }
}

/// Always answers 200 with the same body, recording the `FetchMetadata` each
/// call received — the thing the gate changes.
struct RecordingFetcher {
    bodies: HashMap<String, Vec<u8>>,
    received: Mutex<Vec<(String, FetchMetadata)>>,
}

impl RecordingFetcher {
    fn received_for(&self, url: &str) -> Vec<FetchMetadata> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _)| u == url)
            .map(|(_, m)| m.clone())
            .collect()
    }
}

#[async_trait]
impl UrlFetcher for RecordingFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.received
            .lock()
            .unwrap()
            .push((url.to_string(), meta.clone()));
        match self.bodies.get(url) {
            Some(bytes) => Ok(FetchResult::Downloaded {
                bytes: bytes.clone(),
                content_type: Some("application/atom+xml".to_string()),
                etag: Some("\"fresh-v2\"".to_string()),
                last_modified: None,
                final_url: None,
            }),
            None => Err(Error::Internal {
                message: format!("no scripted body for {url}"),
                correlation_id: "test_no_body".to_string(),
            }),
        }
    }
}

struct ArcFetcher(Arc<RecordingFetcher>);
#[async_trait]
impl UrlFetcher for ArcFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.0.fetch(url, meta).await
    }
}

fn atom_feed(entry_urls: &[&str]) -> Vec<u8> {
    let entries: String = entry_urls
        .iter()
        .enumerate()
        .map(|(i, u)| {
            format!(
                r#"<entry><title>E{i}</title><id>urn:e{i}</id>
                   <updated>2026-01-05T00:00:00Z</updated>
                   <published>2026-01-05T00:00:00Z</published>
                   <link href="{u}" rel="alternate"/>
                   <summary>Summary {i}</summary></entry>"#
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Test Feed</title><id>urn:feed</id>
  <updated>2026-01-05T00:00:00Z</updated>
  <link href="{FEED_URL}" rel="self"/>
  {entries}
</feed>"#
    )
    .into_bytes()
}

/// One `run_source_ingestion` against a feed whose stored validators are
/// `STORED_ETAG` and whose stored inputs digest is `stored_digest`. Returns
/// the metadata the feed document's own fetch received, plus the run's
/// result.
async fn run_once(
    max_entries: Option<u32>,
    fetch_full_content: bool,
    policy_version: &str,
    stored_digest: Option<String>,
    entry_urls: &[&str],
    refetch: bool,
) -> (FetchMetadata, IngestionResult) {
    let mut bodies = HashMap::new();
    bodies.insert(FEED_URL.to_string(), atom_feed(entry_urls));
    for u in entry_urls {
        bodies.insert((*u).to_string(), b"# Entry\n\nBody.".to_vec());
    }
    let fetcher = Arc::new(RecordingFetcher {
        bodies,
        received: Mutex::new(Vec::new()),
    });

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = Source {
        id: "source-1".to_string(),
        store_id: "store-1".to_string(),
        kind: SourceKind::Feed,
        spec: SourceSpec::Feed {
            url: FEED_URL.to_string(),
            max_entries,
            fetch_full_content,
            refresh_interval_secs: None,
        },
        source_preset: "prose".to_string(),
    };
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let config = IngestionConfig {
        store_id: "store-1".to_string(),
        policy_version: policy_version.to_string(),
        chunker: ChunkerConfig::prose(),
    };
    let mut doc_index = DocumentIndex::new();

    let result = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Retain,
            // Non-empty on purpose: every suppression assertion below is
            // only meaningful because there was something to replay.
            document_validators: FetchMetadata {
                etag: Some(STORED_ETAG.to_string()),
                last_modified: None,
            },
            stored_inputs_digest: stored_digest,
            refetch,
            // Retaining run: the liveness sweep never fires, so this must
            // never be reached.
            fetcher: &UnreachableFetcher,
        },
    )
    .await
    .unwrap();

    let received = fetcher.received_for(FEED_URL);
    assert_eq!(received.len(), 1, "exactly one feed-document fetch per run");
    (received.into_iter().next().unwrap(), result)
}

/// The digest for the baseline inputs every test below varies one field of.
fn baseline_digest() -> String {
    compute_feed_inputs_digest("policy-v1", true, Some(10))
}

async fn run_baseline_with(stored_digest: Option<String>) -> FetchMetadata {
    run_once(Some(10), true, "policy-v1", stored_digest, &[], false)
        .await
        .0
}

#[tokio::test]
async fn matching_inputs_replay_the_stored_validators() {
    let received = run_baseline_with(Some(baseline_digest())).await;
    assert_eq!(
        received.etag.as_deref(),
        Some(STORED_ETAG),
        "unchanged inputs are the whole point of the cache — replay must happen"
    );
}

#[tokio::test]
async fn a_changed_policy_version_suppresses_the_replay() {
    // Same stored digest, different policy: the digest no longer describes
    // this run's inputs.
    let received = run_once(
        Some(10),
        true,
        "policy-v2",
        Some(baseline_digest()),
        &[],
        false,
    )
    .await
    .0;
    assert_eq!(
        received,
        FetchMetadata::default(),
        "a policy change must force an unconditional fetch: a 304 would skip \
         the entry loop and no entry would ever be reprocessed"
    );
}

#[tokio::test]
async fn a_changed_fetch_full_content_suppresses_the_replay() {
    let received = run_once(
        Some(10),
        false,
        "policy-v1",
        Some(baseline_digest()),
        &[],
        false,
    )
    .await
    .0;
    assert_eq!(received, FetchMetadata::default());
}

#[tokio::test]
async fn a_changed_max_entries_suppresses_the_replay() {
    let received = run_once(
        Some(5),
        true,
        "policy-v1",
        Some(baseline_digest()),
        &[],
        false,
    )
    .await
    .0;
    assert_eq!(received, FetchMetadata::default());
}

/// `None` is every row written before the column existed. Such a validator
/// carries no evidence about which inputs produced it, so it is not trusted
/// — the store refetches once after an upgrade rather than risking a
/// permanently stranded feed.
#[tokio::test]
async fn an_absent_stored_digest_suppresses_the_replay() {
    let received = run_baseline_with(None).await;
    assert_eq!(received, FetchMetadata::default());
}

/// The digest the run reports back for persistence must be the one computed
/// from *this* run's inputs, not the stale one it compared against —
/// otherwise the mismatch would repeat forever and the cache would never
/// come back into use.
#[tokio::test]
async fn the_run_reports_the_current_inputs_digest_for_persistence() {
    let (_received, result) = run_once(
        Some(5),
        true,
        "policy-v1",
        Some(baseline_digest()),
        &[],
        false,
    )
    .await;
    assert_eq!(
        result.document_inputs_digest.as_deref(),
        Some(compute_feed_inputs_digest("policy-v1", true, Some(5)).as_str())
    );
}

/// End of the chain, at the behavior that actually matters: narrowing
/// `max_entries` against an unchanged feed must still run the entry loop.
/// The unit assertions above pin the suppressed header; this pins that the
/// suppression buys what it was meant to buy.
#[tokio::test]
async fn a_narrowed_max_entries_still_processes_entries() {
    let entries = ["https://feed.example.com/e0", "https://feed.example.com/e1"];
    let (received, result) = run_once(
        Some(1),
        true,
        "policy-v1",
        Some(baseline_digest()),
        &entries,
        false,
    )
    .await;

    assert_eq!(received, FetchMetadata::default());
    assert!(
        result.docs_seen > 0,
        "the entry loop must have run: a replayed validator would have 304'd \
         and reported nothing at all"
    );
}

/// `--refetch` takes the exact same suppression path as a digest mismatch,
/// even though the inputs digest matches perfectly here — mirrors
/// `a_changed_policy_version_suppresses_the_replay` above, with `refetch`
/// varied instead of `policy_version`. Bypassing only the recheck gate
/// (`core::ingestion::pipeline::callback::PipelineCallback::recheck_is_due`)
/// would still dead-end at this feed document's own 304 before the entry
/// loop ever ran — this is the other half of the escape hatch.
#[tokio::test]
async fn refetch_suppresses_the_replay_even_with_matching_inputs() {
    let received = run_once(
        Some(10),
        true,
        "policy-v1",
        Some(baseline_digest()),
        &[],
        true,
    )
    .await
    .0;
    assert_eq!(
        received,
        FetchMetadata::default(),
        "--refetch must force an unconditional fetch of the feed document \
         even when the inputs digest still matches"
    );
}

/// The other half of the same behavior, pinned the way
/// `a_narrowed_max_entries_still_processes_entries` pins the digest-mismatch
/// case: `--refetch` must not merely drop the header, it must let the entry
/// loop run.
#[tokio::test]
async fn refetch_still_processes_entries_despite_matching_inputs() {
    let entries = ["https://feed.example.com/e0", "https://feed.example.com/e1"];
    let (received, result) = run_once(
        Some(10),
        true,
        "policy-v1",
        Some(baseline_digest()),
        &entries,
        true,
    )
    .await;

    assert_eq!(received, FetchMetadata::default());
    assert!(
        result.docs_seen > 0,
        "the entry loop must have run: a replayed validator would have 304'd \
         and reported nothing at all"
    );
}
