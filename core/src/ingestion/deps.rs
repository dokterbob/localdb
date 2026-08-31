//! The ingestion pipeline's dependency-injection surface: the structs a
//! caller builds to drive [`super::pipeline::index_resource`] or
//! [`super::run_source_ingestion`], plus the small data types those
//! dependencies traffic in ([`DocumentRecord`]/[`DocumentIndex`] for the
//! incremental-skip index, [`FetchMetadata`]/[`FetchResult`]/[`UrlFetcher`]
//! for conditional GET, [`DeletionPolicy`] for the delete-sweep opt-in).
//!
//! Grouped separately from the mechanics that consume them
//! (`super::pipeline`, `super::liveness`) because this is the contract
//! external callers (the CLI, the daemon, `ingest`'s tests) actually
//! construct — keeping it in one place means a caller building a
//! `SourceIngestionDeps` never has to go looking through the pipeline
//! implementation to find out what it needs to provide.

use std::collections::HashMap;

use crate::embedder::Embedder;
use crate::error::Error;
use crate::store::RetrievalStore;

use super::IngestionConfig;

/// A lightweight record of a previously-indexed document, used to detect
/// content changes and enable incremental skip or replace-by-URI.
///
/// Stored by the pipeline coordinator; for one-shot (non-daemon) use, this
/// lives in-memory only during the run.
#[derive(Debug, Clone)]
pub struct DocumentRecord {
    /// Canonical URI of the document.
    pub uri: String,
    /// Content-addressed document ID from last indexing.
    pub resource_id: String,
    /// ID of the source that last indexed this document — the delete-sweep's
    /// ownership key. Persisted as `resources.source_id` (baseline schema),
    /// so rehydrated indexes know it for every row ever written.
    pub source_id: String,
    /// blake3 content hash of normalized text from last indexing.
    pub content_hash: String,
    /// The policy version that was used to index this document.
    pub policy_version: String,
    /// `core::ids::compute_metadata_hash` of the persisted metadata state
    /// (post-title-backfill `Metadata` plus
    /// `external_id`/`external_etag`/`modified_at`) from last indexing or
    /// last metadata-only update.
    /// Drives the metadata-only incremental update (issue #176;
    /// specs/04-search-pipeline.md): a mismatch here, with `content_hash`
    /// and `policy_version` both unchanged, means only the resource row
    /// needs rewriting, not chunks/embeddings. Kept as a plain field (not an
    /// extension point) so a future addition (e.g. #269's
    /// `extractor_version`) is a small struct change, not a redesign.
    pub metadata_hash: String,
    /// Raw HTTP `ETag` validator captured from the last successful fetch of
    /// this resource (`url` sources and feed entry links only; `None`
    /// otherwise). Replayed as `If-None-Match` on the next fetch of the same
    /// URI — see `IngestCallback::lookup_fetch_metadata` — subject to the
    /// suppression rule: only when `policy_version` still matches the run's.
    /// See specs/04-search-pipeline.md §1.
    pub external_etag: Option<String>,
    /// Raw HTTP `Last-Modified` validator, replayed as `If-Modified-Since`
    /// under the same conditions as `external_etag`. Unlike `external_etag`,
    /// not an input to `core::ids::compute_metadata_hash` — see
    /// specs/02-domain-model.md §2.
    pub external_last_modified: Option<String>,
}

/// In-memory index of previously-seen documents keyed by URI.
///
/// Used by the ingestion pipeline to detect unchanged, changed, and deleted
/// documents within a single run.
pub struct DocumentIndex {
    /// Map from canonical URI to the last-indexed record.
    records: HashMap<String, DocumentRecord>,
}

impl DocumentIndex {
    /// Create a new empty index.
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    /// Pre-populate the index from lightweight `DocumentRecord`s returned by
    /// `RetrievalStore::list_indexed_documents`. Use this to rehydrate the
    /// incremental-skip index across process runs without loading embeddings.
    pub fn from_records(records: Vec<DocumentRecord>) -> Self {
        let map = records.into_iter().map(|r| (r.uri.clone(), r)).collect();
        Self { records: map }
    }

    /// Look up a document record by URI.
    pub fn get(&self, uri: &str) -> Option<&DocumentRecord> {
        self.records.get(uri)
    }

    /// Insert or update a record.
    pub fn upsert(&mut self, record: DocumentRecord) {
        self.records.insert(record.uri.clone(), record);
    }

    /// Remove a record by URI and return it if it existed.
    pub fn remove(&mut self, uri: &str) -> Option<DocumentRecord> {
        self.records.remove(uri)
    }

    /// List all URIs currently in the index.
    pub fn uris(&self) -> Vec<String> {
        self.records.keys().cloned().collect()
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl Default for DocumentIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether an ingestion run may remove documents from the store.
///
/// Deletion is opt-in, following `rsync --delete` (issues #156/#185): removing
/// indexed content is destructive and asymmetric — a wrong delete cost this
/// project's `books` store ~4.4M chunks and a full re-index, while a missed
/// delete costs only a stale search hit. Retaining is also frequently what a
/// user actually wants from a local index: a copy of a newspaper article that
/// has since 404'd is *more* valuable for having outlived its origin, not less.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeletionPolicy {
    /// Never remove anything; report what would have been removed via
    /// [`IngestionResult::docs_prunable`]. The default.
    #[default]
    Retain,
    /// Remove documents confirmed gone at the origin, and — subject to the
    /// enumeration guards — documents absent from this run.
    Prune,
}

/// Metadata from a previous URL fetch, used for conditional GET.
///
/// `Serialize`/`Deserialize` are derived because this type is embedded in
/// [`IngestionResult`], which crosses the SSE wire boundary via
/// [`crate::progress::ProgressEvent::SourceFinished`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FetchMetadata {
    /// ETag value from the previous response.
    pub etag: Option<String>,
    /// Last-Modified value from the previous response.
    pub last_modified: Option<String>,
}

/// Result of fetching a URL.
#[derive(Debug)]
pub enum FetchResult {
    /// Content downloaded successfully.
    Downloaded {
        bytes: Vec<u8>,
        content_type: Option<String>,
        etag: Option<String>,
        last_modified: Option<String>,
        /// Effective URL after redirects, when the fetcher can report one.
        /// `None` means "no redirect information available" — callers must
        /// fall back to the URL they requested, never treat `None` as "no
        /// redirect".
        final_url: Option<String>,
    },
    /// Server returned 304 Not Modified (conditional GET).
    ///
    /// Carries whichever validators the 304 response *itself* carried, raw
    /// and unmodified. RFC 9111 requires a cache to store these: an origin
    /// MAY rotate its `ETag` or `Last-Modified` on a 304 even though the
    /// body is unchanged, and dropping the new value means the next
    /// conditional request replays a stale validator and needlessly gets a
    /// full 200 body back.
    ///
    /// Both fields being `None` is the common case — a bare 304 — and means
    /// "keep whatever validator is already stored", never "clear the stored
    /// validator". That distinction is the whole point of `Option` here: a
    /// caller must not read `None` as an instruction to blank out a
    /// previously stored value.
    NotModified {
        etag: Option<String>,
        last_modified: Option<String>,
    },
    /// Document gone (404/410 after retry). Should trigger deletion.
    Gone,
    /// The fetcher refused to connect because the destination violates its
    /// policy — today, a non-globally-routable address behind a locator that
    /// came from untrusted content (see `fetch`'s destination guard).
    ///
    /// A `FetchResult` variant rather than an `Error` on purpose. `Err` is
    /// the ambiguous-and-possibly-transient bucket; every caller treats it as
    /// "try again next run, keep what we have". A blocked destination is
    /// neither ambiguous nor transient — it will be refused identically next
    /// run — so it belongs beside `Gone` among the stable outcomes the
    /// pipeline knows how to route. Keeping it out of `Error` also means no
    /// new stable exit code is minted (see specs/05-surfaces.md §5).
    Blocked,
}

/// HTTP client seam for URL fetching.
///
/// Allows the ingestion pipeline to be tested without real HTTP.
#[async_trait::async_trait]
pub trait UrlFetcher: Send + Sync {
    /// Fetch a URL, optionally providing previous ETag/Last-Modified for
    /// conditional GET.
    async fn fetch(&self, url: &str, metadata: &FetchMetadata) -> Result<FetchResult, Error>;
}

/// A [`UrlFetcher`] that panics if ever called.
///
/// [`SourceIngestionDeps::fetcher`] is only ever dereferenced by the feed
/// liveness sweep, and only when [`RetrievalStore::list_stale_feed_resources`]
/// returns a non-empty candidate list. Every store used in tests unrelated
/// to that sweep (`FakeStore` included) inherits the trait's no-op default,
/// which always returns an empty list — so this fetcher is a safe filler for
/// every such test's `SourceIngestionDeps` literal, and doubles as an
/// assertion that the sweep really did stay inert for them.
#[cfg(any(test, feature = "test-support"))]
pub struct UnreachableFetcher;

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl UrlFetcher for UnreachableFetcher {
    async fn fetch(&self, url: &str, _metadata: &FetchMetadata) -> Result<FetchResult, Error> {
        panic!("UnreachableFetcher::fetch called for '{url}' — the feed liveness sweep should not have run in this test");
    }
}

/// Dependencies for [`index_resource`]: the storage/embedding seam plus the
/// effective ingestion config (store, embedder, chunker config), minus an
/// extractor — the `Resource` arrives pre-extracted.
pub struct IndexResourceDeps<'a> {
    pub store: &'a dyn RetrievalStore,
    pub embedder: &'a dyn Embedder,
    pub config: &'a IngestionConfig,
}

/// Dependencies for [`run_source_ingestion`]: the mutable incremental-skip
/// index plus everything [`index_resource`] needs, grouped for a single run.
pub struct SourceIngestionDeps<'a> {
    pub doc_index: &'a mut DocumentIndex,
    pub store: &'a dyn RetrievalStore,
    pub embedder: &'a dyn Embedder,
    pub config: &'a IngestionConfig,
    pub progress: Option<crate::progress::ProgressSink>,
    /// Whether this run may remove documents. Defaults to
    /// [`DeletionPolicy::Retain`] — deletion is opt-in.
    pub deletion: DeletionPolicy,
    /// Conditional-GET validators stored for the source's own top-level
    /// document (`sources.feed_etag`/`feed_last_modified`), forwarded
    /// verbatim into [`IngestSource::document_validators`]. The caller reads
    /// these directly off the `SourceRow` it already holds — this type has
    /// no store handle of its own to look them up with. Meaningless for any
    /// non-feed source; defaults to an empty [`FetchMetadata`].
    pub document_validators: FetchMetadata,
    /// The digest stored alongside those validators
    /// (`sources.feed_inputs_digest`), read off the same `SourceRow`.
    /// `None` means "inputs unknown" — a row predating the column — and is
    /// treated as a mismatch. Meaningless for any non-feed source.
    pub stored_inputs_digest: Option<String>,
    /// HTTP client for the feed liveness sweep's own probe of an aged-out
    /// feed entry's link. **Must be the public-destination-only fetcher**
    /// (`fetch::HttpUrlFetcher::new_public_only`) — an entry link is
    /// third-party content chosen by a feed author, not an
    /// operator-configured URL, so it crosses the same trust boundary
    /// `ingest::FeedIngestor::new`'s doc comment describes for its own
    /// `entry_fetcher`. Passing the unrestricted client here is an SSRF
    /// regression. Unused for non-[`SourceSpec::Feed`] sources and for any
    /// run under [`DeletionPolicy::Retain`].
    pub fetcher: &'a dyn UrlFetcher,
}

#[cfg(any(test, feature = "test-support"))]
impl<'a> SourceIngestionDeps<'a> {
    /// Build a `SourceIngestionDeps` for the four fields nearly every test
    /// cares about, defaulting the rest to what the overwhelming majority of
    /// call sites across `core`'s and `ingest`'s test suites already wrote by
    /// hand: `progress: None` (no progress sink under test), `deletion:
    /// DeletionPolicy::Retain` (matching the type's own opt-in-deletion
    /// default — tests that exercise the delete-sweep or the feed liveness
    /// sweep, which only run under `Prune`, override it explicitly),
    /// `document_validators: FetchMetadata::default()` and
    /// `stored_inputs_digest: None` (no prior conditional-GET state), and
    /// `fetcher: &UnreachableFetcher` (the sweep never runs, so the fetcher
    /// is never dereferenced — see `UnreachableFetcher`'s own doc comment).
    ///
    /// A call site that needs a non-default value for any of these five
    /// still uses a plain struct literal, or struct-update syntax over this
    /// constructor — this constructor exists to remove the boilerplate at the
    /// sites that don't vary it, not to hide the field from sites that do.
    /// `deletion` is the one field varied often enough to get a constructor
    /// of its own; see [`Self::for_test_pruning`].
    pub fn for_test(
        doc_index: &'a mut DocumentIndex,
        store: &'a dyn RetrievalStore,
        embedder: &'a dyn Embedder,
        config: &'a IngestionConfig,
    ) -> Self {
        Self {
            doc_index,
            store,
            embedder,
            config,
            progress: None,
            deletion: DeletionPolicy::Retain,
            document_validators: FetchMetadata::default(),
            stored_inputs_digest: None,
            fetcher: &UnreachableFetcher,
        }
    }

    /// [`Self::for_test`] with `deletion: DeletionPolicy::Prune`.
    ///
    /// A separate constructor rather than a boolean parameter, and rather
    /// than leaving these sites on struct-update syntax: `deletion` is the
    /// one field the test suites genuinely split on — every sweep test needs
    /// `Prune`, since neither the delete-sweep nor the feed liveness sweep
    /// does anything under `Retain` — and naming it in the constructor says
    /// at the call site which of the two a test is exercising.
    pub fn for_test_pruning(
        doc_index: &'a mut DocumentIndex,
        store: &'a dyn RetrievalStore,
        embedder: &'a dyn Embedder,
        config: &'a IngestionConfig,
    ) -> Self {
        Self {
            deletion: DeletionPolicy::Prune,
            ..Self::for_test(doc_index, store, embedder, config)
        }
    }
}
