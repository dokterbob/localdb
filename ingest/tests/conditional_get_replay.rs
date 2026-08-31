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
    run_source_ingestion, DocumentIndex, FetchMetadata, FetchResult, IngestionConfig,
    SourceIngestionDeps, UrlFetcher,
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

/// Same proof, for a feed **entry link**. Entry links replay through
/// `IngestCallback::lookup_fetch_metadata`, off their own `resources` row —
/// a different mechanism from the feed document's, which replays off the
/// `sources` row instead. This asserts specifically on the entry link URL's
/// received metadata, not the feed document URL's.
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

    // The feed document replays off `sources.feed_etag`/`feed_last_modified`,
    // which only `job_exec`'s persistence hop ever writes. This test drives
    // `run_source_ingestion` directly and passes an empty
    // `document_validators` on both runs, so nothing is carried between them
    // — a property of this harness, not of the feed document's own
    // conditional GET, which is covered end to end in
    // `server/src/job_exec/tests/feed_validator_persistence.rs`.
    let feed_received = fetcher.received_for(feed_url);
    assert_eq!(feed_received.len(), 2);
    assert!(
        feed_received.iter().all(|m| *m == FetchMetadata::default()),
        "this harness stores nothing between runs for the feed document"
    );
}

// ---------------------------------------------------------------------------
// What a 304 that *wrote* reports (specs/04-search-pipeline.md, outcomes)
// ---------------------------------------------------------------------------

/// Returns `Downloaded` for a URL the first time it is asked and
/// `NotModified` — carrying a rotated `ETag` — every time after, per URL.
///
/// The feed document is exempt: it is scripted to keep answering 200 so the
/// entry loop runs on every pass, which is what puts the entry link's 304 on
/// the path under test here.
struct ThenNotModifiedFetcher {
    bodies: Mutex<HashMap<String, Vec<u8>>>,
    always_200: String,
    served: Mutex<HashMap<String, usize>>,
}

impl ThenNotModifiedFetcher {
    fn new(bodies: HashMap<String, Vec<u8>>, always_200: &str) -> Self {
        Self {
            bodies: Mutex::new(bodies),
            always_200: always_200.to_string(),
            served: Mutex::new(HashMap::new()),
        }
    }

    /// Replace one URL's body, standing in for an origin whose content
    /// changed between runs.
    fn set_body(&self, url: &str, bytes: Vec<u8>) {
        self.bodies
            .lock()
            .unwrap()
            .insert(url.to_string(), bytes.clone());
    }
}

#[async_trait]
impl UrlFetcher for ThenNotModifiedFetcher {
    async fn fetch(&self, url: &str, _meta: &FetchMetadata) -> Result<FetchResult, Error> {
        let nth = {
            let mut served = self.served.lock().unwrap();
            let n = served.entry(url.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        let bodies = self.bodies.lock().unwrap();
        let Some(bytes) = bodies.get(url) else {
            return Err(Error::Internal {
                message: format!("no scripted body for {url}"),
                correlation_id: "test_no_body".to_string(),
            });
        };
        if nth > 1 && url != self.always_200 {
            // The rotated validator RFC 9111 requires storing, on a response
            // that carries no body.
            return Ok(FetchResult::NotModified {
                etag: Some("\"v2\"".to_string()),
                last_modified: None,
            });
        }
        Ok(FetchResult::Downloaded {
            bytes: bytes.clone(),
            content_type: Some("text/markdown".to_string()),
            etag: Some("\"v1\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
            final_url: None,
        })
    }
}

struct ArcNotModified(Arc<ThenNotModifiedFetcher>);
#[async_trait]
impl UrlFetcher for ArcNotModified {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.0.fetch(url, meta).await
    }
}

/// Wraps `FakeStore` and can be armed to fail every
/// `update_resource_metadata`, simulating a backend write failure (a dropped
/// connection) rather than a logic bug — every other operation still goes to
/// the real double, so the run reaches the metadata write normally.
struct FailingMetadataStore {
    inner: FakeStore,
    fail: std::sync::atomic::AtomicBool,
}

impl FailingMetadataStore {
    fn new() -> Self {
        Self {
            inner: FakeStore::new(),
            fail: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn start_failing(&self) {
        self.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl localdb_core::store::RetrievalStore for FailingMetadataStore {
    async fn upsert_chunks(&self, records: Vec<localdb_core::ChunkRecord>) -> Result<usize, Error> {
        self.inner.upsert_chunks(records).await
    }

    async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
        self.inner.delete_by_resource(resource_id).await
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        self.inner.delete_by_store(store_id).await
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[localdb_core::store::MetadataFilter],
    ) -> Result<Vec<localdb_core::store::SearchResult>, Error> {
        self.inner.dense_search(query_vector, limit, filters).await
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[localdb_core::store::MetadataFilter],
    ) -> Result<Vec<localdb_core::store::SearchResult>, Error> {
        self.inner.bm25_search(query_text, limit, filters).await
    }

    async fn stats(&self) -> Result<localdb_core::store::StoreStats, Error> {
        self.inner.stats().await
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<localdb_core::ChunkRecord>, Error> {
        self.inner.get_chunk(chunk_id).await
    }

    async fn get_chunks_for_resource(
        &self,
        resource_id: &str,
    ) -> Result<Vec<localdb_core::ChunkRecord>, Error> {
        self.inner.get_chunks_for_resource(resource_id).await
    }

    async fn list_indexed_documents(
        &self,
    ) -> Result<Vec<localdb_core::ingestion::DocumentRecord>, Error> {
        self.inner.list_indexed_documents().await
    }

    async fn update_resource_metadata(
        &self,
        store_id: &str,
        resource_id: &str,
        record: &localdb_core::ResourceRecord,
    ) -> Result<(), Error> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Internal {
                message: "simulated metadata write failure".to_string(),
                correlation_id: "failing_metadata_store".to_string(),
            });
        }
        self.inner
            .update_resource_metadata(store_id, resource_id, record)
            .await
    }

    async fn get_resource_record(
        &self,
        store_id: &str,
        resource_id: &str,
    ) -> Result<Option<localdb_core::ResourceRecord>, Error> {
        self.inner.get_resource_record(store_id, resource_id).await
    }

    async fn upsert_blocks(
        &self,
        store_id: &str,
        resource_id: &str,
        blocks: &[localdb_core::block::Block],
    ) -> Result<(), Error> {
        self.inner
            .upsert_blocks(store_id, resource_id, blocks)
            .await
    }

    async fn get_blocks_for_resource(
        &self,
        resource_id: &str,
    ) -> Result<Vec<localdb_core::block::Block>, Error> {
        self.inner.get_blocks_for_resource(resource_id).await
    }

    async fn upsert_chunks_and_blocks(
        &self,
        store_id: &str,
        resource_id: &str,
        records: Vec<localdb_core::ChunkRecord>,
        blocks: &[localdb_core::block::Block],
        replaces_resource_id: Option<&str>,
        external_last_modified: Option<&str>,
    ) -> Result<usize, Error> {
        self.inner
            .upsert_chunks_and_blocks(
                store_id,
                resource_id,
                records,
                blocks,
                replaces_resource_id,
                external_last_modified,
            )
            .await
    }
}

fn feed_xml_with_author(feed_url: &str, entry_url: &str, author: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Test Feed</title><link>{feed_url}</link><description>d</description><item><title>E1</title><link>{entry_url}</link><guid>{entry_url}</guid><author>{author}</author><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Summary</description></item></channel></rss>"#
    )
}

fn feed_source(store_id: &str, feed_url: &str) -> Source {
    Source {
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
    }
}

/// A 304 that rotated a validator *and* carried moved-on feed metadata wrote
/// to the store twice. Reporting the URI as a plain skip would hide both
/// writes: the run would say it changed nothing while two columns moved.
///
/// The URI is still reported exactly once — `docs_seen` is partitioned by the
/// outcome counters, so landing in `docs_skipped` as well would double-count
/// it (specs/04-search-pipeline.md).
#[tokio::test]
async fn a_304_that_writes_reports_a_metadata_update_not_a_skip() {
    let store_id = "store-1";
    let feed_url = "https://feed.example.com/feed.xml";
    let entry_url = "https://feed.example.com/entry-1";

    let mut bodies = HashMap::new();
    bodies.insert(
        feed_url.to_string(),
        feed_xml_with_author(feed_url, entry_url, "Alice").into_bytes(),
    );
    bodies.insert(
        entry_url.to_string(),
        b"# Entry\n\nFull entry body text.".to_vec(),
    );
    let fetcher = Arc::new(ThenNotModifiedFetcher::new(bodies, feed_url));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcNotModified(fetcher.clone())),
        Box::new(ArcNotModified(fetcher.clone())),
    );
    let source = feed_source(store_id, feed_url);
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
    assert_eq!(first.docs_indexed, 1, "the entry indexes on the first run");

    // Second run: the entry link answers 304 with a rotated ETag, and the
    // feed has meanwhile corrected the entry's byline.
    fetcher.set_body(
        feed_url,
        feed_xml_with_author(feed_url, entry_url, "Bob").into_bytes(),
    );
    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(second.docs_seen, 1);
    assert_eq!(second.docs_indexed, 0, "a 304 never re-indexes");
    assert_eq!(
        second.docs_metadata_updated, 1,
        "both hooks wrote; the run must report the write"
    );
    assert_eq!(
        second.docs_skipped, 0,
        "a URI counted as both a skip and a metadata update double-counts it"
    );
    assert_eq!(second.error_count, 0);
}

/// The other half: a metadata write that *failed* must not report as a clean
/// skip. Before this, a run whose every metadata write failed reported
/// `error_count: 0` and a full page of skips, so nothing surfaced the
/// staleness — and the next run would try again and report the same nothing.
#[tokio::test]
async fn a_304_whose_metadata_write_fails_reports_an_error_not_a_skip() {
    let store_id = "store-1";
    let feed_url = "https://feed.example.com/feed.xml";
    let entry_url = "https://feed.example.com/entry-1";

    let mut bodies = HashMap::new();
    bodies.insert(
        feed_url.to_string(),
        feed_xml_with_author(feed_url, entry_url, "Alice").into_bytes(),
    );
    bodies.insert(
        entry_url.to_string(),
        b"# Entry\n\nFull entry body text.".to_vec(),
    );
    let fetcher = Arc::new(ThenNotModifiedFetcher::new(bodies, feed_url));

    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcNotModified(fetcher.clone())),
        Box::new(ArcNotModified(fetcher.clone())),
    );
    let source = feed_source(store_id, feed_url);
    let store = FailingMetadataStore::new();
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

    store.start_failing();
    fetcher.set_body(
        feed_url,
        feed_xml_with_author(feed_url, entry_url, "Bob").into_bytes(),
    );
    let second = run_source_ingestion(
        &source,
        &ingestor,
        SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
    )
    .await
    .unwrap();

    assert_eq!(second.docs_seen, 1);
    assert_eq!(
        second.error_count, 1,
        "a failed metadata write is an error, reported exactly once"
    );
    assert_eq!(
        second.docs_skipped, 0,
        "a failed write must not report as a clean skip"
    );
    assert_eq!(second.docs_metadata_updated, 0);
}

// ---------------------------------------------------------------------------
// A `Last-Modified`-only origin (specs/04-search-pipeline.md §1)
// ---------------------------------------------------------------------------

/// An origin that issues no `ETag` and moves its `Last-Modified` on every
/// response, while serving the same bytes throughout — the shape that makes
/// the stored validator go stale without any hash input moving. Records the
/// `FetchMetadata` each call received, like `ScriptedFetcher`.
struct MovingLastModifiedFetcher {
    body: Vec<u8>,
    served: Mutex<usize>,
    received: Mutex<Vec<FetchMetadata>>,
}

impl MovingLastModifiedFetcher {
    fn new(body: &[u8]) -> Self {
        Self {
            body: body.to_vec(),
            served: Mutex::new(0),
            received: Mutex::new(Vec::new()),
        }
    }

    fn received(&self) -> Vec<FetchMetadata> {
        self.received.lock().unwrap().clone()
    }
}

#[async_trait]
impl UrlFetcher for MovingLastModifiedFetcher {
    async fn fetch(&self, _url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.received.lock().unwrap().push(meta.clone());
        let mut served = self.served.lock().unwrap();
        *served += 1;
        Ok(FetchResult::Downloaded {
            bytes: self.body.clone(),
            content_type: Some("text/markdown".to_string()),
            etag: None,
            last_modified: Some(format!("Wed, 0{} Oct 2015 07:28:00 GMT", *served)),
            final_url: None,
        })
    }
}

struct ArcMovingLastModified(Arc<MovingLastModifiedFetcher>);
#[async_trait]
impl UrlFetcher for ArcMovingLastModified {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.0.fetch(url, meta).await
    }
}

/// A `200` whose body and metadata are unchanged but whose `Last-Modified`
/// moved must still persist the new validator.
///
/// `external_last_modified` is deliberately not a `compute_metadata_hash`
/// input, so the incremental skip-check's hash comparison cannot see it move.
/// With the hash alone deciding, this origin's second run took the plain-skip
/// branch and wrote nothing: the stored validator stayed at run 1's value,
/// and run 3 — and every run after it — replayed an `If-Modified-Since` the
/// origin had already moved past, so a resource that never changes was
/// downloaded in full forever. Three runs are the minimum that shows it: run
/// 2 proves the write happens, run 3 proves what the write stored is what
/// gets replayed.
#[tokio::test]
async fn a_moved_last_modified_on_an_unchanged_200_is_persisted_and_replayed() {
    let store_id = "store-1";
    let url = "https://example.com/doc";
    let fetcher = Arc::new(MovingLastModifiedFetcher::new(b"# Doc\n\nBody text."));

    let ingestor = UrlIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcMovingLastModified(fetcher.clone())),
    );
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

    macro_rules! run {
        () => {
            run_source_ingestion(
                &source,
                &ingestor,
                SourceIngestionDeps::for_test(&mut doc_index, &store, &embedder, &config),
            )
            .await
            .unwrap()
        };
    }

    let first = run!();
    assert_eq!(first.docs_indexed, 1, "run 1 indexes the resource");

    let second = run!();
    assert_eq!(
        second.docs_metadata_updated, 1,
        "the moved Last-Modified is the only thing that changed, and it must be written"
    );
    assert_eq!(
        second.docs_skipped, 0,
        "a run that rewrote the validator is not a plain skip"
    );
    assert_eq!(second.chunks_written, 0, "and it re-chunks nothing");

    let third = run!();
    assert_eq!(third.docs_metadata_updated, 1);

    let received = fetcher.received();
    assert_eq!(received.len(), 3, "one fetch per run");
    assert_eq!(
        received[0],
        FetchMetadata::default(),
        "run 1 has nothing stored to replay"
    );
    assert_eq!(
        received[1],
        FetchMetadata {
            etag: None,
            last_modified: Some("Wed, 01 Oct 2015 07:28:00 GMT".to_string()),
        },
        "run 2 replays run 1's validator"
    );
    assert_eq!(
        received[2],
        FetchMetadata {
            etag: None,
            last_modified: Some("Wed, 02 Oct 2015 07:28:00 GMT".to_string()),
        },
        "run 3 must replay the validator run 2 stored, not run 1's stale one"
    );
}
