//! Ingestion pipeline — scan-and-index orchestration.
//!
//! Coordinates: enumerate sources → acquire → extract → chunk → embed → upsert.
//!
//! Key behaviors:
//! - **Incremental skip**: if `content_hash` unchanged for a URI, skip reprocessing.
//! - **Replace-by-URI**: on change, delete old chunks then insert new ones.
//! - **Deletes**: file deleted / URL 404-410 / source removed → delete its chunks.
//! - **IndexJob lifecycle**: pending → running → done | failed; stats accumulated.
//! - **Policy version stamping**: every chunk carries `policy_version`; if the
//!   stored policy hash differs from the effective one, the store is marked stale.
//!
//! One-shot semantics only (T11 adds scheduling/watching).
//!
//! See specs/04-search-pipeline.md §1, §3, §4.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::block::Resource;
use crate::chunker::{chunk_blocks, CharSizer, ChunkSizer, ChunkerConfig, TokenSizer};
use crate::embedder::{DocumentChunks, Embedder};
use crate::error::Error;
use crate::ids::new_ulid;
use crate::ingestor::{
    Enumeration, IngestCallback, IngestSource, Ingestor, MetadataWriteOutcome, SkipReason,
};
use crate::metadata::Metadata;
use crate::store::{ChunkRecord, RetrievalStore, StaleFeedResource};
use crate::types::{
    Chunk, IndexJob, IndexJobScope, IndexJobState, IndexJobStats, Provenance, Source, SourceRef,
    SourceSpec,
};
use crate::uri::Uri;

// ---------------------------------------------------------------------------
// DocumentRecord — tracks what was last indexed for a URI
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// DocumentIndex — in-memory index of known documents
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// IngestionConfig — parameters for a single pipeline run
// ---------------------------------------------------------------------------

/// Configuration for a single ingestion pipeline run.
#[derive(Clone)]
pub struct IngestionConfig {
    /// Store ID (ULID) owning this run.
    pub store_id: String,
    /// The computed policy version hash for the current indexing policy.
    pub policy_version: String,
    /// Chunking config derived from the effective store policy.
    pub chunker: ChunkerConfig,
}

// ---------------------------------------------------------------------------
// IngestionResult — summary returned by the pipeline after a run
// ---------------------------------------------------------------------------

/// Result of a completed ingestion pipeline run.
///
/// `Serialize`/`Deserialize` are derived because this type is embedded in
/// [`crate::progress::ProgressEvent::SourceFinished`], which crosses the
/// SSE wire boundary (issue #83).
///
/// `#[serde(default)]` at the **struct** level, matching
/// [`crate::types::IndexJobStats`], and for the same reason: this crosses a
/// version boundary. `localdb index` attaches to a running daemon's SSE
/// stream, and the two are not upgraded in lockstep — a newer CLI reading an
/// older daemon's `SourceFinished` frame would otherwise fail the whole
/// deserialize on the first field the daemon does not know about, dropping a
/// frame the user is watching rather than reading it with zeros for the
/// fields that are missing. Struct-level, not per-field, so every counter
/// added later inherits it without anyone having to remember.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IngestionResult {
    /// Total documents seen in the scan.
    pub docs_seen: u64,
    /// Documents actually indexed (new or changed content).
    pub docs_indexed: u64,
    /// Documents skipped (unchanged content hash).
    pub docs_skipped: u64,
    /// Documents deleted (no longer in source).
    pub docs_deleted: u64,
    /// Total chunks written to the retrieval backend.
    pub chunks_written: u64,
    /// Files with unsupported format (counted but not errors).
    pub unsupported_format_count: u64,
    /// Files that errored during processing.
    pub error_count: u64,
    /// Documents this run would have deleted had deletion been enabled
    /// ([`DeletionPolicy::Prune`]) — either confirmed gone at the origin or
    /// absent from a trustworthy enumeration. Always 0 when pruning ran, since
    /// then they were actually deleted and counted in `docs_deleted`.
    ///
    /// Surfaced so a default (retaining) run can tell the user what `--delete`
    /// would remove, instead of silently accumulating stale documents.
    pub docs_prunable: u64,
    /// Documents whose content and policy were unchanged but whose metadata
    /// differed from what's persisted — the resource row was rewritten in
    /// place, no chunks/embeddings touched (issue #176). A strict subset of
    /// what would otherwise be counted in `docs_skipped`: a metadata-only
    /// update is not a content skip, but it is also not a full `docs_indexed`
    /// re-index, so it gets its own counter rather than overloading either.
    pub docs_metadata_updated: u64,
    /// Number of feed-discovered resources the liveness sweep probed this
    /// run — every candidate a fetch was actually attempted for, regardless
    /// of outcome (`Gone`, `NotModified`, `Downloaded`, `Blocked`, or a
    /// transport error all count; a candidate skipped because this run's
    /// own ingestion pass already observed it does not). Confirmed-gone
    /// prunes the sweep performs fold into `docs_deleted` above rather than
    /// a separate counter, so this is the only place the sweep's own probe
    /// work is visible — including on a run that deleted nothing. Always 0
    /// for a non-feed source and for any run under
    /// [`DeletionPolicy::Retain`] — see
    /// specs/04-search-pipeline.md §1 "Aged-out feed entries: the liveness
    /// sweep".
    pub feed_entries_liveness_checked: u64,
    /// Refreshed validators for a feed source's own top-level document, to
    /// persist onto `sources.feed_etag`/`feed_last_modified` — threaded
    /// straight from [`crate::ingestor::IngestResult::document_validators`].
    /// `None` for every non-feed source, and for a feed source whose run
    /// left the stored validators untouched (a bare 304, or no document
    /// fetch at all).
    pub document_validators: Option<FetchMetadata>,
    /// The local-inputs digest in force for this run
    /// (`crate::ids::compute_feed_inputs_digest`), to persist onto
    /// `sources.feed_inputs_digest` **in the same hop** as
    /// [`Self::document_validators`]. `None` for every non-feed source.
    ///
    /// Persisted only alongside the validators, never on its own: the two
    /// are one fact — "these validators were captured under these inputs".
    /// Recording new inputs against validators the run never refreshed would
    /// mark the cache trustworthy for a reprocessing that did not happen,
    /// and the next run would replay them and skip the entry loop, which is
    /// the exact failure the digest exists to prevent.
    pub document_inputs_digest: Option<String>,
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

// ---------------------------------------------------------------------------
// Staleness check
// ---------------------------------------------------------------------------

/// Check if the store's existing data is stale relative to the current policy.
///
/// Returns `true` if the sampled chunk was indexed with a different policy version.
/// Callers should trigger a full reindex when this is true.
///
/// # Note
/// This samples one chunk from the store as a representative. In a consistent
/// store all chunks share the same policy version (reindex is atomic per document),
/// so a single sample is sufficient in practice. If partial-reindex bugs occur,
/// this check may give a false negative; a full scan is not performed for performance.
pub async fn is_store_stale(
    store: &dyn RetrievalStore,
    current_policy_version: &str,
) -> Result<bool, Error> {
    let stats = store.stats().await?;
    if stats.chunk_count == 0 {
        // An empty store is never stale — there is nothing to reindex.
        return Ok(false);
    }

    // Sample one chunk via BM25 to check its policy version.
    //
    // We avoid dense_search here because it requires a query vector whose
    // dimension must match the index.  An empty (&[]) or zero-length vector
    // causes real LanceDB implementations to return an error.
    //
    // The BM25 query uses very common single-character substrings ("e t a")
    // so that any chunk containing typical text will produce a match.  If the
    // store contains only numeric or symbolic content and no result is returned,
    // we conservatively return `false` (not stale) to avoid a spurious reindex.
    let results = store.bm25_search("e t a", 1, &[]).await?;
    if results.is_empty() {
        return Ok(false);
    }

    let sample = &results[0].chunk;
    Ok(sample.policy_version != current_policy_version)
}

// ---------------------------------------------------------------------------
// index_source_path — enumerate files in a path source
// ---------------------------------------------------------------------------

/// A file found by path-source enumeration.
#[derive(Debug, Clone)]
pub struct FoundFile {
    /// Absolute file path.
    pub path: std::path::PathBuf,
    /// Canonical file URI: `file:///absolute/path`.
    pub uri: Uri,
}

/// The outcome of enumerating a `path`-kind source.
///
/// This is an enum rather than a plain `Vec<FoundFile>` on purpose (#156):
/// a missing root used to be flattened into `Ok(vec![])`, indistinguishable
/// from an empty-but-present directory, and the delete-sweep read that empty
/// vector as "every file in this source was deleted." Making the caller
/// destructure the two cases is the fix — every future caller has to confront
/// the distinction that caused the data loss.
#[derive(Debug, Clone)]
pub enum PathEnumeration {
    /// The root was present and walked in full: these are all its files.
    Complete(Vec<FoundFile>),
    /// The root does not exist — an unmounted volume, a detached external
    /// disk, a moved directory. Says nothing about whether the files it used
    /// to hold still exist, so it must never license a delete.
    RootUnavailable,
}

impl PathEnumeration {
    /// The enumerated files, or an empty slice if the root was unavailable.
    ///
    /// Convenience for callers that only care about what was found (tests,
    /// display). Anything that *deletes* on the strength of absence must
    /// match on the variant instead.
    pub fn files(&self) -> &[FoundFile] {
        match self {
            PathEnumeration::Complete(files) => files,
            PathEnumeration::RootUnavailable => &[],
        }
    }
}

/// Enumerate files in a `path`-kind source, applying include/exclude globs.
///
/// Returns [`PathEnumeration::Complete`] with the found files sorted by path
/// for determinism, or [`PathEnumeration::RootUnavailable`] if the configured
/// root does not exist.
///
/// # Errors
/// Returns `Error::Internal` if the root path exists but cannot be read.
pub fn enumerate_path_source(
    root: &str,
    include: &[String],
    exclude: &[String],
) -> Result<PathEnumeration, Error> {
    let root_path = Path::new(root);

    if !root_path.exists() {
        // #156: a root that isn't there is *unavailable*, not empty. Reporting
        // it as zero files is what let an unmounted volume delete a whole
        // source's worth of indexed documents.
        return Ok(PathEnumeration::RootUnavailable);
    }

    let include_set = build_glob_set(include)?;
    let exclude_set = build_glob_set(exclude)?;
    let include_empty = include.is_empty();

    let mut found = Vec::new();
    enumerate_dir(
        root_path,
        root_path,
        &include_set,
        include_empty,
        &exclude_set,
        &mut found,
    )?;
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(PathEnumeration::Complete(found))
}

/// Recursively enumerate a directory.
fn enumerate_dir(
    root: &Path,
    dir: &Path,
    include_set: &GlobSet,
    include_empty: bool,
    exclude_set: &GlobSet,
    found: &mut Vec<FoundFile>,
) -> Result<(), Error> {
    let entries = std::fs::read_dir(dir).map_err(|e| Error::Internal {
        message: format!("cannot read directory '{}': {}", dir.display(), e),
        correlation_id: "enumerate_dir".to_string(),
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| Error::Internal {
            message: format!("error reading directory entry: {}", e),
            correlation_id: "enumerate_dir_entry".to_string(),
        })?;

        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative_str = relative.to_string_lossy();

        // Apply exclude globs first. Match the root-relative path (so anchored
        // patterns like `**/node_modules/**` work) AND the bare file/dir name (so
        // a bare pattern like `.DS_Store` matches at any depth, e.g.
        // `Call/.DS_Store`). The include check below intentionally stays
        // path-anchored.
        if let Some(name) = path.file_name() {
            let basename = name.to_string_lossy();
            if exclude_set.is_match(relative_str.as_ref())
                || exclude_set.is_match(basename.as_ref())
            {
                continue;
            }
        } else if exclude_set.is_match(relative_str.as_ref()) {
            continue;
        }

        if path.is_dir() {
            enumerate_dir(root, &path, include_set, include_empty, exclude_set, found)?;
        } else if path.is_file() {
            // Apply include globs: if any are specified, file must match one
            if !include_empty && !include_set.is_match(relative_str.as_ref()) {
                continue;
            }

            let abs_path = path.canonicalize().unwrap_or(path.clone());
            // `Uri::from_file_path` percent-encodes correctly (spaces,
            // non-ASCII, `#`, `?`, ...), unlike the old lossy
            // `format!("file://{}", path.display())`. It returns `None` only
            // for a non-absolute path, which `abs_path` is not — *unless*
            // `canonicalize()` above failed (the file was moved or deleted
            // between `is_file()` and here) and the source's configured root
            // was itself relative, which `normalize_path_source` permits.
            //
            // Error out rather than panicking or silently dropping the file.
            // Dropping it would be the worse of the two: the file would never
            // be reported to the pipeline, so the delete-sweep would treat its
            // still-live document as gone and delete it — exactly the data
            // loss this module's normalization work exists to prevent.
            // Returning `Err` aborts the run before the sweep, so nothing is
            // deleted on the strength of an incomplete enumeration.
            let uri = Uri::from_file_path(&abs_path).ok_or_else(|| Error::Internal {
                message: format!(
                    "cannot build a file:// URI for non-absolute path '{}' \
                     (canonicalization failed and the source root is relative)",
                    abs_path.display()
                ),
                correlation_id: "enumerate_dir".to_string(),
            })?;
            found.push(FoundFile {
                path: abs_path,
                uri,
            });
        }
    }

    Ok(())
}

/// Build a compiled `GlobSet` from a slice of glob pattern strings.
///
/// Each pattern is compiled with `literal_separator(true)` so that `*` and `?`
/// do not cross `/`, while `**` still matches across directory boundaries —
/// matching the pre-existing semantics exactly.
fn build_glob_set(patterns: &[String]) -> Result<GlobSet, Error> {
    let mut b = GlobSetBuilder::new();
    for pat in patterns {
        let glob = GlobBuilder::new(pat)
            .literal_separator(true)
            .build()
            .map_err(|e| Error::InvalidConfig {
                message: format!("invalid glob pattern '{pat}': {e}"),
            })?;
        b.add(glob);
    }
    b.build().map_err(|e| Error::InvalidConfig {
        message: format!("failed to build glob set: {e}"),
    })
}

/// Thin wrapper used only by unit tests: match a single pattern against a path.
#[cfg(test)]
fn glob_match(pattern: &str, path: &str) -> bool {
    let Ok(set) = build_glob_set(&[pattern.to_string()]) else {
        return false;
    };
    set.is_match(path)
}

/// Scale a prose token budget to a character budget (×4) for `CharSizer`.
///
/// Used when the embedder has no local tokenizer: the prose preset's
/// token-denominated `target`/`overlap` are reinterpreted as ~4 chars/token so
/// the character-based splitter approximates the intended token budget. Only the
/// `prose` preset is scaled; `code` already uses a char budget.
fn scale_to_chars(config: &ChunkerConfig) -> ChunkerConfig {
    if config.preset != "prose" {
        return config.clone();
    }
    ChunkerConfig {
        preset: config.preset.clone(),
        target_tokens: Some(config.resolved_target_tokens() * 4),
        overlap_tokens: Some(config.resolved_overlap_tokens() * 4),
        window_turns: config.window_turns,
        stride_turns: config.stride_turns,
    }
}

/// Run a fallible, synchronous closure and convert any panic into an `Error::Internal`.
///
/// Any panic in extraction or chunking is downgraded to a per-document error so
/// the ingestion loop can continue with the next file rather than unwinding the
/// whole process.
///
/// The default panic hook is temporarily replaced with a no-op before calling
/// `catch_unwind` to suppress the `thread 'main' panicked at ...` output that
/// the default hook prints to stderr.  This swap is NOT thread-safe (the hook
/// is a global), so callers must ensure no concurrent `catch_panic` calls occur.
/// Currently extraction runs single-threaded, so this is safe.
fn catch_panic<T>(
    label: &str,
    f: impl FnOnce() -> Result<T, Error> + std::panic::UnwindSafe,
) -> Result<T, Error> {
    // Suppress the default panic hook's stderr output for any unexpected
    // third-party parser panic on malformed input. The caller emits a clean
    // WARN line instead. (The PDF path no longer panics — pdf_oxide returns
    // errors — but this stays as belt-and-braces for every parser.)
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(f);
    std::panic::set_hook(prev_hook);

    match result {
        Ok(result) => result,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".to_string());
            Err(Error::Internal {
                message: format!("{label} panicked: {msg}"),
                correlation_id: label.replace(' ', "_"),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// URL fetching — conditional GET
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// IndexJob management helpers
// ---------------------------------------------------------------------------

/// Create a new IndexJob in `Pending` state.
pub fn create_index_job(store_id: &str, scope: IndexJobScope) -> IndexJob {
    IndexJob {
        id: new_ulid(),
        store_id: store_id.to_string(),
        scope,
        state: IndexJobState::Pending,
        stats: IndexJobStats::default(),
        error: None,
        error_code: None,
        created_at: now_rfc3339(),
        started_at: None,
        completed_at: None,
    }
}

/// Mark an IndexJob as running.
pub fn start_index_job(job: &mut IndexJob) {
    job.state = IndexJobState::Running;
    job.started_at = Some(now_rfc3339());
}

/// Mark an IndexJob as done with final stats.
pub fn complete_index_job(job: &mut IndexJob, stats: IndexJobStats) {
    job.state = IndexJobState::Done;
    job.stats = stats;
    job.completed_at = Some(now_rfc3339());
}

/// Mark an IndexJob as failed with an unclassified error message — a
/// synthetic queue-level failure (the queue itself is full/closed, or the
/// job's task panicked) that never had a typed `core::Error` to carry a
/// stable code from. `job.error_code` is left `None`; a caller reconstructing
/// the job's error (`cli::job_attach::finish_job`) falls back to
/// `Error::Internal` for these, same as it always has.
pub fn fail_index_job(job: &mut IndexJob, error: String) {
    job.state = IndexJobState::Failed;
    job.error = Some(error);
    job.error_code = None;
    job.completed_at = Some(now_rfc3339());
}

/// Mark an IndexJob as failed from a typed `core::Error`, carrying both its
/// display message (`job.error`) and its stable `code()` string
/// (`job.error_code`) — the pairing `Error::from_code` can invert. This is
/// what lets a daemon-attached job failure surface with the same exit code
/// an embedded pre-flight failure of the same kind would (issue #187
/// review): without it, every job-level failure collapsed to a bare string,
/// indistinguishable from `Error::Internal` once read back by the CLI.
pub fn fail_index_job_with_error(job: &mut IndexJob, error: &Error) {
    job.state = IndexJobState::Failed;
    // Store the bare message (`raw_message()`), not `error.to_string()`:
    // `cli::job_attach::finish_job` reconstructs the typed error via
    // `Error::from_code(error_code, error)`, which re-adds the `Display`
    // prefix (e.g. "invalid config: "). Storing the already-prefixed string
    // would double it (issue #187 review, finding F4). Variants
    // `raw_message()` can't reconstruct fall back to the full `Display`
    // string, since there's no bare field to store instead.
    job.error = Some(
        error
            .raw_message()
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
    );
    job.error_code = Some(error.code().to_string());
    job.completed_at = Some(now_rfc3339());
}

/// Get the current time as an RFC 3339 string.
///
/// Only the clock is stubbed under `cfg(test)` (a fixed instant keeps
/// timestamp-carrying fixtures deterministic); the formatting logic itself is
/// always compiled and unit-tested via [`format_secs_rfc3339`].
pub fn now_rfc3339() -> String {
    #[cfg(not(test))]
    {
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }
    #[cfg(test)]
    {
        "2026-06-10T12:00:00Z".to_string()
    }
}

/// Format a Unix timestamp as RFC 3339 (UTC, no sub-second precision).
/// Public so callers that need an RFC 3339 string for an instant *other*
/// than now — e.g. `server`'s terminal-job eviction cutoff (now minus a
/// retention grace) — can produce one that compares correctly against
/// [`now_rfc3339`] output.
///
/// The canonical form contract (specs/02-domain-model.md): always
/// `YYYY-MM-DDTHH:MM:SSZ` — `SecondsFormat::Secs` forbids fractional
/// seconds and the trailing `true` forces a literal `Z` rather than
/// `+00:00`, matching every other stored timestamp in the system so plain
/// string comparison stays correct.
pub fn format_secs_rfc3339(secs: u64) -> String {
    // `i64::try_from`, never `as i64`: a wrapping cast turns a `u64` above
    // `i64::MAX` into a *negative* timestamp, which chrono formats happily as
    // a pre-epoch instant (`u64::MAX` becomes `1969-12-31T23:59:59Z`). That
    // is far worse than the out-of-range fallback below — it is a plausible
    // value that silently sorts before every real row. Fail the conversion
    // instead, and let both out-of-range paths share one fallback.
    let formatted = i64::try_from(secs)
        .ok()
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        // Chrono represents years well past 9999 and renders them with a
        // sign prefix (`+10000-01-01T00:00:00Z`), which is not canonical
        // form — and `+` sorts below every digit, so such a value would
        // order before every real row instead of after them. Both
        // out-of-range paths converge on the fallback below.
        .filter(|s| crate::dates::is_canonical_timestamp(s));

    match formatted {
        Some(s) => s,
        // Reachable only for an input no real Unix timestamp carries: above
        // `i64::MAX`, beyond chrono's representable range, or past year
        // 9999. This function is documented as infallible, so rather than
        // introduce a `Result` no caller has a recovery path for, fall back
        // to the epoch: deterministic, never panics, and canonical-form so
        // it still sorts against every other stored value.
        None => "1970-01-01T00:00:00Z".to_string(),
    }
}

#[cfg(test)]
mod format_secs_rfc3339_tests {
    use super::format_secs_rfc3339;

    #[test]
    fn epoch_zero_is_1970_01_01() {
        assert_eq!(format_secs_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn leap_day_2024_02_29_is_formatted_correctly() {
        assert_eq!(format_secs_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn year_end_boundary_rolls_over_correctly() {
        assert_eq!(format_secs_rfc3339(1_704_067_199), "2023-12-31T23:59:59Z");
        assert_eq!(format_secs_rfc3339(1_704_067_200), "2024-01-01T00:00:00Z");
    }
}

// ---------------------------------------------------------------------------
// Canonical-form contract tests — additional to (not a
// substitute for) the golden `format_secs_rfc3339_tests` exact-value
// assertions above, which are the strongest evidence the chrono
// implementation is behaviorally identical to the hand-rolled arithmetic it
// replaced. These instead guard the *shape* of the contract:
// specs/02-domain-model.md's canonical timestamp form is `YYYY-MM-DDTHH:MM:SSZ`
// — no fractional seconds, `Z` never `+00:00` — and old (hand-rolled-era)
// and new (chrono-era) stored rows must still sort correctly against each
// other by plain string comparison.
#[cfg(test)]
mod canonical_form_tests {
    use super::format_secs_rfc3339;

    /// `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`, checked by hand (no regex
    /// dependency in this crate) rather than as a literal pattern.
    use crate::dates::is_canonical_timestamp as matches_canonical_shape;

    /// `now_rfc3339`'s real-clock branch is stubbed to a fixed literal under
    /// `cfg(test)` and never touches the formatter, so this exercises the
    /// real formatter it delegates to instead — the same function
    /// `now_rfc3339`'s non-test branch calls — to prove a freshly produced
    /// value matches the canonical shape.
    #[test]
    fn format_secs_rfc3339_matches_canonical_shape() {
        assert!(
            matches_canonical_shape(&format_secs_rfc3339(1_783_524_645)),
            "expected canonical YYYY-MM-DDTHH:MM:SSZ shape, got: {}",
            format_secs_rfc3339(1_783_524_645)
        );
    }

    /// Guards specifically against a one-word `SecondsFormat::Secs` ->
    /// `SecondsFormat::AutoSi` typo, which would silently reintroduce
    /// variable-precision output and break every stored-value comparison
    /// that assumes a fixed-width timestamp.
    #[test]
    fn format_secs_rfc3339_has_no_fractional_second_component() {
        assert!(!format_secs_rfc3339(1_783_524_645).contains('.'));
    }

    /// Whatever the input, the output is always canonical form. Chrono
    /// happily represents years past 9999 and renders them with a sign
    /// prefix (`+10000-01-01T00:00:00Z`), which sorts below every real row
    /// because `+` is 0x2B — so those must reach the fallback too, not just
    /// values chrono cannot represent at all.
    #[test]
    fn every_timestamp_formats_to_canonical_form_or_falls_back() {
        for secs in [
            0,
            1_781_092_800,
            253_402_300_800,     // 10000-01-01, representable but not RFC 3339
            1_000_000_000_000,   // ~year 33658
            8_210_298_412_800,   // past chrono's range
            i64::MAX as u64 + 1, // past i64
            u64::MAX,
        ] {
            let formatted = format_secs_rfc3339(secs);
            assert!(
                matches_canonical_shape(&formatted),
                "{secs} produced non-canonical {formatted:?}; a `+`/`-` year prefix \
                 sorts below every digit and would misorder against every stored row"
            );
        }
    }

    /// A `u64` above `i64::MAX` must reach the out-of-range fallback, never
    /// wrap into a negative timestamp. A wrapping `as i64` cast turns
    /// `u64::MAX` into `-1`, which chrono formats as `1969-12-31T23:59:59Z`
    /// — a plausible-looking value that would sort before every real row.
    #[test]
    fn timestamps_above_i64_max_fall_back_instead_of_wrapping_to_pre_epoch() {
        for secs in [u64::MAX, i64::MAX as u64 + 1] {
            let formatted = format_secs_rfc3339(secs);
            assert_eq!(
                formatted, "1970-01-01T00:00:00Z",
                "{secs} must hit the epoch fallback, not wrap"
            );
            assert!(
                !formatted.starts_with("1969"),
                "{secs} wrapped into a pre-epoch instant: {formatted}"
            );
        }
    }

    /// The test that actually matters: a hand-written legacy-form literal
    /// (the exact shape hand-rolled `secs_to_ymd_hms` used to produce) and a
    /// freshly chrono-produced value for the same instant — plus values one
    /// second either side — must still order correctly under plain Rust
    /// string comparison. This is what proves old rows (written before this
    /// migration) and new rows (written after) sort together correctly; a
    /// shape check alone does not.
    #[test]
    fn legacy_literal_and_chrono_produced_value_interleave_correctly() {
        // 2026-06-10T12:00:00Z, the same instant `now_rfc3339`'s cfg(test)
        // branch returns as a literal.
        const AT_SECS: u64 = 1_781_092_800;
        let legacy_literal = "2026-06-10T12:00:00Z".to_string();
        let chrono_produced = format_secs_rfc3339(AT_SECS);
        let one_second_before = format_secs_rfc3339(AT_SECS - 1);
        let one_second_after = format_secs_rfc3339(AT_SECS + 1);

        assert_eq!(legacy_literal, chrono_produced);
        assert!(one_second_before < legacy_literal);
        assert!(one_second_before < chrono_produced);
        assert!(legacy_literal < one_second_after);
        assert!(chrono_produced < one_second_after);
    }
}

// ---------------------------------------------------------------------------
// Ingestion pipeline (#117) — Ingestor-driven, no I/O in core
// ---------------------------------------------------------------------------
//
// `run_source_ingestion` + `index_resource` are the pipeline shape described in
// specs/01-architecture.md §1: the caller (CLI) builds a concrete `&dyn Ingestor`
// per `SourceSpec` and drives it through `core` here, which streams `Resource`s
// one at a time via `PipelineCallback`. Extraction happens outside `core`
// entirely: the ingestor (in the `ingest` crate) does its own acquisition +
// extraction I/O and hands `core` an already-built `Resource` (blocks,
// metadata, content_hash final). `index_resource` preserves the crash-safe A6
// ordering (embed before delete, delete-and-insert in a single replace
// transaction, issue #79) that the pipeline has always used.

/// Dependencies for [`index_resource`]: the storage/embedding seam plus the
/// effective ingestion config (store, embedder, chunker config), minus an
/// extractor — the `Resource` arrives pre-extracted.
pub struct IndexResourceDeps<'a> {
    pub store: &'a dyn RetrievalStore,
    pub embedder: &'a dyn Embedder,
    pub config: &'a IngestionConfig,
}

/// A resource's post-backfill metadata state, plus its derived
/// `metadata_hash` and Dublin Core dates — the single source of truth for
/// both what [`index_resource`] persists and what [`PipelineCallback::on_resource`]
/// compares against, so a resource's title-backfill decision is never made
/// twice with room for the two sites to disagree (issue #176's whole premise:
/// the metadata_hash is comparable across index-time, metadata-update-time,
/// and rehydration-time only if every writer derives it from the *persisted*
/// state via this one function). See specs/04-search-pipeline.md.
struct DerivedResourceState {
    /// `resource.metadata`, with `resource.title` folded into
    /// `dublin_core_mut().title` when the metadata itself carried none.
    metadata: Metadata,
    /// `core::ids::compute_metadata_hash` of `metadata` plus
    /// `resource.external_id`/`external_etag`/`modified_at`.
    metadata_hash: String,
    /// `metadata`'s own Dublin Core `date`, exactly as the source expressed it.
    date_original: Option<String>,
    /// `date_original` normalized via `crate::dates::parse_partial_iso8601`.
    date_parsed: Option<String>,
    /// `resource.modified_at`, unchanged — carried here so the value fed to
    /// `compute_metadata_hash` above is exactly the value later stamped onto
    /// the persisted `ChunkRecord`/`ResourceRecord`, never two separately
    /// read copies.
    modified_at: Option<String>,
}

/// Compute [`DerivedResourceState`] for `resource`. Pure function of
/// `resource` alone — safe to call more than once for the same resource (as
/// [`PipelineCallback::on_resource`] and [`index_resource`] both do, on
/// different branches) since `Metadata` carries no maps and therefore
/// serializes deterministically (see `compute_metadata_hash`'s doc comment).
fn derive_resource_state(resource: &Resource) -> DerivedResourceState {
    // Title propagation: resource.title backfills the metadata's Dublin Core
    // title when the resource's own metadata doesn't already carry one.
    let mut metadata = resource.metadata.clone();
    if metadata.dublin_core().title.is_none() {
        if let Some(title) = &resource.title {
            metadata.dublin_core_mut().title = Some(title.clone());
        }
    }

    // The resource's own claimed date, exactly as the source expressed it —
    // computed once per resource (not redundantly per chunk). Read from
    // `metadata` (post-title-backfill is fine: the Dublin Core date itself
    // is never backfilled) rather than `resource.metadata`, so a future
    // backfill of `dc.date` would be picked up here too.
    let date_original = metadata.dublin_core().date.clone();
    let date_parsed = date_original
        .as_deref()
        .and_then(crate::dates::parse_partial_iso8601);

    let modified_at = resource.modified_at.clone();

    let metadata_hash = crate::ids::compute_metadata_hash(
        &metadata,
        resource.external_id.as_deref(),
        resource.external_etag.as_deref(),
        modified_at.as_deref(),
    );

    DerivedResourceState {
        metadata,
        metadata_hash,
        date_original,
        date_parsed,
        modified_at,
    }
}

/// Compute the effective `ChunkerConfig` for one resource (issue #60; see
/// specs/04-search-pipeline.md §3 "Source preset override").
///
/// - A source whose `source_preset` is anything other than the default
///   `"prose"` is authoritative: `base_chunker` (assumed already resolved for
///   that preset by the caller, e.g. `ChunkerConfig::code()`/`::messages()`
///   plus any store-level overrides) is used **unconditionally**, regardless
///   of what per-file detection would otherwise guess. This is what lets an
///   explicit `code` or `messages` source win over a `.md` file that
///   `preset_for` would otherwise route to `prose`.
/// - A `"prose"` (default) source allows per-file auto-routing: `preset_for`
///   inspects `filename_hint`/`mime`; when it says `"code"`,
///   `ChunkerConfig::code()` defaults are used, otherwise `base_chunker` (the
///   store's configured prose chunker) is used.
fn effective_chunker_config(
    source_preset: &str,
    base_chunker: &ChunkerConfig,
    filename_hint: Option<&str>,
    mime: Option<&str>,
) -> ChunkerConfig {
    if source_preset != "prose" {
        return base_chunker.clone();
    }
    use crate::chunker::preset_for;
    if preset_for(filename_hint, mime) == "code" {
        ChunkerConfig::code()
    } else {
        base_chunker.clone()
    }
}

/// Derive a filename hint from a resource's URI: its last path segment, if any.
///
/// Used by [`effective_chunker_config`]'s per-file auto-routing. Non-hierarchical
/// or extension-less URIs (e.g. `notion://page/abc123`) simply yield `None`,
/// falling through to mime-based or default (`prose`) routing.
fn filename_hint_from_uri(uri: &crate::uri::Uri) -> Option<String> {
    let last = uri.as_url().path_segments()?.next_back()?;
    if last.is_empty() {
        None
    } else {
        Some(last.to_string())
    }
}

/// Index a single already-built `Resource`: the post-extraction half of the
/// pipeline (preset gate → chunk → embed → upsert).
///
/// Crash-safe A6 ordering: chunking and embedding happen first (read-only /
/// reversible); only once embedding has succeeded is `replaces_resource_id`
/// threaded into a single
/// `upsert_chunks_and_blocks` call, so a write failure leaves any existing
/// document for this URI intact and searchable (issue #79) — the replace
/// delete is never issued as a separate call.
///
/// The skip-check (unchanged content) is the caller's responsibility (see
/// `PipelineCallback` below) — this function always (re)indexes; `resource`'s
/// blocks, metadata, and `content_hash` must already be final.
///
/// Returns [`IndexOutcome::Written`] with the number of chunks written and
/// the persisted metadata_hash, or [`IndexOutcome::Empty`] if the resource
/// produced no chunks at all.
pub async fn index_resource(
    resource: &Resource,
    source: &Source,
    replaces_resource_id: Option<&str>,
    deps: &IndexResourceDeps<'_>,
) -> Result<IndexOutcome, Error> {
    let token_counter = deps.embedder.token_counter();
    let sizer: Box<dyn ChunkSizer> = match &token_counter {
        Some(f) => Box::new(TokenSizer::new(f.clone())),
        None => Box::new(CharSizer),
    };

    // Preset gate (#60).
    let filename_hint = filename_hint_from_uri(&resource.uri);
    let effective_chunker = effective_chunker_config(
        &source.source_preset,
        &deps.config.chunker,
        filename_hint.as_deref(),
        resource.mime.as_deref(),
    );

    let chunker_cfg = if token_counter.is_none() {
        scale_to_chars(&effective_chunker)
    } else {
        effective_chunker
    };

    let chunk_outputs = catch_panic(
        "chunk",
        std::panic::AssertUnwindSafe(|| {
            chunk_blocks(&resource.id, &resource.blocks, &chunker_cfg, sizer.as_ref())
        }),
    )?;

    if chunk_outputs.is_empty() {
        // #185 — the sink's invariant: an empty replacement writes nothing AND
        // deletes nothing.
        //
        // This arm used to `delete_by_resource(replaces_resource_id)`, on the
        // reading that a resource which now chunks to nothing is a document
        // that has become empty. But `index_resource` cannot distinguish
        // "this file is legitimately empty now" from "extraction produced
        // nothing this run" (a scanned PDF with no text layer, a parser
        // regression, an HTML page whose body failed to render) — and only the
        // first is evidence the content is gone. Guarding this at each
        // ingestor (as PR #170 did for url/feed) leaves every future connector
        // one oversight away from silently erasing a URI's content, so the
        // rule lives here, where nothing can bypass it.
        //
        // The escape hatch for a genuinely empty file is clean and already
        // exists: delete the file, and the delete-sweep removes it normally.
        tracing::warn!(
            uri = %resource.uri,
            "resource produced no chunks — keeping any previously indexed \
             content for this URI (delete the source item if it is really gone)"
        );
        return Ok(IndexOutcome::Empty);
    }

    // Embed BEFORE any delete (A6) — see module doc comment above.
    // `document_context` is built from the resource's block texts in order
    // (the new `Resource` shape carries blocks, not a flat Markdown string).
    let document_context = resource
        .blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let doc_chunks = DocumentChunks {
        document_context,
        chunks: chunk_outputs.iter().map(|c| c.text.clone()).collect(),
    };

    let embedded = deps.embedder.embed_documents(vec![doc_chunks]).await?;

    // Guard: the embedder must return exactly one EmbeddedDocument (one per
    // input document), and that document must have exactly one vector per
    // chunk. A length mismatch indicates a malformed embedder response (F4).
    if embedded.len() != 1 {
        return Err(Error::Internal {
            message: format!(
                "embedder returned {} EmbeddedDocuments for 1 input document",
                embedded.len()
            ),
            correlation_id: "embed_count_mismatch".to_string(),
        });
    }
    let embeddings = &embedded[0];
    if embeddings.len() != chunk_outputs.len() {
        return Err(Error::Internal {
            message: format!(
                "embedder returned {} vectors for {} chunks",
                embeddings.len(),
                chunk_outputs.len()
            ),
            correlation_id: "embed_chunk_count_mismatch".to_string(),
        });
    }

    let provenance = Provenance {
        origin_store: deps.config.store_id.clone(),
        source_ref: SourceRef {
            id: resource.source_id.clone(),
            kind: resource.ingestor_kind.as_str().to_string(),
        },
        // Acquisition time, i.e. when *our store* got hold of this resource —
        // `added_at`, never `modified_at` (which for a feed entry is the
        // feed's own claim about the content's age). The libsql backend binds
        // this to `resources.added_at`, the column `MetadataFilter::
        // DateAfter`/`DateBefore` (`DateAxis::Added`) filter on and every
        // citation reports. See specs/02-domain-model.md §4.
        fetched_at: resource.added_at.clone(),
        content_hash: resource.content_hash.clone(),
        share_path: vec![],
    };

    // Post-backfill metadata, its persisted-state hash, and derived dates —
    // computed via the one function `on_resource`'s skip-check and
    // metadata-only-update path also call, so the two never disagree about
    // what a resource's persisted state is (issue #176). See
    // `DerivedResourceState`'s doc comment.
    let derived = derive_resource_state(resource);
    let record_metadata = derived.metadata;
    let date_original = derived.date_original;
    let date_parsed = derived.date_parsed;
    let modified_at = derived.modified_at;

    // Page lookup for paginated formats (#103): block seq → location.page,
    // copied onto each chunk record from its originating block.
    let page_by_seq: std::collections::HashMap<u32, u32> = resource
        .blocks
        .iter()
        .filter_map(|b| {
            b.location
                .as_ref()
                .and_then(|loc| loc.page)
                .map(|page| (b.seq, page))
        })
        .collect();

    let mut records = Vec::with_capacity(chunk_outputs.len());
    for (chunk_out, embedding) in chunk_outputs.iter().zip(embeddings.iter()) {
        let chunk = Chunk {
            id: chunk_out.id.clone(),
            resource_id: resource.id.clone(),
            store_id: deps.config.store_id.clone(),
            text: chunk_out.text.clone(),
            span: chunk_out.span.clone(),
            heading_path: chunk_out.heading_path.clone(),
            policy_version: deps.config.policy_version.clone(),
            provenance: provenance.clone(),
            window_block_seqs: chunk_out.window_block_seqs.clone(),
        };

        let mut record = ChunkRecord::from_chunk(
            &chunk,
            embedding.clone(),
            resource.uri.as_str().to_string(),
            resource.mime.clone(),
            record_metadata.clone(),
        );
        record.block_seq = chunk_out.block_seq;
        record.seq_in_block = chunk_out.seq_in_block;
        record.block_kind = chunk_out.block_kind.clone();
        record.page = page_by_seq.get(&chunk_out.block_seq).copied();
        // The resource's own claimed modification time — distinct from
        // `fetched_at`/`provenance.fetched_at` (acquisition time, stamped by
        // `from_chunk` above). Normalized (`Some("")` → `None`) via
        // `derive_resource_state`, so this always matches what
        // `derived.metadata_hash` was computed over. See
        // specs/02-domain-model.md §2.
        record.modified_at = modified_at.clone();
        record.date_original = date_original.clone();
        record.date_parsed = date_parsed.clone();
        record.external_id = resource.external_id.clone();
        record.external_etag = resource.external_etag.clone();
        records.push(record);
    }

    let written = records.len();
    deps.store
        .upsert_chunks_and_blocks(
            &deps.config.store_id,
            &resource.id,
            records,
            &resource.blocks,
            replaces_resource_id,
            resource.external_last_modified.as_deref(),
        )
        .await?;

    Ok(IndexOutcome::Written(written, derived.metadata_hash))
}

/// What [`index_resource`] did with a resource.
///
/// `Empty` is a distinct outcome rather than `Written(0)` because the caller
/// must treat it differently (#185): a resource that chunked to nothing is
/// *not* an indexed document, and recording it as one — bumping
/// `docs_indexed`, upserting its hash into the `DocumentIndex` — is what
/// turned "this file extracted to nothing" into "this file's indexed content
/// is gone."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOutcome {
    /// The resource was chunked, embedded, and written: chunk count, plus
    /// the `metadata_hash` this write persisted (issue #176) — threaded out
    /// rather than recomputed at the call site, so the value the caller
    /// stamps into its `DocumentIndex` is guaranteed to be exactly what this
    /// call persisted, not a second, separately-computed guess at it.
    Written(usize, String),
    /// The resource produced no chunks. Nothing was written, and — the
    /// invariant this type exists to carry — nothing was deleted either.
    Empty,
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

/// Why the delete-sweep (path/url) or the feed liveness sweep (feed) was
/// suppressed this run — guard 1 or guard 2, documented at the
/// `suppressed_because` computation in `run_source_ingestion` below.
///
/// Both guards suppress the presumed-gone sweep for path/url sources, and
/// both are logged at `warn!` there: either one means a run that should have
/// produced full evidence didn't, which is anomalous regardless of which
/// guard caught it.
///
/// The feed liveness sweep inherits only `IncompleteEnumeration`. The
/// zero-seen backstop is the routine steady state for a feed — the feed
/// document's own 304 short-circuit fires zero entry callbacks — and, more
/// importantly, absence there only decides who gets *probed*; the delete
/// still needs a confirmed 404/410. See the feed branch in
/// `run_source_ingestion` and specs/04-search-pipeline.md §1 "Guards".
enum SweepSuppression {
    /// Guard 1 — the ingestor itself reported it could not observe the
    /// source. Always anomalous.
    IncompleteEnumeration(String),
    /// Guard 2 — a run that claimed complete enumeration nonetheless
    /// observed none of the source's previously owned URIs.
    ZeroSeen,
}

impl SweepSuppression {
    /// The reason clause both suppression warnings interpolate.
    fn reason(&self) -> &str {
        match self {
            SweepSuppression::IncompleteEnumeration(reason) => reason,
            SweepSuppression::ZeroSeen => {
                "this run observed none of the documents this source owns"
            }
        }
    }
}

/// Run the unified ingestion pipeline for one source, driven by a caller-supplied
/// `&dyn Ingestor` (issue #117; specs/01-architecture.md §1).
///
/// Streams `Resource`s one at a time via [`PipelineCallback`] — no buffering of
/// an entire source's resources in memory. Per resource: skip-check (unchanged
/// `content_hash` + `policy_version`) → [`index_resource`] → counters/progress.
/// Per-resource errors become stats counters and progress events, never abort
/// the run.
///
/// # Removal
///
/// Nothing is ever removed unless `deps.deletion` is [`DeletionPolicy::Prune`]
/// — deletion is opt-in, like `rsync --delete`. A retaining run counts what it
/// would have removed into [`IngestionResult::docs_prunable`] instead.
///
/// Under `Prune`, two separate paths remove documents, and the difference
/// between them is the subject of issues #156/#185:
///
/// - **Confirmed gone.** A URI reported via `IngestCallback::on_gone` (an HTTP
///   404/410 after retry) is deleted unconditionally. The origin was reached
///   and answered — that is knowledge, so no guard applies.
/// - **Presumed gone (the delete-sweep).** A URI previously indexed for this
///   source that was neither yielded nor reported via `on_skipped` is
///   *inferred* to be gone. Because that is an inference from absence, it is
///   swept only when the absence is informative: feed sources are exempt
///   entirely (a feed is a bounded window), an incomplete enumeration
///   suppresses it (guard 1), and so does a run that observed none of the
///   source's own URIs (guard 2). See the comments at the sweep below.
pub async fn run_source_ingestion(
    source: &Source,
    ingestor: &dyn Ingestor,
    deps: SourceIngestionDeps<'_>,
) -> Result<IngestionResult, Error> {
    let SourceIngestionDeps {
        doc_index,
        store,
        embedder,
        config,
        progress,
        deletion,
        document_validators,
        stored_inputs_digest,
        fetcher,
    } = deps;

    // The origin's validator speaks for the origin's bytes. It cannot speak
    // for our indexing policy, whether we follow entry links, or how many
    // entries we take — so a change to any of those must not be allowed to
    // hide behind a 304 that skips the entry loop entirely. Compared here,
    // in `core`, rather than inside the feed ingestor: this mirrors
    // `PipelineCallback::lookup_fetch_metadata`'s `policy_version`
    // suppression one layer down, and keeps both halves of the same rule in
    // the same crate. See `specs/02-domain-model.md`'s Feed connector,
    // "Conditional GET and pruning".
    let document_inputs_digest = feed_inputs_digest(source, config);
    let document_validators = match &document_inputs_digest {
        Some(current) if stored_inputs_digest.as_deref() != Some(current.as_str()) => {
            tracing::debug!(
                source_id = %source.id,
                "feed inputs changed since the stored validators were captured; \
                 fetching the feed document unconditionally"
            );
            FetchMetadata::default()
        }
        _ => document_validators,
    };

    let ingest_config = serde_json::to_value(&source.spec).map_err(|e| Error::Internal {
        message: format!("failed to serialize source spec: {e}"),
        correlation_id: "source_spec_serialize".to_string(),
    })?;

    let ingest_source = IngestSource {
        source_id: source.id.clone(),
        store_id: source.store_id.clone(),
        ingestor_kind: ingestor.kind(),
        config: ingest_config,
        policy_version: config.policy_version.clone(),
        document_validators,
    };

    if let Some(sink) = &progress {
        sink(crate::progress::ProgressEvent::SourceStarted {
            source_id: source.id.clone(),
            location: source_location(source),
        });
    }

    let mut callback = PipelineCallback {
        source,
        doc_index,
        store,
        embedder,
        config,
        progress: progress.clone(),
        result: IngestionResult::default(),
        seen: std::collections::HashSet::new(),
        gone: std::collections::HashSet::new(),
        discovered_total: 0,
        next_index: 0,
        skip_error_count: 0,
    };

    let ingest_result = ingestor.ingest(&ingest_source, &mut callback).await?;

    let PipelineCallback {
        mut result,
        seen,
        gone,
        doc_index,
        skip_error_count,
        ..
    } = callback;

    // C8: `result.error_count` (below) is already authoritative — every
    // error path an ingestor takes must report `on_skipped(SkipReason::Error)`
    // exactly once (which increments `skip_error_count` here and
    // `result.error_count` above), and `PipelineCallback::on_resource`'s
    // `Err(e)` arm additionally counts `index_resource` failures the
    // ingestor never sees. So `ingest_result.errors` (the ingestor's own,
    // narrower self-report) is intentionally NOT folded into
    // `result.error_count` here — doing so would double-count every error
    // the ingestor already surfaced via `on_skipped`. It's used only as a
    // consistency check: a well-behaved ingestor's own error counter must
    // exactly match the number of `SkipReason::Error` skips it reported this
    // run. A mismatch means an ingestor bumped `IngestResult.errors` without
    // (or instead of) calling `on_skipped(Error)`, silently keeping a dead
    // URI alive in the sweep (or vice versa) — a bug in that ingestor, not
    // in the pipeline.
    debug_assert_eq!(
        ingest_result.errors, skip_error_count,
        "ingestor for source {} reported {} internal errors but only {} were \
         surfaced via on_skipped(SkipReason::Error) — every error path must \
         report exactly one SkipReason::Error skip",
        source.id, ingest_result.errors, skip_error_count
    );

    result.document_validators = ingest_result.document_validators.clone();
    result.document_inputs_digest = document_inputs_digest;

    // Confirmed deletions first, and unconditionally: a URI reported via
    // `on_gone` was positively established as absent at the origin (an HTTP
    // 404/410 after retry). That is *knowledge*, not inference — the origin
    // was reached and answered — so none of the sweep's guards below apply to
    // it, and neither does the feed exemption: a feed entry whose linked page
    // is confirmed 410 is gone whether or not the feed window still lists it.
    //
    // This is the distinction the rest of this function is organized around.
    // Knowing a resource is gone is not the same as failing to find it; only
    // the latter needs guarding, because only the latter is inferred.
    //
    // Both still answer to `deletion`, though: "the origin no longer has this"
    // is not the same as "you no longer want this." A local copy of a page
    // that has since 404'd is often the most valuable thing in the index.
    for uri in &gone {
        let owned_by_this_source = doc_index
            .get(uri)
            .is_some_and(|record| record.source_id == source.id);
        if !owned_by_this_source {
            continue;
        }
        if deletion == DeletionPolicy::Retain {
            result.docs_prunable += 1;
            continue;
        }
        if let Some(old_record) = doc_index.remove(uri) {
            let deleted = store.delete_by_resource(&old_record.resource_id).await?;
            if deleted > 0 {
                result.docs_deleted += 1;
            }
        }
    }

    // Delete-sweep: any URI known to this source's doc_index that was neither
    // yielded (on_resource) nor reported skipped (on_skipped) this run is
    // *presumed* gone — delete it. A deleted file simply isn't enumerated
    // again. Unlike the `on_gone` path above, this is an inference from
    // absence, which is why it is guarded.
    //
    // Ownership is decided by `source_id`, never by comparing URI strings
    // against the source's configured root/URL. The doc_index is store-wide,
    // and URI-shape heuristics misattribute rows across sources: a root that
    // is a string prefix of a sibling's (`/data/blog` vs `/data/blog-drafts`),
    // or percent-encoding twins (a literal `foo%23` directory vs a `foo#`
    // directory, whose canonical URIs are byte-identical), would let sweeping
    // one source delete another source's live documents. `source_id` is exact:
    // it is persisted per resource (baseline schema), rehydrated by
    // `list_indexed_documents`, and immune to encoding.
    // C1: feed sources are exempt from the delete-sweep. A feed only ever
    // exposes its most-recent N entries (an Atom/RSS document is a bounded
    // window, not a full archive listing) — an entry's absence from this
    // run means only "it scrolled off the feed," not "it was deleted at the
    // origin." Sweeping on that basis would delete everything the feed
    // previously contributed as soon as it aged out of the window, and a
    // feed-level 304 Not Modified (zero callbacks at all) would make the
    // sweep delete the *entire* source on every unchanged poll. Path and
    // url sources have no such windowing — their ingestor enumerates the
    // full current state every run — so absence there really does mean
    // deletion and the sweep must still run for them. This exemption is
    // unchanged by the feed liveness sweep below: that sweep only ever
    // deletes on a positively confirmed 404/410, never on absence alone, so
    // it belongs to a different bucket entirely (the same one the `on_gone`
    // loop above does) and doesn't need this guard reasoning to justify it.
    //
    // Ownership + guard evaluation below is shared by this sweep (path/url
    // sources) and the feed liveness sweep further down (feed sources):
    // both infer something from "this run observed none of the URIs this
    // source owns," so both need the same two guards computed from the same
    // owned/seen sets. Computed unconditionally rather than only inside the
    // `!Feed` branch — feed sources need it just as much; see the liveness
    // sweep's own guard handling below, in particular how it reconciles
    // guard 2 with the feed document's own 304 short-circuit.
    let owned_uris: Vec<String> = doc_index
        .uris()
        .into_iter()
        .filter(|uri| {
            doc_index
                .get(uri)
                .is_some_and(|record| record.source_id == source.id)
        })
        .collect();
    let any_owned_uri_seen = owned_uris.iter().any(|uri| seen.contains(uri));

    // Two further suppressions, both from issue #156, both stating the rule
    // the feed exemption above states for feeds: the sweep infers deletion
    // (or, for the liveness sweep, "eligible to probe") from absence, so it
    // may only run when the absence is *informative*.
    let suppressed_because = match &ingest_result.enumeration {
        // Guard 1 — enumeration completeness. The ingestor itself reported
        // that it could not observe the source (an unmounted volume, an
        // unreachable root, an API that failed part-way). Its silence
        // about a URI says nothing about whether that URI still exists.
        // This is the guard that fires for the reported incident:
        // `/Volumes/Archive` unmounted, `FileIngestor` enumerated zero
        // files, and the sweep deleted every document the source owned.
        Enumeration::Incomplete { reason } => {
            Some(SweepSuppression::IncompleteEnumeration(reason.clone()))
        }
        // Guard 2 — zero-seen backstop, source-shape-agnostic. Even with a
        // *complete* enumeration claimed, a source that previously owned
        // documents and observed none of them this run is far more likely
        // to be a broken connector than a source whose entire contents
        // vanished at once. This does not subsume guard 1: a connector
        // that enumerates 3 of 500 items before failing has a non-empty
        // `seen` set, so only guard 1 protects the other 497. For a feed
        // source specifically, this is also what reconciles the liveness
        // sweep with the feed document's own 304 short-circuit: a 304 fires
        // zero entry callbacks, so `seen` is empty and this guard fires —
        // correctly, since an unchanged feed document means an unchanged
        // window and nothing can have aged out since the last run.
        //
        // Deliberate trade-off: a source whose files really were all
        // deleted or renamed in one run keeps its stale documents until
        // the source is re-created. The warning below says so.
        Enumeration::Complete if !owned_uris.is_empty() && !any_owned_uri_seen => {
            Some(SweepSuppression::ZeroSeen)
        }
        Enumeration::Complete => None,
    };

    if !matches!(source.spec, SourceSpec::Feed { .. }) {
        if let Some(reason) = &suppressed_because {
            tracing::warn!(
                source_id = %source.id,
                location = %source_location(source),
                documents_preserved = owned_uris.len(),
                "skipping delete-sweep for source at '{}': {}. {} previously \
                 indexed document(s) were left in place rather than deleted, \
                 because this run produced no evidence that they are gone. If \
                 the source really is empty now, remove and re-add it \
                 (`localdb source remove` / `localdb source add`) and reindex.",
                source_location(source),
                reason.reason(),
                owned_uris.len(),
            );
        } else {
            for uri in owned_uris {
                if seen.contains(&uri) || gone.contains(&uri) {
                    continue;
                }
                if deletion == DeletionPolicy::Retain {
                    result.docs_prunable += 1;
                    continue;
                }
                if let Some(old_record) = doc_index.remove(&uri) {
                    let deleted = store.delete_by_resource(&old_record.resource_id).await?;
                    if deleted > 0 {
                        result.docs_deleted += 1;
                    }
                }
            }
        }
    } else if deletion == DeletionPolicy::Prune {
        // The feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out
        // feed entries: the liveness sweep"). Gated on `Prune` here, not
        // inside `run_feed_liveness_sweep` itself: a retaining run performs
        // zero liveness fetches and reports nothing pruned or prunable for
        // this mechanism — there is no free preview signal for it the way
        // `docs_prunable` is one for the presumed-gone sweep, since the only
        // way to learn anything here is a network request per candidate.
        // The liveness sweep inherits *one* of the two guards, not both
        // (specs/04-search-pipeline.md §1 "Guards"). The two sweeps read the
        // same seen-set for different purposes: the presumed-gone sweep
        // deletes on absence, so an untrustworthy seen-set is an
        // untrustworthy delete signal, while here absence only decides who
        // gets probed and the delete needs a confirmed 404/410 from the
        // origin.
        if let Some(reason @ SweepSuppression::IncompleteEnumeration(_)) = &suppressed_because {
            // Guard 1 stays anomalous for a feed exactly as it is for
            // path/url sources: the ingestor itself failed to observe the
            // source, so it knows nothing about which entries the window
            // holds. Every previously indexed URI would look aged out and the
            // source's whole document set would queue for probing, 25 per
            // run, off a signal already known to be broken.
            tracing::warn!(
                source_id = %source.id,
                location = %source_location(source),
                "skipping feed liveness sweep for source at '{}': {}. This is \
                 the same signal that suppresses the presumed-gone sweep for \
                 path/url sources above: a run that could not read the feed's \
                 window cannot tell an aged-out entry from one it simply never \
                 saw.",
                source_location(source),
                reason.reason(),
            );
        } else {
            // Guard 2 — the zero-seen backstop — deliberately does NOT
            // suppress this sweep, even though it is the routine steady state
            // for a feed under `--delete`: the feed document's own 304 fires
            // zero entry callbacks, so `seen` is empty on every quiet run.
            // Suppressing here starved the mechanism in precisely the case it
            // exists for — a feed goes quiet, every subsequent run 304s, and
            // the aged-out backlog is never probed again, this sweep being the
            // only thing that could ever shrink it.
            //
            // Running is safe because both bounds that make the sweep safe at
            // all are independent of the seen-set: it deletes only on a
            // confirmed 404/410, and it probes at most 25 candidates per run
            // per source, none more often than the recheck floor allows. An
            // empty seen-set subtracts nothing from the candidate list, which
            // is the right answer for a 304'd run — the window is unchanged,
            // so nothing aged out *during* this run, and every candidate the
            // query returns had already aged out before it began.
            let refresh_interval_secs = match &source.spec {
                SourceSpec::Feed {
                    refresh_interval_secs,
                    ..
                } => *refresh_interval_secs,
                _ => None,
            };
            run_feed_liveness_sweep(
                &source.id,
                &source.store_id,
                refresh_interval_secs,
                &seen,
                doc_index,
                store,
                fetcher,
                &mut result,
            )
            .await?;
        }
    }

    if let Some(sink) = &progress {
        sink(crate::progress::ProgressEvent::SourceFinished {
            result: result.clone(),
        });
    }

    Ok(result)
}

/// The local-inputs digest for `source` under `config`, or `None` when the
/// source has no feed document whose fetch could be made conditional.
///
/// The `None` return is what keeps every non-feed source out of the gate
/// entirely: with no digest there is nothing to compare, nothing to
/// suppress, and nothing to persist.
fn feed_inputs_digest(source: &Source, config: &IngestionConfig) -> Option<String> {
    match &source.spec {
        crate::types::SourceSpec::Feed {
            max_entries,
            fetch_full_content,
            ..
        } => Some(crate::ids::compute_feed_inputs_digest(
            &config.policy_version,
            *fetch_full_content,
            *max_entries,
        )),
        _ => None,
    }
}

/// Batch cap for the feed liveness sweep: at most this many aged-out feed
/// entries are probed per source per run. See
/// specs/04-search-pipeline.md §1 "Aged-out feed entries: the liveness
/// sweep".
const FEED_LIVENESS_BATCH_LIMIT: usize = 25;

/// Recheck floor for the feed liveness sweep, in seconds: a candidate is
/// never re-probed more often than this, however long it has been aged out.
/// A feed source's own `refresh_interval_secs` raises the effective floor
/// when configured above this; the common unconfigured case uses this bare
/// value.
const FEED_LIVENESS_MIN_RECHECK_SECS: i64 = 24 * 60 * 60;

/// Ceiling on how far the candidate query may over-fetch to compensate for
/// the seen-set it cannot see (see [`run_feed_liveness_sweep`]). Chosen well
/// above any realistic feed window, and fixed rather than derived, because
/// `max_entries` is optional and defaults to unbounded — the seen-set has no
/// principled size of its own to scale by.
const FEED_LIVENESS_OVERFETCH_CAP: usize = 500;

/// The feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out feed
/// entries: the liveness sweep"). For a `SourceSpec::Feed` source, probes a
/// bounded batch of feed-discovered resources this run did not observe —
/// entries aged out of the feed's window — against their stored link, and
/// deletes only the ones a probe *positively confirms* gone (404/410).
///
/// This sits in the confirmed-gone bucket alongside `IngestCallback::on_gone`
/// above, not the presumed-gone one: it never deletes on absence alone, only
/// on the origin's own answer. That is what lets it coexist with the feed
/// exemption from the presumed-gone sweep above without contradicting it —
/// an entry merely scrolling off the window is still never, on its own, a
/// deletion signal; only a confirmed 404/410 on its own link is.
///
/// Callers must suppress this on an `Enumeration::Incomplete` run — a run
/// that could not read the feed's window cannot tell an aged-out entry from
/// one it never saw. A *zero-seen* run, by contrast, must still reach this
/// function: that is the routine feed-304 case, and suppressing it starves
/// the sweep exactly when a feed goes quiet (specs/04-search-pipeline.md §1
/// "Guards"). See the call site in `run_source_ingestion`. This function
/// performs no guard check of its own; it probes whatever
/// [`RetrievalStore::list_stale_feed_resources`] returns, minus `seen`, up to
/// [`FEED_LIVENESS_BATCH_LIMIT`].
#[allow(clippy::too_many_arguments)]
async fn run_feed_liveness_sweep(
    source_id: &str,
    store_id: &str,
    refresh_interval_secs: Option<u64>,
    seen: &std::collections::HashSet<String>,
    doc_index: &mut DocumentIndex,
    store: &dyn RetrievalStore,
    fetcher: &dyn UrlFetcher,
    result: &mut IngestionResult,
) -> Result<(), Error> {
    let floor_secs = refresh_interval_secs
        .unwrap_or(0)
        .max(FEED_LIVENESS_MIN_RECHECK_SECS as u64);
    // `refresh_interval_secs` is an unvalidated `u64` from config (no upper
    // bound is enforced in `core::config::refresh::validate_refresh_interval`),
    // so it must not be cast with `as i64`: a value above `i64::MAX` wraps
    // negative, pushing `checked_before` into the future and making every
    // resource a candidate — the opposite of this floor's purpose. Saturate
    // every step instead of only the cast: `chrono::Duration::seconds` itself
    // panics above `i64::MAX / 1_000`, and subtracting from `Utc::now()` can
    // in principle underflow past the representable range.
    let floor_secs_i64 = i64::try_from(floor_secs).unwrap_or(i64::MAX);
    let recheck_window =
        chrono::Duration::try_seconds(floor_secs_i64).unwrap_or(chrono::Duration::MAX);
    let checked_before = Utc::now()
        .checked_sub_signed(recheck_window)
        .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC)
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    // The batch cap counts candidates actually *probed*, not rows returned.
    // The query orders oldest-`last_checked_at` first and knows nothing
    // about this run's in-memory seen-set, so a run whose freshly-observed
    // entries happen to sort oldest would fill a SQL-side `LIMIT 25`
    // entirely with entries it had just seen and probe nothing at all —
    // permanently, for a feed whose whole window sorts that way. Over-fetch
    // by the seen-set's size, subtract it here, then take the real 25.
    let query_limit = FEED_LIVENESS_BATCH_LIMIT
        .saturating_add(seen.len())
        .min(FEED_LIVENESS_OVERFETCH_CAP);
    let candidates: Vec<StaleFeedResource> = store
        .list_stale_feed_resources(store_id, source_id, &checked_before, query_limit)
        .await?;

    let mut ctx = LivenessProbeContext {
        store_id,
        doc_index,
        store,
        fetcher,
        result,
    };
    for candidate in candidates
        .into_iter()
        // Still inside the feed's window — this run's own ingestion pass
        // already observed it, so it hasn't aged out at all. Reachable when
        // a currently-live entry's `last_checked_at` happens to be unset or
        // stale (it has simply never been probed before, or was probed long
        // ago while still current); probing it here would be redundant with
        // the ordinary ingestion pass that already just ran for it.
        .filter(|candidate| !seen.contains(&candidate.uri))
        .take(FEED_LIVENESS_BATCH_LIMIT)
    {
        probe_liveness_candidate(&mut ctx, candidate).await?;
    }

    Ok(())
}

/// Everything [`probe_liveness_candidate`] needs beyond the candidate
/// itself, bundled so splitting the sweep in two moves the parameter list
/// rather than duplicating it.
struct LivenessProbeContext<'a> {
    store_id: &'a str,
    doc_index: &'a mut DocumentIndex,
    store: &'a dyn RetrievalStore,
    fetcher: &'a dyn UrlFetcher,
    result: &'a mut IngestionResult,
}

/// Probe one aged-out feed entry and record the outcome.
///
/// Every outcome except a confirmed 404/410 converges on a single
/// `touch_resource_liveness` call; the only thing an outcome decides is
/// which validators go with it. `last_checked_at` therefore advances on
/// **every attempt** — the normative meaning of that column
/// (specs/04-search-pipeline.md §1). It has to: the candidate query is
/// oldest-first, so leaving an unreachable candidate's timestamp where it
/// was would put it back at the head of the next query, and a source with
/// `FEED_LIVENESS_BATCH_LIMIT` or more permanently-blocked entries would
/// re-probe that same stuck set forever while no other candidate ever
/// reached a batch.
///
/// `Err` is returned only for a failure that makes continuing the sweep
/// meaningless (a failed delete); per-candidate write failures are logged
/// and skipped, so one racing delete cannot discard the stats already
/// computed for the candidates processed alongside it.
async fn probe_liveness_candidate(
    ctx: &mut LivenessProbeContext<'_>,
    candidate: StaleFeedResource,
) -> Result<(), Error> {
    let stored = FetchMetadata {
        etag: candidate.external_etag.clone(),
        last_modified: candidate.external_last_modified.clone(),
    };

    // Counted for every candidate an attempt is made for, regardless of
    // outcome — see `IngestionResult::feed_entries_liveness_checked`'s
    // doc comment.
    ctx.result.feed_entries_liveness_checked += 1;

    let refreshed = match ctx.fetcher.fetch(&candidate.uri, &stored).await {
        Err(e) => {
            // A transport error is evidence of nothing about the entry, so
            // nothing about its content, metadata or validators moves — only
            // the clock, so the rotation stays fair.
            tracing::debug!(
                uri = %candidate.uri,
                error = %e,
                "feed liveness sweep: fetch error, advancing only the probe clock"
            );
            stored
        }
        Ok(FetchResult::Gone) => {
            ctx.doc_index.remove(&candidate.uri);
            let deleted = ctx.store.delete_by_resource(&candidate.resource_id).await?;
            if deleted > 0 {
                ctx.result.docs_deleted += 1;
            }
            return Ok(());
        }
        Ok(FetchResult::NotModified {
            etag,
            last_modified,
        }) => {
            // A bare 304 means unchanged; fold any rotated validator over
            // what was already stored rather than reporting a partial value
            // that would read as "clear it" — mirrors the feed document's
            // own 304 handling (`ingest::FeedIngestor::ingest`). Still
            // there, so not a delete.
            FetchMetadata {
                etag: etag.or(stored.etag),
                last_modified: last_modified.or(stored.last_modified),
            }
        }
        Ok(FetchResult::Downloaded { .. }) => {
            // The body is deliberately discarded: an aged-out entry's
            // feed-sourced metadata (title, author, per-entry date) is long
            // gone, and re-indexing from the bare page alone would silently
            // degrade the stored resource rather than improve it.
            //
            // The response's *validators* are discarded with it, and that is
            // the whole point. Storing them would leave the resource
            // pointing at a representation this store never indexed: the
            // next probe would answer 304, and if the entry ever re-entered
            // the feed window that 304 would suppress the reindex of the
            // changed content indefinitely. Replaying the stored validators
            // instead costs a full 200 at every recheck on an entry that
            // genuinely changed — pure overhead on something the sweep does
            // not re-index anyway, and the correct side to err on.
            stored
        }
        Ok(FetchResult::Blocked) => {
            // Neither evidence of anything, like a transport error — only
            // the clock moves. This arm is reachable in production:
            // `fetcher` here is the public-only client (see
            // `SourceIngestionDeps::fetcher`), and a real entry link that
            // resolves to a non-globally-routable address (an internal/LAN
            // link — specs/02-domain-model.md's "Destination policy (entry
            // links)") is refused exactly like it would be on the ordinary
            // ingestion pass. A fragment URI (a link-less entry's synthetic
            // `{feed_url}#entry:{id}`) can no longer drive this arm — it
            // never becomes a candidate in the first place
            // (`RetrievalStore::list_stale_feed_resources`).
            stored
        }
    };

    // A per-candidate failure here (e.g. a concurrent delete racing this
    // probe) must not abort the whole source and discard the run's
    // already-computed stats for the candidates already processed: log and
    // move on to the next candidate.
    if let Err(e) = ctx
        .store
        .touch_resource_liveness(
            ctx.store_id,
            &candidate.resource_id,
            refreshed.etag.as_deref(),
            refreshed.last_modified.as_deref(),
        )
        .await
    {
        tracing::debug!(
            uri = %candidate.uri,
            error = %e,
            "feed liveness sweep: touch_resource_liveness failed, leaving \
             resource untouched"
        );
    }

    Ok(())
}

/// Human-readable "location" string for `ProgressEvent::SourceStarted`.
fn source_location(source: &Source) -> String {
    match &source.spec {
        SourceSpec::Path { root, .. } => root.clone(),
        SourceSpec::Url { url, .. } => url.clone(),
        SourceSpec::Feed { url, .. } => url.clone(),
    }
}

/// `IngestCallback` implementation that drives the unified pipeline one
/// `Resource` at a time.
///
/// # The `&mut DocumentIndex`-across-`await` design
///
/// `PipelineCallback` OWNS its dependency references (including
/// `doc_index: &'a mut DocumentIndex`) as plain struct fields rather than
/// threading them through method parameters. `#[async_trait]` desugars
/// `on_resource`/`on_discovered`/`on_skipped` into methods returning
/// `Pin<Box<dyn Future<Output = ...> + Send + 'async_trait>>` tied to
/// `&'async_trait mut self`. Since the mutable borrow of `DocumentIndex` lives
/// entirely *inside* that per-call future (never held across separate calls,
/// never stored anywhere else), there is no conflict: each call reborrows
/// `self.doc_index` for its own duration and releases it when the future
/// resolves — ordinary NLL reborrowing, not a lifetime fight. `run_source_ingestion`
/// hands `PipelineCallback` its own `&mut DocumentIndex` (from
/// `SourceIngestionDeps`) for the lifetime of the `ingestor.ingest(...)` call
/// only; once that call returns, `callback` is destructured and `doc_index` is
/// used directly again for the delete-sweep. No interior mutability
/// (`RefCell`/`Mutex`) is needed — the fix for the "known risk" flagged for
/// this ticket was simply to give the callback ownership of the dependency
/// *references* up front, rather than threading `&mut DocumentIndex` through a
/// chain of function parameters that would each need to re-borrow it across an
/// `.await` point.
struct PipelineCallback<'a> {
    source: &'a Source,
    doc_index: &'a mut DocumentIndex,
    store: &'a dyn RetrievalStore,
    embedder: &'a dyn Embedder,
    config: &'a IngestionConfig,
    progress: Option<crate::progress::ProgressSink>,
    result: IngestionResult,
    /// URIs yielded or reported skipped this run — survive the delete-sweep.
    seen: std::collections::HashSet<String>,
    /// URIs the ingestor positively confirmed gone at the origin (404/410
    /// after retry). Deleted unconditionally — see `IngestCallback::on_gone`.
    gone: std::collections::HashSet<String>,
    /// Last total reported via `on_discovered`, if any (0 until then).
    discovered_total: usize,
    /// Running index for `ProgressEvent::DocumentStarted`.
    next_index: usize,
    /// Count of `on_skipped(SkipReason::Error(_))` calls this run — used
    /// only to cross-check the ingestor's own `IngestResult.errors` in
    /// `run_source_ingestion` (see the debug_assert there); NOT folded into
    /// `result.error_count` twice.
    skip_error_count: usize,
}

impl PipelineCallback<'_> {
    fn emit(&self, event: crate::progress::ProgressEvent) {
        if let Some(sink) = &self.progress {
            sink(event);
        }
    }

    fn start_document(&mut self, uri: &str) {
        let index = self.next_index;
        self.next_index += 1;
        self.emit(crate::progress::ProgressEvent::DocumentStarted {
            uri: uri.to_string(),
            index,
            total: self.discovered_total,
        });
    }
}

#[async_trait::async_trait]
impl IngestCallback for PipelineCallback<'_> {
    async fn on_resource(&mut self, resource: Resource) -> Result<(), Error> {
        let uri = resource.uri.as_str().to_string();
        self.seen.insert(uri.clone());
        self.result.docs_seen += 1;
        self.start_document(&uri);

        // Skip-check: unchanged content_hash + same policy_version → either
        // an unchanged-metadata skip or a metadata-only update, decided by
        // `metadata_hash` (issue #176). Ingestors may ALSO skip earlier via
        // `on_skipped`; every path here marks the URI seen so the
        // delete-sweep leaves it alone.
        if let Some(existing) = self.doc_index.get(&uri) {
            if existing.content_hash == resource.content_hash
                && existing.policy_version == self.config.policy_version
            {
                // Computed once here — the sole use of `derive_resource_state`
                // on this branch, since neither arm below calls
                // `index_resource` (which would otherwise duplicate it).
                let derived = derive_resource_state(&resource);

                // `external_last_modified` is compared on its own because
                // it is deliberately not one of `compute_metadata_hash`'s
                // inputs (specs/02-domain-model.md §2), so a hash comparison
                // alone cannot see it move. A `Last-Modified`-only origin
                // rotates exactly that field on an unchanged 200: without
                // this the skip returns before the write, the stored
                // validator stays at whatever the first run captured, and
                // every run after replays an `If-Modified-Since` the origin
                // has already moved past — a full re-download, every run,
                // forever. The write is the metadata-only branch below,
                // which already persists the field. Same reasoning
                // `on_validators_refreshed` gives for comparing the
                // validator pair rather than the hash.
                if existing.metadata_hash == derived.metadata_hash
                    && existing.external_last_modified == resource.external_last_modified
                {
                    self.result.docs_skipped += 1;
                    self.emit(crate::progress::ProgressEvent::DocumentFinished {
                        uri,
                        outcome: crate::progress::DocOutcome::Skipped,
                    });
                    return Ok(());
                }

                // Content and policy are unchanged, but persisted metadata
                // differs: rewrite the resource row in place, no
                // chunks/blocks/embeddings touched.
                let resource_id = existing.resource_id.clone();
                let record = crate::store::ResourceRecord {
                    metadata: derived.metadata,
                    external_id: resource.external_id.clone(),
                    external_etag: resource.external_etag.clone(),
                    external_last_modified: resource.external_last_modified.clone(),
                    modified_at: derived.modified_at,
                    date_original: derived.date_original,
                    date_parsed: derived.date_parsed,
                };
                // Per-resource errors never abort the run (specs/04 §2),
                // mirroring the full-reindex error arm below: a metadata-only
                // write failure counts as an error and processing continues.
                // doc_index is deliberately left untouched — the stale hash
                // makes this resource retry the metadata write on the next
                // run, exactly like a failed full reindex retries.
                if let Err(e) = self
                    .store
                    .update_resource_metadata(&self.config.store_id, &resource_id, &record)
                    .await
                {
                    tracing::warn!("error updating metadata for resource '{}': {}", uri, e);
                    self.result.error_count += 1;
                    self.emit(crate::progress::ProgressEvent::DocumentFinished {
                        uri,
                        outcome: crate::progress::DocOutcome::Error,
                    });
                    return Ok(());
                }
                self.doc_index.upsert(DocumentRecord {
                    uri: uri.clone(),
                    resource_id,
                    source_id: existing.source_id.clone(),
                    content_hash: existing.content_hash.clone(),
                    policy_version: existing.policy_version.clone(),
                    metadata_hash: derived.metadata_hash,
                    external_etag: resource.external_etag.clone(),
                    external_last_modified: resource.external_last_modified.clone(),
                });
                self.result.docs_metadata_updated += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::MetadataUpdated,
                });
                return Ok(());
            }
        }

        let replaces = self.doc_index.get(&uri).map(|e| e.resource_id.clone());

        let deps = IndexResourceDeps {
            store: self.store,
            embedder: self.embedder,
            config: self.config,
        };

        match index_resource(&resource, self.source, replaces.as_deref(), &deps).await {
            Ok(IndexOutcome::Empty) => {
                // #185: the resource chunked to nothing, so `index_resource`
                // wrote nothing and deleted nothing. Count it as a skip, not
                // as an indexed document.
                //
                // `doc_index` is deliberately left UNTOUCHED. Upserting the
                // empty resource's id/hash here would point the index at a
                // resource_id the store has no rows for (the store still holds
                // the *old* resource), which would make the next run's
                // skip-check compare against a phantom and leave the real rows
                // unreachable. The URI is already in `seen` (inserted at the
                // top of this method), so it survives the delete-sweep.
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::Skipped,
                });
            }
            Ok(IndexOutcome::Written(chunks_written, metadata_hash)) => {
                self.result.docs_indexed += 1;
                self.result.chunks_written += chunks_written as u64;
                self.doc_index.upsert(DocumentRecord {
                    uri: uri.clone(),
                    resource_id: resource.id.clone(),
                    source_id: resource.source_id.clone(),
                    content_hash: resource.content_hash.clone(),
                    policy_version: self.config.policy_version.clone(),
                    metadata_hash,
                    external_etag: resource.external_etag.clone(),
                    external_last_modified: resource.external_last_modified.clone(),
                });
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::Indexed {
                        chunks: chunks_written,
                    },
                });
            }
            Err(e) => {
                // Per-resource errors never abort the run (specs/04 §2).
                // doc_index is deliberately left untouched so a later run
                // retries.
                tracing::warn!("error indexing resource '{}': {}", uri, e);
                self.result.error_count += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::Error,
                });
            }
        }

        Ok(())
    }

    async fn on_discovered(&mut self, total: usize) {
        self.discovered_total = total;
        self.emit(crate::progress::ProgressEvent::Discovered { total });
    }

    async fn on_gone(&mut self, uri: &Uri) {
        // Positively confirmed absent at the origin. Recorded rather than
        // deleted here so that all deletion happens in one place in
        // `run_source_ingestion` — but unlike the sweep's inferred deletions,
        // this one is exempt from every guard: the ingestor didn't fail to see
        // it, the origin told us it's gone.
        //
        // Deliberately NOT added to `seen`: `seen` means "still alive, don't
        // sweep", which is the opposite of what this signal says.
        self.gone.insert(uri.as_str().to_string());
    }

    async fn on_skipped(&mut self, uri: &Uri, reason: SkipReason) {
        // `uri` is already canonical by construction (see `Ingestor::on_skipped`'s
        // doc comment) — no normalization step belongs here.
        let uri = uri.as_str();
        self.seen.insert(uri.to_string());
        self.result.docs_seen += 1;
        self.start_document(uri);

        match reason {
            SkipReason::Unchanged => {
                // Still alive, just unchanged — never re-index, never sweep.
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Skipped,
                });
            }
            SkipReason::Unsupported => {
                // An unsupported file is counted but never deleted — it stays
                // "seen" so any previously-indexed
                // content for it (from before it became unsupported) survives
                // the sweep untouched, neither refreshed nor removed.
                self.result.unsupported_format_count += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Unsupported,
                });
            }
            SkipReason::Other(_) => {
                // No direct old-path analog; nearest classification is a
                // (non-format, non-error) skip. Alive either way (marked seen
                // above), so it survives the sweep regardless.
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Skipped,
                });
            }
            SkipReason::MetadataUpdated => {
                // Not a skip: the resource row was rewritten in place. Mirrors
                // `on_resource`'s metadata-only branch exactly — same counter,
                // same progress outcome — so a metadata write reads the same
                // whether it arrived with a body or behind a 304.
                self.result.docs_metadata_updated += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::MetadataUpdated,
                });
            }
            SkipReason::Error(ref msg) => {
                // C7/C8: processing failed but the item still exists — count
                // it as an error (not a benign skip) so the CLI summary and
                // IngestionResult.error_count reflect it accurately. Still
                // marked "seen" above, so it keeps its URI alive across the
                // delete-sweep exactly like Unchanged/Other/Unsupported do —
                // a transient failure must never look like the resource is
                // gone.
                tracing::warn!("error processing '{}': {}", uri, msg);
                self.result.error_count += 1;
                self.skip_error_count += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Error,
                });
            }
        }
    }

    /// The suppression rule (normative) — specs/04-search-pipeline.md §1.
    ///
    /// Conditional headers are sent only when the stored resource's
    /// `policy_version` equals this run's. A 304 returns no bytes, so a
    /// resource that needs re-chunking under a changed policy could never be
    /// re-chunked if it were allowed to answer 304 — the document would be
    /// silently frozen at the old policy forever. This reuses the exact
    /// signal `on_resource`'s own skip-check gates on a few dozen lines
    /// above (`existing.policy_version == self.config.policy_version`), so
    /// the two checks can never drift apart into disagreeing about whether a
    /// resource is "current."
    ///
    /// **This is the designated join point.** Any future axis able to force
    /// reprocessing without a content change (a real `extractor_version`
    /// bump, a `Resource.mime` reclassification) must gate its own replay
    /// suppression through this same check — bypassing it reintroduces
    /// exactly the bug this comment describes, just under a different name.
    async fn lookup_fetch_metadata(&mut self, uri: &Uri) -> FetchMetadata {
        let Some(existing) = self.doc_index.get(uri.as_str()) else {
            return FetchMetadata::default();
        };
        if existing.policy_version != self.config.policy_version {
            return FetchMetadata::default();
        }
        FetchMetadata {
            etag: existing.external_etag.clone(),
            last_modified: existing.external_last_modified.clone(),
        }
    }

    async fn on_validators_refreshed(
        &mut self,
        uri: &Uri,
        meta: &FetchMetadata,
    ) -> MetadataWriteOutcome {
        // A bare 304 (both `None`) means "keep whatever is already stored"
        // — never "clear it" (see `FetchResult::NotModified`'s doc comment).
        // Nothing to persist.
        if meta.etag.is_none() && meta.last_modified.is_none() {
            return MetadataWriteOutcome::Unchanged;
        }
        let uri_str = uri.as_str().to_string();
        let Some(existing) = self.doc_index.get(&uri_str).cloned() else {
            // Nothing indexed for this URI under the current doc_index (a
            // concurrent delete raced this fetch, or the liveness sweep is
            // probing a URI this run's doc_index never loaded) — nothing to
            // refresh.
            return MetadataWriteOutcome::Unchanged;
        };

        let new_etag = meta.etag.clone().or_else(|| existing.external_etag.clone());
        let new_last_modified = meta
            .last_modified
            .clone()
            .or_else(|| existing.external_last_modified.clone());

        // An origin is free to repeat the validator it already issued, and a
        // well-behaved one does exactly that on every 304 for unchanged
        // content — so this is the common case, not an edge. Writing anyway
        // would rewrite the resource row and bump `index_updated_at` (public
        // as `DocumentInfo.index_updated_at`) on a run that changed nothing.
        //
        // The comparison is on the validator pair itself, not on
        // `compute_metadata_hash` as `on_metadata_refreshed` below uses:
        // `external_last_modified` is deliberately not one of that hash's
        // inputs (specs/02-domain-model.md §2), so a 304 rotating only
        // `Last-Modified` produces an identical hash while still needing to
        // be persisted. A hash guard here would silently drop it.
        if new_etag == existing.external_etag
            && new_last_modified == existing.external_last_modified
        {
            return MetadataWriteOutcome::Unchanged;
        }

        // RFC 9111 requires storing whichever validator(s) the 304 itself
        // carried, but content and the resource's own metadata are
        // unchanged by definition (a 304 has no body) — this never triggers
        // a re-chunk or re-embed, and it never goes through `index_resource`
        // or `on_resource`'s own metadata-only branch (neither has a
        // `Resource` to work from here, only a `Uri` and a `FetchMetadata`).
        //
        // The subtle part: `external_etag` IS an input to
        // `compute_metadata_hash`, but `external_last_modified` deliberately
        // is not (specs/02-domain-model.md §2). `resources.external_etag` is
        // about to change, and `list_indexed_documents` recomputes
        // `metadata_hash` straight from that same column on every
        // rehydration — so leaving the *cached* `metadata_hash` in
        // `doc_index` unrefreshed would desync it from what a fresh
        // rehydration computes for the same row. The next metadata-unchanged
        // fetch for this URI would then see a spurious `metadata_hash`
        // mismatch and route through a needless metadata-only update purely
        // to correct a staleness this method introduced. So this recomputes
        // and re-caches `metadata_hash` in lockstep with the rotated
        // validator, over the resource's current persisted state —
        // `update_resource_metadata` rewrites every column of that state, so
        // every column it does not mean to change has to be read back first,
        // and `DocumentRecord` carries only the hash, not what was hashed.
        let resource_id = existing.resource_id.clone();
        let persisted = match self
            .store
            .get_resource_record(&self.config.store_id, &resource_id)
            .await
        {
            Ok(Some(record)) => record,
            // No row: a concurrent delete, same race the `doc_index` miss
            // above tolerates.
            Ok(None) => return MetadataWriteOutcome::Unchanged,
            Err(e) => {
                let msg = format!(
                    "error reading resource '{uri_str}' to refresh conditional-GET validators: {e}"
                );
                tracing::warn!("{msg}");
                return MetadataWriteOutcome::Failed(msg);
            }
        };

        let record = crate::store::ResourceRecord {
            external_etag: new_etag.clone(),
            external_last_modified: new_last_modified.clone(),
            ..persisted
        };
        let metadata_hash = crate::ids::compute_metadata_hash(
            &record.metadata,
            record.external_id.as_deref(),
            new_etag.as_deref(),
            record.modified_at.as_deref(),
        );

        self.persist_metadata_write(
            &uri_str,
            &resource_id,
            &record,
            DocumentRecord {
                metadata_hash,
                external_etag: new_etag,
                external_last_modified: new_last_modified,
                ..existing
            },
            "refreshed conditional-GET validators",
        )
        .await
    }

    /// A 304 carries no body, so the connector's own metadata for the
    /// resource — which it re-supplies on every run, independently of the
    /// fetch — is the only thing that can have changed. Layer it back onto
    /// the persisted state and write only if the result actually differs.
    ///
    /// The comparison is the point, not an optimization: an unchanged feed
    /// entry is the overwhelmingly common case, and a blind write would turn
    /// every 304 into a resource-row rewrite, bumping `index_updated_at`
    /// (publicly visible as `DocumentInfo.index_updated_at`) on a run that
    /// changed nothing. So this recomputes `metadata_hash` from the merged
    /// state and returns early when it matches what is already cached —
    /// exactly the equality the skip-check in `on_resource` performs, on the
    /// same derivation, for the same reason.
    ///
    /// The merge runs against the *persisted* metadata rather than a fresh
    /// parse, because there is no fresh parse to run. One consequence
    /// follows from `MetadataEnrichment`'s title rule and is intended: a
    /// connector title only fills a gap, so a feed that renames an entry
    /// whose linked page supplied its own title changes nothing here — which
    /// is what a full re-fetch would conclude too, since the page's title
    /// would win again. Where the two paths do differ is the rarer case of a
    /// page with no title of its own: the persisted title is then the
    /// connector's previous one, no longer a gap, so a renamed entry keeps
    /// the old title until its content changes. Erring toward keeping
    /// extracted state is the safe direction; the overwrite-class fields
    /// (`creator`, `date`, provenance, `external_id`, `modified_at`), where
    /// staleness actually costs something, take the connector's current claim
    /// — including the absence of one. A connector that stops claiming a
    /// `date` it previously stamped retracts it (`MetadataEnrichment::
    /// apply_to`), and `external_id`/`modified_at` are authoritative as
    /// passed, `None` included. A connector that stops claiming a `creator`
    /// is the one case that does not retract: `creator` carries no
    /// provenance stamp, so there is no way to tell the connector's own
    /// previous value from the extraction's.
    async fn on_metadata_refreshed(
        &mut self,
        uri: &Uri,
        enrichment: &crate::metadata::MetadataEnrichment,
        external_id: Option<&str>,
        modified_at: Option<&str>,
    ) -> MetadataWriteOutcome {
        let uri_str = uri.as_str().to_string();
        let Some(existing) = self.doc_index.get(&uri_str).cloned() else {
            // Nothing indexed for this URI under the current doc_index —
            // same race `on_validators_refreshed` tolerates.
            return MetadataWriteOutcome::Unchanged;
        };

        let resource_id = existing.resource_id.clone();
        let persisted = match self
            .store
            .get_resource_record(&self.config.store_id, &resource_id)
            .await
        {
            Ok(Some(record)) => record,
            Ok(None) => return MetadataWriteOutcome::Unchanged,
            Err(e) => {
                let msg = format!(
                    "error reading resource '{uri_str}' to refresh source-supplied metadata: {e}"
                );
                tracing::warn!("{msg}");
                return MetadataWriteOutcome::Failed(msg);
            }
        };

        let mut metadata = persisted.metadata;
        enrichment.apply_to(metadata.dublin_core_mut());
        // `date_original`/`date_parsed` are projections of the merged
        // `dc.date`, re-derived here rather than carried over from the
        // persisted record — the enrichment may have just replaced (or
        // retracted) that date, and the two columns are what the `document`
        // date axis filters on.
        let date_original = metadata.dublin_core().date.clone();
        let date_parsed = date_original
            .as_deref()
            .and_then(crate::dates::parse_partial_iso8601);
        let external_id = external_id.map(str::to_string);
        let modified_at = modified_at.map(str::to_string);

        let metadata_hash = crate::ids::compute_metadata_hash(
            &metadata,
            external_id.as_deref(),
            existing.external_etag.as_deref(),
            modified_at.as_deref(),
        );
        if metadata_hash == existing.metadata_hash {
            return MetadataWriteOutcome::Unchanged;
        }

        let record = crate::store::ResourceRecord {
            metadata,
            external_id,
            external_etag: existing.external_etag.clone(),
            external_last_modified: existing.external_last_modified.clone(),
            modified_at,
            date_original,
            date_parsed,
        };

        self.persist_metadata_write(
            &uri_str,
            &resource_id,
            &record,
            DocumentRecord {
                metadata_hash,
                ..existing
            },
            "refreshed source-supplied metadata",
        )
        .await
    }
}

impl PipelineCallback<'_> {
    /// The write tail both metadata-refresh hooks share: persist the record,
    /// and on success cache the `DocumentRecord` the hook derived for it.
    ///
    /// Only the tail is shared. Each hook keeps its own read, its own
    /// derivation and its own unchanged-condition, because those genuinely
    /// differ — one folds a validator pair and compares it directly, the
    /// other merges a connector's claim and compares a metadata hash. `what`
    /// names the refresh in the warning; it selects no behavior.
    ///
    /// A failed write leaves `doc_index` untouched on purpose, exactly like
    /// `on_resource`'s metadata-only branch: the stale cached hash is what
    /// makes the next run retry the write.
    async fn persist_metadata_write(
        &mut self,
        uri: &str,
        resource_id: &str,
        record: &crate::store::ResourceRecord,
        updated: DocumentRecord,
        what: &str,
    ) -> MetadataWriteOutcome {
        if let Err(e) = self
            .store
            .update_resource_metadata(&self.config.store_id, resource_id, record)
            .await
        {
            let msg = format!("error persisting {what} for '{uri}': {e}");
            tracing::warn!("{msg}");
            return MetadataWriteOutcome::Failed(msg);
        }
        self.doc_index.upsert(updated);
        MetadataWriteOutcome::Written
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
