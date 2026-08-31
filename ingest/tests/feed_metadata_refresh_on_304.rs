//! A 304 on a feed entry's link proves the *page* is unchanged; it proves
//! nothing about the feed's own description of that entry. These tests drive
//! the real `run_source_ingestion` + `PipelineCallback` machinery twice and
//! assert on `FakeStore`'s `update_resource_metadata` log — the only place
//! the distinction is observable (specs/04-search-pipeline.md §1).
//!
//! Counting writes, not just checking values, is the point: the fix is only
//! correct if it writes when the feed changed and stays completely silent
//! when it did not. An assertion that merely read the stored metadata back
//! would pass just as happily against an implementation that rewrote the
//! resource row on every 304 forever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ingest::FeedIngestor;
use localdb_core::embedder::FakeEmbedder;
use localdb_core::error::Error;
use localdb_core::ids::compute_metadata_hash;
use localdb_core::ingestion::{
    run_source_ingestion, DocumentIndex, FetchMetadata, FetchResult, IngestionConfig,
    SourceIngestionDeps, UrlFetcher,
};
use localdb_core::metadata::DublinCoreMetadata;
use localdb_core::parser::{ParsedDocument, Parser, Probe};
use localdb_core::store::{FakeStore, RetrievalStore};
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::ChunkerConfig;

const FEED_URL: &str = "https://feed.example.com/feed.xml";
const ENTRY_URL: &str = "https://feed.example.com/entry-1";

/// Pass-through parser: every input is UTF-8 Markdown, no title, no
/// metadata. Deliberately title-less so the entry's stored Dublin Core state
/// comes purely from the feed — the state these tests are about.
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

/// A fetcher driven by a per-URL script of responses, consumed in order;
/// the last entry repeats once the script runs dry.
struct ScriptedFetcher {
    script: Mutex<HashMap<String, Vec<FetchResult>>>,
    calls: Mutex<HashMap<String, usize>>,
}

impl ScriptedFetcher {
    fn new(script: HashMap<String, Vec<FetchResult>>) -> Self {
        Self {
            script: Mutex::new(script),
            calls: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl UrlFetcher for ScriptedFetcher {
    async fn fetch(&self, url: &str, _meta: &FetchMetadata) -> Result<FetchResult, Error> {
        let mut calls = self.calls.lock().unwrap();
        let n = calls.entry(url.to_string()).or_insert(0);
        let idx = *n;
        *n += 1;
        let script = self.script.lock().unwrap();
        let responses = script.get(url).ok_or_else(|| Error::Internal {
            message: format!("no scripted response for {url}"),
            correlation_id: "test_no_script".to_string(),
        })?;
        Ok(clone_result(
            &responses[idx.min(responses.len().saturating_sub(1))],
        ))
    }
}

/// `FetchResult` is deliberately not `Clone` in `core` (a `Downloaded` body
/// is not something production code should copy casually), so the script
/// replays responses through an explicit copy here.
fn clone_result(r: &FetchResult) -> FetchResult {
    match r {
        FetchResult::Downloaded {
            bytes,
            content_type,
            etag,
            last_modified,
            final_url,
        } => FetchResult::Downloaded {
            bytes: bytes.clone(),
            content_type: content_type.clone(),
            etag: etag.clone(),
            last_modified: last_modified.clone(),
            final_url: final_url.clone(),
        },
        FetchResult::NotModified {
            etag,
            last_modified,
        } => FetchResult::NotModified {
            etag: etag.clone(),
            last_modified: last_modified.clone(),
        },
        FetchResult::Gone => FetchResult::Gone,
        FetchResult::Blocked => FetchResult::Blocked,
    }
}

/// Shares one `ScriptedFetcher` between the two `Box<dyn UrlFetcher>` slots
/// `FeedIngestor` owns (feed document and entry links).
struct ArcFetcher(Arc<ScriptedFetcher>);
#[async_trait]
impl UrlFetcher for ArcFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.0.fetch(url, meta).await
    }
}

fn downloaded(bytes: Vec<u8>) -> FetchResult {
    FetchResult::Downloaded {
        bytes,
        content_type: Some("text/markdown".to_string()),
        etag: Some("\"v1\"".to_string()),
        last_modified: None,
        final_url: None,
    }
}

/// A bare 304 — no validators of its own, so it cannot route through
/// `on_validators_refreshed` and every write these tests observe is
/// attributable to the metadata seam alone.
fn not_modified() -> FetchResult {
    FetchResult::NotModified {
        etag: None,
        last_modified: None,
    }
}

fn atom_feed(entry_title: &str, author: &str, published: &str, updated: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Test Feed</title>
  <id>urn:feed</id>
  <updated>{updated}</updated>
  <link href="{FEED_URL}" rel="self"/>
  <entry>
    <title>{entry_title}</title>
    <id>urn:e1</id>
    <published>{published}</published>
    <updated>{updated}</updated>
    <author><name>{author}</name></author>
    <link href="{ENTRY_URL}" rel="alternate"/>
    <summary>Entry summary</summary>
  </entry>
</feed>"#
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

/// Two runs against one shared `DocumentIndex`: run 1 indexes the entry from
/// `first_feed` with a real 200 on its link, run 2 re-reads `second_feed`
/// while the link answers a bare 304.
async fn run_two_feeds(first_feed: Vec<u8>, second_feed: Vec<u8>) -> (FakeStore, DocumentIndex) {
    let store_id = "store-1";
    let mut script = HashMap::new();
    script.insert(
        FEED_URL.to_string(),
        vec![downloaded(first_feed), downloaded(second_feed)],
    );
    script.insert(
        ENTRY_URL.to_string(),
        vec![
            downloaded(b"# Entry\n\nFull entry body text.".to_vec()),
            not_modified(),
        ],
    );
    let fetcher = Arc::new(ScriptedFetcher::new(script));

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
            SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
        )
        .await
        .unwrap();
    }

    (store, doc_index)
}

/// The common case: nothing about the entry changed and its link 304'd, so
/// the run must perform no resource write at all. A blind write here would
/// bump `index_updated_at` — publicly visible as
/// `DocumentInfo.index_updated_at` — on every run of every unchanged feed.
#[tokio::test]
async fn unchanged_feed_metadata_and_a_304_writes_nothing() {
    let feed = atom_feed(
        "Entry One",
        "Jane Doe",
        "2026-01-05T00:00:00Z",
        "2026-01-05T00:00:00Z",
    );
    let (store, _doc_index) = run_two_feeds(feed.clone(), feed).await;

    assert!(
        store.metadata_updates().await.is_empty(),
        "an unchanged entry behind a 304 must not write the resource row: {:?}",
        store.metadata_updates().await
    );
}

/// The regression this seam exists for: the feed corrects an entry's author
/// and publication date while the linked page is unchanged. Before the seam,
/// `process_url` returned from its `NotModified` arm before ever building a
/// `Resource`, so the correction could never land — and would stay unlanded
/// for as long as the page itself did not change.
#[tokio::test]
async fn changed_feed_metadata_behind_a_304_updates_the_resource_once() {
    let (store, doc_index) = run_two_feeds(
        atom_feed(
            "Entry One",
            "Jane Doe",
            "2026-01-05T00:00:00Z",
            "2026-01-05T00:00:00Z",
        ),
        atom_feed(
            "Entry One",
            "Jane Q. Doe",
            "2026-02-09T00:00:00Z",
            "2026-02-10T00:00:00Z",
        ),
    )
    .await;

    let updates = store.metadata_updates().await;
    assert_eq!(
        updates.len(),
        1,
        "exactly one metadata-only write, on the run whose feed changed: {updates:?}"
    );
    let (resource_id, record) = &updates[0];

    let dc = record.metadata.dublin_core();
    assert_eq!(dc.creator, vec!["Jane Q. Doe".to_string()]);
    assert_eq!(dc.date.as_deref(), Some("2026-02-09T00:00:00Z"));
    assert_eq!(
        dc.date_source.as_deref(),
        Some("feed-entry"),
        "the feed's date must carry the feed's own provenance, never the page parser's"
    );
    assert_eq!(
        record.modified_at.as_deref(),
        Some("2026-02-10T00:00:00Z"),
        "modified_at takes the entry's `updated`, the opposite preference to dc.date"
    );
    assert_eq!(
        record.date_original.as_deref(),
        Some("2026-02-09T00:00:00Z"),
        "date_original is a projection of the merged dc.date, re-derived not carried over"
    );
    assert_eq!(
        record.date_parsed.as_deref(),
        Some("2026-02-09"),
        "date_parsed is date_original through `parse_partial_iso8601`, which keeps \
         only the date prefix — same derivation the full-index path applies"
    );

    // The chunks themselves were never rewritten: this is a metadata-only
    // path, and a 304 has no body to re-chunk from in the first place.
    let chunks = store.get_chunks_for_resource(resource_id).await.unwrap();
    assert!(!chunks.is_empty());
    let text: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert!(
        text.contains("Full entry body text"),
        "content must still be the page body captured on the 200, not the feed \
         summary a re-index from the entry alone would have produced: {text}"
    );

    // The cached hash must equal what a rehydration from the persisted row
    // computes. A mismatch is the exact desync `on_validators_refreshed`
    // guards against, one field over: the next run would read a spurious
    // metadata_hash difference and rewrite the row for nothing.
    let cached = doc_index
        .get(ENTRY_URL)
        .expect("the entry stays indexed across both runs");
    assert_eq!(
        cached.metadata_hash,
        compute_metadata_hash(
            &chunks[0].metadata,
            chunks[0].external_id.as_deref(),
            chunks[0].external_etag.as_deref(),
            chunks[0].modified_at.as_deref(),
        ),
        "the cached metadata_hash must match what the persisted state rehydrates to"
    );
}

/// A connector title only ever fills a gap — the same rule the index-time
/// merge applies, evaluated here against persisted state. The entry already
/// has a title, so renaming it in the feed changes nothing behind a 304.
///
/// Deliberate: on a real re-fetch the linked page's own title would win
/// again anyway. The divergence is confined to a page that supplies no title
/// at all, where the stored title is the connector's previous one and no
/// longer reads as a gap — erring toward keeping extracted state.
#[tokio::test]
async fn a_renamed_entry_alone_does_not_overwrite_a_stored_title() {
    let (store, _doc_index) = run_two_feeds(
        atom_feed(
            "Entry One",
            "Jane Doe",
            "2026-01-05T00:00:00Z",
            "2026-01-05T00:00:00Z",
        ),
        atom_feed(
            "Entry One, Revised",
            "Jane Doe",
            "2026-01-05T00:00:00Z",
            "2026-01-05T00:00:00Z",
        ),
    )
    .await;

    assert!(
        store.metadata_updates().await.is_empty(),
        "a title-only feed change must not rewrite the resource row: {:?}",
        store.metadata_updates().await
    );
}
