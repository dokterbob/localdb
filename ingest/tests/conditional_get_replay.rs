//! E2E proof that conditional-GET validators round-trip: captured on a 200,
//! replayed on the next fetch of the same URI — for both a `url` source and
//! a feed entry link (specs/04-search-pipeline.md §1).
//!
//! Each test drives the real `core::ingestion::run_source_ingestion` +
//! `PipelineCallback` machinery (not a bespoke test callback) twice, sharing
//! one `DocumentIndex` across both runs — exactly the seam
//! `IngestCallback::lookup_fetch_metadata` reads from. A test double that
//! only recorded outcomes could not distinguish "replay works" from "replay
//! is silently skipped and every run just happens to look the same," so
//! `ScriptedFetcher` records the actual `FetchMetadata` each call received.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ingest::{FeedIngestor, UrlIngestor};
use localdb_core::embedder::FakeEmbedder;
use localdb_core::error::Error;
use localdb_core::ingestion::{
    run_source_ingestion, DeletionPolicy, DocumentIndex, FetchMetadata, FetchResult,
    IngestionConfig, SourceIngestionDeps, UrlFetcher,
};
use localdb_core::parser::{ParsedDocument, Parser, Probe};
use localdb_core::store::FakeStore;
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::ChunkerConfig;

/// Pass-through parser: treats every input as UTF-8 Markdown, no title, no
/// metadata. Mirrors `url_ingestor::tests::AllParser`.
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

/// A fetcher scripted to always return the same `Downloaded` body for each
/// URL, while recording the `FetchMetadata` every call actually received —
/// the thing this stage's replay logic is meant to change.
#[derive(Default)]
struct ScriptedFetcher {
    bodies: HashMap<String, Vec<u8>>,
    received: Mutex<Vec<(String, FetchMetadata)>>,
}

impl ScriptedFetcher {
    fn new(bodies: HashMap<String, Vec<u8>>) -> Self {
        Self {
            bodies,
            received: Mutex::new(Vec::new()),
        }
    }

    /// The `FetchMetadata` values received, in call order, for `url`.
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
impl UrlFetcher for ScriptedFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.received
            .lock()
            .unwrap()
            .push((url.to_string(), meta.clone()));
        match self.bodies.get(url) {
            Some(bytes) => Ok(FetchResult::Downloaded {
                bytes: bytes.clone(),
                content_type: Some("text/markdown".to_string()),
                etag: Some("\"v1\"".to_string()),
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
                final_url: None,
            }),
            None => Err(Error::Internal {
                message: format!("no scripted body for {url}"),
                correlation_id: "test_no_body".to_string(),
            }),
        }
    }
}

/// Shares one `ScriptedFetcher` across a `Box<dyn UrlFetcher>` slot (owned
/// by the ingestor under test) and a handle this test keeps for itself to
/// inspect afterward — mirrors `feed_ingestor::tests`'s own `ArcFetcher`.
struct ArcFetcher(Arc<ScriptedFetcher>);
#[async_trait]
impl UrlFetcher for ArcFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.0.fetch(url, meta).await
    }
}

fn make_config(store_id: &str) -> IngestionConfig {
    IngestionConfig {
        store_id: store_id.to_string(),
        policy_version: "policy-v1".to_string(),
        chunker: ChunkerConfig::prose(),
    }
}

/// Two `run_source_ingestion` calls sharing one `DocumentIndex`, over a
/// `url` source: the first call's fetch has nothing to replay, the second's
/// must replay exactly what the first captured.
#[tokio::test]
async fn url_source_replays_stored_validators_on_second_run() {
    let store_id = "store-1";
    let url = "https://example.com/doc";
    let mut bodies = HashMap::new();
    bodies.insert(url.to_string(), b"# Doc\n\nBody text.".to_vec());
    let fetcher = Arc::new(ScriptedFetcher::new(bodies));

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
        SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Retain,
        },
    )
    .await
    .unwrap();

    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Retain,
        },
    )
    .await
    .unwrap();

    let received = fetcher.received_for(url);
    assert_eq!(received.len(), 2, "one fetch per run");
    assert_eq!(
        received[0],
        FetchMetadata::default(),
        "the first run has nothing stored yet to replay"
    );
    assert_eq!(
        received[1],
        FetchMetadata {
            etag: Some("\"v1\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
        "the second run must replay exactly what the first run's response captured"
    );
}

/// Same proof, for a feed **entry link** — the feed document's own fetch is
/// deliberately excluded from replay in this stage (Stage 5), so this
/// asserts specifically on the entry link URL's received metadata, not the
/// feed document URL's.
#[tokio::test]
async fn feed_entry_link_replays_stored_validators_on_second_run() {
    let store_id = "store-1";
    let feed_url = "https://feed.example.com/feed.xml";
    let entry_url = "https://feed.example.com/entry-1";
    let feed_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Test Feed</title><link>{feed_url}</link><description>d</description><item><title>E1</title><link>{entry_url}</link><guid>{entry_url}</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Summary</description></item></channel></rss>"#
    );

    let mut bodies = HashMap::new();
    bodies.insert(feed_url.to_string(), feed_xml.into_bytes());
    bodies.insert(
        entry_url.to_string(),
        b"# Entry\n\nFull entry body text.".to_vec(),
    );
    let fetcher = Arc::new(ScriptedFetcher::new(bodies));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    let source = Source {
        id: "source-1".to_string(),
        store_id: store_id.to_string(),
        kind: SourceKind::Feed,
        spec: SourceSpec::Feed {
            url: feed_url.to_string(),
            max_entries: None,
            fetch_full_content: true,
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
        SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Retain,
        },
    )
    .await
    .unwrap();

    run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps {
            doc_index: &mut doc_index,
            store: &store,
            embedder: &embedder,
            config: &config,
            progress: None,
            deletion: DeletionPolicy::Retain,
        },
    )
    .await
    .unwrap();

    let received = fetcher.received_for(entry_url);
    assert_eq!(received.len(), 2, "one entry-link fetch per run");
    assert_eq!(
        received[0],
        FetchMetadata::default(),
        "the first run has nothing stored yet to replay for the entry link"
    );
    assert_eq!(
        received[1],
        FetchMetadata {
            etag: Some("\"v1\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
        "the second run must replay exactly what the first run's entry-link fetch captured"
    );

    // The feed document's own fetch is unconditional in this stage (its
    // conditional GET is Stage 5) — every call replays nothing.
    let feed_received = fetcher.received_for(feed_url);
    assert_eq!(feed_received.len(), 2);
    assert!(
        feed_received.iter().all(|m| *m == FetchMetadata::default()),
        "the feed document's own fetch must not replay a validator in this stage"
    );
}
