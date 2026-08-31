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
/// The two guards are logged at the same level for path/url sources: either
/// one means a run that should have produced full evidence didn't, which is
/// anomalous regardless of which guard caught it. For a feed source under
/// `--delete`, only `IncompleteEnumeration` keeps that meaning — the
/// zero-seen backstop is the routine steady state there (the feed
/// document's own 304 short-circuit fires zero entry callbacks), so the
/// feed branch logs it at a lower level. See the two log call sites below.
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
        match &suppressed_because {
            // Guard 1 stays anomalous for a feed exactly as it is for
            // path/url sources: the ingestor itself failed to observe the
            // source, which is never routine.
            Some(reason @ SweepSuppression::IncompleteEnumeration(_)) => {
                tracing::warn!(
                    source_id = %source.id,
                    location = %source_location(source),
                    "skipping feed liveness sweep for source at '{}': {}. This is \
                     the same signal that suppresses the presumed-gone sweep for \
                     path/url sources above; for a feed source it is most often \
                     the feed document's own 304 short-circuit — an unchanged \
                     feed document means an unchanged window, so nothing could \
                     have aged out of it since the last run.",
                    source_location(source),
                    reason.reason(),
                );
            }
            // Guard 2's zero-seen backstop is the routine steady state for a
            // feed under `--delete`: the overwhelmingly common cause is the
            // feed document's own 304, which is exactly what conditional GET
            // exists to produce and fires zero entry callbacks. Warning on
            // every routine run trains operators to ignore the level
            // entirely, so this stays at `debug!`; only guard 1 above still
            // warns.
            Some(reason @ SweepSuppression::ZeroSeen) => {
                tracing::debug!(
                    source_id = %source.id,
                    location = %source_location(source),
                    "skipping feed liveness sweep for source at '{}': {}. This is \
                     the same signal that suppresses the presumed-gone sweep for \
                     path/url sources above; for a feed source it is most often \
                     the feed document's own 304 short-circuit — an unchanged \
                     feed document means an unchanged window, so nothing could \
                     have aged out of it since the last run.",
                    source_location(source),
                    reason.reason(),
                );
            }
            None => {
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
/// Callers must apply the same guards the presumed-gone sweep above applies
/// — an `Enumeration::Incomplete` run and a zero-seen run must not reach
/// this function at all — see the call site in `run_source_ingestion`. This
/// function performs no guard check of its own; it probes whatever
/// [`RetrievalStore::list_stale_feed_resources`] returns.
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

    let candidates: Vec<StaleFeedResource> = store
        .list_stale_feed_resources(
            store_id,
            source_id,
            &checked_before,
            FEED_LIVENESS_BATCH_LIMIT,
        )
        .await?;

    for candidate in candidates {
        // Still inside the feed's window — this run's own ingestion pass
        // already observed it, so it hasn't aged out at all. Reachable when
        // a currently-live entry's `last_checked_at` happens to be unset or
        // stale (it has simply never been probed before, or was probed long
        // ago while still current); probing it here would be redundant with
        // the ordinary ingestion pass that already just ran for it.
        if seen.contains(&candidate.uri) {
            continue;
        }

        let metadata = FetchMetadata {
            etag: candidate.external_etag.clone(),
            last_modified: candidate.external_last_modified.clone(),
        };

        // Counted for every candidate an attempt is made for, regardless of
        // outcome — see `IngestionResult::feed_entries_liveness_checked`'s
        // doc comment.
        result.feed_entries_liveness_checked += 1;

        let fetch_result = match fetcher.fetch(&candidate.uri, &metadata).await {
            Ok(r) => r,
            Err(e) => {
                // A transport error is evidence of nothing — leave the
                // resource and its `last_checked_at` untouched so it's
                // eligible again next run, exactly like `Blocked` below.
                tracing::debug!(
                    uri = %candidate.uri,
                    error = %e,
                    "feed liveness sweep: fetch error, leaving resource untouched"
                );
                continue;
            }
        };

        match fetch_result {
            FetchResult::Gone => {
                doc_index.remove(&candidate.uri);
                let deleted = store.delete_by_resource(&candidate.resource_id).await?;
                if deleted > 0 {
                    result.docs_deleted += 1;
                }
            }
            FetchResult::NotModified {
                etag,
                last_modified,
            } => {
                // A bare 304 means unchanged; fold any rotated validator
                // over what was already stored rather than reporting a
                // partial value that would read as "clear it" — mirrors the
                // feed document's own 304 handling
                // (`ingest::FeedIngestor::ingest`). Still there, so not a
                // delete.
                let etag = etag.or(candidate.external_etag);
                let last_modified = last_modified.or(candidate.external_last_modified);
                store
                    .touch_resource_liveness(
                        store_id,
                        &candidate.resource_id,
                        etag.as_deref(),
                        last_modified.as_deref(),
                    )
                    .await?;
            }
            FetchResult::Downloaded {
                etag,
                last_modified,
                ..
            } => {
                // Deliberately not re-indexed: an aged-out entry's
                // feed-sourced metadata (title, author, per-entry date) is
                // long gone, and re-indexing from the bare page alone would
                // silently degrade the stored resource rather than improve
                // it. Only the validators and the throttle clock move,
                // replacing what was stored wholesale — even with both
                // `None` — exactly like a fresh 200 does everywhere else in
                // this pipeline (a 200 is a full fresh representation, so
                // silence about a validator means the origin stopped
                // offering it, not "unchanged").
                store
                    .touch_resource_liveness(
                        store_id,
                        &candidate.resource_id,
                        etag.as_deref(),
                        last_modified.as_deref(),
                    )
                    .await?;
            }
            FetchResult::Blocked => {
                // Neither evidence of anything, like a transport error —
                // leave it untouched. In production `fetcher` here is
                // already the public-only client (see
                // `SourceIngestionDeps::fetcher`), so this should be
                // unreachable, but `FetchResult` is matched exhaustively
                // regardless.
            }
        }
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
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::ids::content_hash;
    use crate::ids::resource_id;
    use crate::store::FakeStore;
    use crate::types::{SourceKind, SourceSpec};

    fn make_ingestion_config(store_id: &str) -> IngestionConfig {
        IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v1".to_string(),
            chunker: ChunkerConfig::prose(),
        }
    }

    // ---------------------------------------------------------------------------
    // DocumentIndex tests
    // ---------------------------------------------------------------------------

    #[test]
    fn document_index_empty() {
        let idx = DocumentIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn document_index_upsert_and_get() {
        let mut idx = DocumentIndex::new();
        let rec = DocumentRecord {
            uri: "file:///test.md".to_string(),
            resource_id: "doc-id-1".to_string(),
            source_id: "src-1".to_string(),
            content_hash: "hash-1".to_string(),
            policy_version: "v1".to_string(),
            metadata_hash: "mhash-1".to_string(),
            external_etag: None,
            external_last_modified: None,
        };
        idx.upsert(rec.clone());
        let found = idx.get("file:///test.md").unwrap();
        assert_eq!(found.resource_id, "doc-id-1");
    }

    #[test]
    fn document_index_remove() {
        let mut idx = DocumentIndex::new();
        let rec = DocumentRecord {
            uri: "file:///test.md".to_string(),
            resource_id: "doc-id-1".to_string(),
            source_id: "src-1".to_string(),
            content_hash: "hash-1".to_string(),
            policy_version: "v1".to_string(),
            metadata_hash: "mhash-1".to_string(),
            external_etag: None,
            external_last_modified: None,
        };
        idx.upsert(rec);
        let removed = idx.remove("file:///test.md");
        assert!(removed.is_some());
        assert!(idx.is_empty());
    }

    // ---------------------------------------------------------------------------
    // IngestionResult wire compatibility
    // ---------------------------------------------------------------------------

    /// This type crosses a version boundary: `localdb index` attaches to a
    /// running daemon's SSE stream, and the two are not upgraded together.
    /// Without struct-level `#[serde(default)]` a newer CLI reading an older
    /// daemon's `SourceFinished` frame fails the whole deserialize on the
    /// first field the daemon never sent, dropping a frame the user is
    /// watching.
    ///
    /// Asserted by deserializing an *empty* object, not by round-tripping a
    /// populated one — a round trip passes with or without the attribute,
    /// since it never omits a field.
    #[test]
    fn ingestion_result_deserializes_from_an_empty_object() {
        let from_nothing: IngestionResult =
            serde_json::from_str("{}").expect("every field must be optional on the wire");
        let expected = IngestionResult::default();
        assert_eq!(from_nothing.docs_seen, expected.docs_seen);
        assert_eq!(from_nothing.docs_indexed, expected.docs_indexed);
        assert_eq!(from_nothing.docs_skipped, expected.docs_skipped);
        assert_eq!(from_nothing.docs_deleted, expected.docs_deleted);
        assert_eq!(from_nothing.docs_prunable, expected.docs_prunable);
        assert_eq!(
            from_nothing.docs_metadata_updated,
            expected.docs_metadata_updated
        );
        assert_eq!(from_nothing.chunks_written, expected.chunks_written);
        assert_eq!(
            from_nothing.unsupported_format_count,
            expected.unsupported_format_count
        );
        assert_eq!(from_nothing.error_count, expected.error_count);
        assert_eq!(
            from_nothing.document_validators,
            expected.document_validators
        );
        assert_eq!(
            from_nothing.document_inputs_digest,
            expected.document_inputs_digest
        );
    }

    /// The other direction, which is the one that actually bites in
    /// production: an *older* consumer must not choke on a field it has
    /// never heard of. Serde ignores unknown keys by default, and nothing
    /// on this type opts into `deny_unknown_fields` — pinned here so a
    /// future contributor adding it has to argue with a failing test.
    #[test]
    fn ingestion_result_ignores_fields_it_does_not_know() {
        let from_future: IngestionResult =
            serde_json::from_str(r#"{"docs_seen":3,"docs_teleported":9}"#)
                .expect("an unknown counter must not fail the frame");
        assert_eq!(from_future.docs_seen, 3);
    }

    // ---------------------------------------------------------------------------
    // IndexJob lifecycle tests
    // ---------------------------------------------------------------------------

    #[test]
    fn create_index_job_starts_pending() {
        let job = create_index_job("store-1", IndexJobScope::Store);
        assert_eq!(job.state, IndexJobState::Pending);
        assert!(job.started_at.is_none());
        assert!(job.completed_at.is_none());
        assert!(job.error.is_none());
    }

    #[test]
    fn start_index_job_sets_running() {
        let mut job = create_index_job("store-1", IndexJobScope::Store);
        start_index_job(&mut job);
        assert_eq!(job.state, IndexJobState::Running);
        assert!(job.started_at.is_some());
    }

    #[test]
    fn complete_index_job_sets_done() {
        let mut job = create_index_job("store-1", IndexJobScope::Store);
        start_index_job(&mut job);
        let stats = IndexJobStats {
            docs_seen: 5,
            docs_indexed: 3,
            docs_deleted: 1,
            chunks_written: 12,
            unsupported_format_count: 1,
            error_count: 0,
            ..Default::default()
        };
        complete_index_job(&mut job, stats.clone());
        assert_eq!(job.state, IndexJobState::Done);
        assert!(job.completed_at.is_some());
        assert_eq!(job.stats.docs_seen, 5);
        assert_eq!(job.stats.docs_indexed, 3);
        assert_eq!(job.stats.chunks_written, 12);
    }

    #[test]
    fn fail_index_job_sets_failed() {
        let mut job = create_index_job("store-1", IndexJobScope::Store);
        start_index_job(&mut job);
        fail_index_job(&mut job, "something went wrong".to_string());
        assert_eq!(job.state, IndexJobState::Failed);
        assert_eq!(job.error.as_deref(), Some("something went wrong"));
        assert_eq!(
            job.error_code, None,
            "a synthetic queue-level failure never had a typed error to carry a code from"
        );
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn fail_index_job_with_error_carries_the_typed_errors_code_and_message() {
        let mut job = create_index_job("store-1", IndexJobScope::Store);
        start_index_job(&mut job);
        let err = Error::InvalidConfig {
            message: "unconfigured embedder provider".to_string(),
        };
        fail_index_job_with_error(&mut job, &err);
        assert_eq!(job.state, IndexJobState::Failed);
        // `job.error` must be the *bare* message ("unconfigured embedder
        // provider"), not `err.to_string()` ("invalid config: unconfigured
        // embedder provider"): `cli::job_attach::finish_job` reconstructs the
        // typed error via `Error::from_code(error_code, error)`, which
        // re-adds the "invalid config: " prefix through `Display`. Storing
        // the already-prefixed string here would double it (issue #187
        // review, finding F4).
        assert_eq!(job.error.as_deref(), Some("unconfigured embedder provider"));
        assert_eq!(job.error_code.as_deref(), Some("invalid_config"));
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn fail_index_job_with_error_falls_back_to_display_for_non_reconstructible_variants() {
        // A variant `raw_message()` returns `None` for (e.g. `Internal`,
        // whose fields don't fit a single `message` string) must still
        // populate `job.error` with something readable — the full `Display`
        // string, since there's no bare field to store instead.
        let mut job = create_index_job("store-1", IndexJobScope::Store);
        start_index_job(&mut job);
        let err = Error::Internal {
            message: "bug".to_string(),
            correlation_id: "corr-1".to_string(),
        };
        fail_index_job_with_error(&mut job, &err);
        assert_eq!(job.error.as_deref(), Some(err.to_string().as_str()));
        assert_eq!(job.error_code.as_deref(), Some("internal"));
    }

    /// Issue #218 review, fix 2: cancelling a still-`Pending` job (before
    /// the worker ever calls `start_index_job` on it) goes straight
    /// `Pending -> Failed` — the one path that leaves `started_at: None` on
    /// a terminal job, since the job never actually ran. Pins the exact
    /// record shape `IndexJobState`'s doc comment now documents, produced
    /// the same way `server::job_queue::run_worker` produces it for a
    /// pending-cancelled job: `fail_index_job_with_error` called on a job
    /// that never went through `start_index_job`.
    #[test]
    fn fail_index_job_with_error_on_a_still_pending_job_leaves_started_at_none() {
        let mut job = create_index_job("store-1", IndexJobScope::Store);
        assert_eq!(job.state, IndexJobState::Pending);
        assert!(job.started_at.is_none());

        fail_index_job_with_error(&mut job, &Error::JobCancelled);

        assert_eq!(job.state, IndexJobState::Failed);
        assert_eq!(job.error_code.as_deref(), Some("job_cancelled"));
        assert!(
            job.started_at.is_none(),
            "a job cancelled before it ever started must not gain a started_at"
        );
        assert!(
            job.completed_at.is_some(),
            "the job is still terminal and must record when that happened"
        );
    }

    // ---------------------------------------------------------------------------
    // glob_match tests
    // ---------------------------------------------------------------------------

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("README.md", "README.md"));
        assert!(!glob_match("README.md", "readme.md"));
    }

    #[test]
    fn glob_match_star() {
        assert!(glob_match("*.md", "README.md"));
        assert!(glob_match("*.md", "notes.md"));
        assert!(!glob_match("*.md", "path/to/notes.md")); // * doesn't cross /
    }

    #[test]
    fn glob_match_double_star() {
        assert!(glob_match("**/*.md", "notes.md"));
        assert!(glob_match("**/*.md", "docs/notes.md"));
        assert!(glob_match("**/*.md", "a/b/c/notes.md"));
    }

    #[test]
    fn glob_match_double_star_dir() {
        assert!(glob_match("**/node_modules/**", "a/node_modules/b/c"));
    }

    #[test]
    fn glob_match_question_mark() {
        assert!(glob_match("file?.md", "file1.md"));
        assert!(glob_match("file?.md", "fileA.md"));
        assert!(!glob_match("file?.md", "file10.md"));
    }

    #[test]
    fn glob_match_non_ascii_does_not_panic() {
        // Regression: en-dash (3-byte char) used to land mid-char in `&path[i..]`.
        assert!(glob_match("*.md", "Notes \u{2013} draft.md"));
        assert!(glob_match(
            "**/*.md",
            "caf\u{e9}/r\u{e9}sum\u{e9} \u{2013} v2.md"
        ));
        assert!(glob_match("*", "\u{dc}n\u{ef}c\u{f6}d\u{eb}.txt"));
        assert!(!glob_match("*.pdf", "Notes \u{2013} draft.md"));
    }

    // ---------------------------------------------------------------------------
    // Path source enumeration tests
    // ---------------------------------------------------------------------------

    /// #156: a root that does not exist is `RootUnavailable`, not an empty
    /// `Complete`. Collapsing the two is what let an unmounted volume look
    /// like a source whose every file had been deleted.
    #[test]
    fn enumerate_path_source_missing_root_is_unavailable() {
        let enumeration = enumerate_path_source("/this/path/does/not/exist", &[], &[]).unwrap();
        assert!(
            matches!(enumeration, PathEnumeration::RootUnavailable),
            "a missing root must be reported as unavailable, not as zero files"
        );
    }

    /// The other half of the distinction: a root that exists and genuinely
    /// holds nothing is `Complete(vec![])` — an observation, not an absence
    /// of one — and the sweep is right to act on it.
    #[test]
    fn enumerate_path_source_empty_dir_is_complete_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let enumeration = enumerate_path_source(dir.path().to_str().unwrap(), &[], &[]).unwrap();
        assert!(
            matches!(&enumeration, PathEnumeration::Complete(files) if files.is_empty()),
            "an existing but empty root is a complete enumeration of zero files"
        );
    }

    #[test]
    fn enumerate_path_source_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"# A").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"hello").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(files.len(), 2, "should find both files");
    }

    #[test]
    fn enumerate_path_source_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), b"# Notes").unwrap();
        std::fs::write(dir.path().join("data.bin"), b"\x00\x01\x02").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &["*.md".to_string()], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(files.len(), 1, "should find only .md files");
        assert!(files[0].path.to_str().unwrap().ends_with(".md"));
    }

    #[test]
    fn enumerate_path_source_exclude_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules").join("lib.js"), b"module").unwrap();
        std::fs::write(dir.path().join("app.js"), b"app").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &["**/node_modules/**".to_string()])
            .unwrap()
            .files()
            .to_vec();
        // Should exclude node_modules files
        assert!(
            files
                .iter()
                .all(|f| !f.path.to_str().unwrap().contains("node_modules")),
            "node_modules files should be excluded"
        );
    }

    #[test]
    fn enumerate_excludes_nested_ds_store_by_basename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Call")).unwrap();
        std::fs::write(dir.path().join("Call").join(".DS_Store"), b"\x00\x01junk").unwrap();
        std::fs::write(dir.path().join("Call").join("note.md"), b"# Note").unwrap();
        std::fs::write(dir.path().join(".DS_Store"), b"\x00root").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &[".DS_Store".to_string()])
            .unwrap()
            .files()
            .to_vec();
        assert!(
            files
                .iter()
                .all(|f| !f.path.to_string_lossy().ends_with(".DS_Store")),
            "no .DS_Store at any depth should be enumerated"
        );
        assert!(files
            .iter()
            .any(|f| f.path.to_string_lossy().ends_with("note.md")));
    }

    #[test]
    fn enumerate_prunes_nested_junk_dirs_by_basename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join(".git")).unwrap();
        std::fs::write(dir.path().join("a").join(".git").join("config"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("node_modules").join("pkg")).unwrap();
        std::fs::write(
            dir.path()
                .join("a")
                .join("node_modules")
                .join("pkg")
                .join("i.js"),
            b"j",
        )
        .unwrap();
        std::fs::write(dir.path().join("a").join("keep.md"), b"# Keep").unwrap();

        let root = dir.path().to_str().unwrap();
        let files =
            enumerate_path_source(root, &[], &[".git".to_string(), "node_modules".to_string()])
                .unwrap()
                .files()
                .to_vec();
        assert!(
            files.iter().all(|f| {
                let p = f.path.to_string_lossy();
                !p.contains("/.git/") && !p.contains("/node_modules/")
            }),
            "nested .git and node_modules subtrees must be pruned"
        );
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn enumerate_exclude_double_star_pattern_still_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join(".DS_Store"), b"x").unwrap();
        std::fs::write(dir.path().join("sub").join("a.md"), b"# A").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &["**/.DS_Store".to_string()])
            .unwrap()
            .files()
            .to_vec();
        assert!(files
            .iter()
            .all(|f| !f.path.to_string_lossy().ends_with(".DS_Store")));
    }

    #[test]
    fn enumerate_include_semantics_unchanged_after_exclude_basename_fix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs").join("notes.md"), b"# N").unwrap();
        std::fs::write(dir.path().join("docs").join("data.bin"), b"\x00").unwrap();

        let root = dir.path().to_str().unwrap();
        // Bare `*.md` include must NOT match nested docs/notes.md (path-anchored).
        let files = enumerate_path_source(root, &["*.md".to_string()], &[])
            .unwrap()
            .files()
            .to_vec();
        assert!(
            files.is_empty(),
            "bare *.md include must not match at depth"
        );
        // `**/*.md` does match.
        let files = enumerate_path_source(root, &["**/*.md".to_string()], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.to_string_lossy().ends_with("notes.md"));
    }

    #[test]
    fn enumerate_exclude_double_star_prunes_nested_dir_before_recursing() {
        // `**/X` (no trailing `/**`) matches the X entry itself, so the dir is
        // excluded before we recurse into it — O(1) prune rather than
        // walk-and-filter. This exercises the shipped DEFAULT_PATH_EXCLUDES form.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("node_modules").join("big")).unwrap();
        std::fs::write(
            dir.path()
                .join("a")
                .join("node_modules")
                .join("big")
                .join("lib.js"),
            b"module",
        )
        .unwrap();
        std::fs::write(dir.path().join("a").join("keep.rs"), b"fn main() {}").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &["**/node_modules".to_string()])
            .unwrap()
            .files()
            .to_vec();
        assert!(
            files
                .iter()
                .all(|f| !f.path.to_string_lossy().contains("node_modules")),
            "`**/node_modules` must exclude the dir and its contents at any depth"
        );
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn enumerate_path_source_uris_are_file_uris() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.md"), b"content").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].uri.scheme(), "file");
        assert!(files[0].uri.as_str().starts_with("file://"));
    }

    #[test]
    fn enumerate_path_source_handles_non_ascii_filenames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Notes \u{2013} draft.md"), b"# hi").unwrap();
        std::fs::write(dir.path().join("r\u{e9}sum\u{e9}.txt"), b"x").unwrap();
        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &["*.md".to_string()], &[])
            .unwrap()
            .files()
            .to_vec();
        assert_eq!(files.len(), 1); // only the .md, no panic
    }

    // ---------------------------------------------------------------------------
    // A3 — is_store_stale works on an empty FakeStore without panicking
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn is_store_stale_empty_store_does_not_panic() {
        let store = FakeStore::new();
        // Must not panic or return an error even though the store is empty.
        let result = is_store_stale(&store, "policy-v1").await;
        assert!(
            result.is_ok(),
            "is_store_stale must not error on empty store"
        );
        assert!(
            !result.unwrap(),
            "empty store must be reported as not stale"
        );
    }

    #[tokio::test]
    async fn store_stale_detection_works() {
        use crate::store::RetrievalStore;

        let store = FakeStore::new();
        let store_id = "store-1";

        // Seed one chunk directly — is_store_stale only samples an existing
        // chunk's policy_version via bm25_search, so there is no need to
        // route this through the ingestion pipeline.
        let mut chunk = make_chunk_record(
            "chunk-1",
            "doc-1",
            store_id,
            "file:///docs/test.md",
            "hash1",
        );
        chunk.policy_version = "policy-v1".to_string();
        store.upsert_chunks(vec![chunk]).await.unwrap();

        // Check with same policy — not stale
        let not_stale = is_store_stale(&store, "policy-v1").await.unwrap();
        assert!(!not_stale, "store should not be stale with same policy");

        // Check with different policy — stale
        let stale = is_store_stale(&store, "policy-v2").await.unwrap();
        assert!(stale, "store should be stale when policy changed");
    }

    // ---------------------------------------------------------------------------
    // A6 / F4 — embed-before-delete ordering and short embedder guard
    // ---------------------------------------------------------------------------

    /// An embedder that always fails with an internal error.
    struct FailingEmbedder;

    #[async_trait::async_trait]
    impl crate::embedder::Embedder for FailingEmbedder {
        async fn embed_documents(
            &self,
            _docs: Vec<crate::embedder::DocumentChunks>,
        ) -> Result<Vec<crate::embedder::EmbeddedDocument>, Error> {
            Err(Error::Internal {
                message: "intentional embedder failure for testing".to_string(),
                correlation_id: "failing_embedder".to_string(),
            })
        }

        fn embedding_dim(&self) -> usize {
            4
        }

        fn model_id(&self) -> &str {
            "failing-embedder"
        }
    }

    /// An embedder that returns fewer vectors than input chunks.
    struct ShortEmbedder {
        dim: usize,
    }

    #[async_trait::async_trait]
    impl crate::embedder::Embedder for ShortEmbedder {
        async fn embed_documents(
            &self,
            docs: Vec<crate::embedder::DocumentChunks>,
        ) -> Result<Vec<crate::embedder::EmbeddedDocument>, Error> {
            // Return one EmbeddedDocument but with fewer vectors than there are chunks.
            let result = docs
                .iter()
                .map(|doc| {
                    // Return at most 0 vectors regardless of how many chunks there are.
                    let _ = &doc.chunks;
                    vec![] // always empty — guarantees a length mismatch
                })
                .collect();
            Ok(result)
        }

        fn embedding_dim(&self) -> usize {
            self.dim
        }

        fn model_id(&self) -> &str {
            "short-embedder"
        }
    }

    // ---------------------------------------------------------------------------
    // scale_to_chars tests
    // ---------------------------------------------------------------------------

    #[test]
    fn scale_to_chars_scales_prose_budget_by_four() {
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(256),
            overlap_tokens: Some(0),
            window_turns: None,
            stride_turns: None,
        };
        let scaled = scale_to_chars(&cfg);
        assert_eq!(scaled.preset, "prose");
        assert_eq!(
            scaled.resolved_target_tokens(),
            256 * 4,
            "prose target should be scaled ×4 for CharSizer"
        );
        assert_eq!(
            scaled.resolved_overlap_tokens(),
            0,
            "prose overlap should be scaled ×4 for CharSizer (0 × 4 = 0)"
        );
    }

    #[test]
    fn scale_to_chars_does_not_change_code_preset() {
        let cfg = ChunkerConfig {
            preset: "code".to_string(),
            target_tokens: Some(3000),
            overlap_tokens: Some(0),
            window_turns: None,
            stride_turns: None,
        };
        let scaled = scale_to_chars(&cfg);
        assert_eq!(scaled.preset, "code");
        assert_eq!(
            scaled.resolved_target_tokens(),
            3000,
            "code preset must not be scaled"
        );
        assert_eq!(
            scaled.resolved_overlap_tokens(),
            0,
            "code overlap must not be scaled"
        );
    }

    #[test]
    fn scale_to_chars_uses_preset_defaults_when_none() {
        // Verify None values resolve through resolved_* before scaling.
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: None,
            overlap_tokens: None,
            window_turns: None,
            stride_turns: None,
        };
        let scaled = scale_to_chars(&cfg);
        // Default prose target is 256; scaled = 256 * 4 = 1024. Overlap 0 → 0.
        assert_eq!(scaled.resolved_target_tokens(), 256 * 4);
        assert_eq!(scaled.resolved_overlap_tokens(), 0);
    }

    #[tokio::test]
    async fn from_records_deduplicates_by_uri() {
        use crate::store::RetrievalStore;

        let store = FakeStore::new();
        // Insert two chunks for the same URI with the same document metadata.
        let chunk_a = make_chunk_record("chunk-1", "doc-1", "store-1", "file:///a.md", "hash1");
        let chunk_b = make_chunk_record("chunk-2", "doc-1", "store-1", "file:///a.md", "hash1");
        let chunk_c = make_chunk_record("chunk-3", "doc-2", "store-1", "file:///b.md", "hash2");
        store
            .upsert_chunks(vec![chunk_a, chunk_b, chunk_c])
            .await
            .unwrap();

        let records = store.list_indexed_documents().await.unwrap();
        assert_eq!(records.len(), 2, "two distinct URIs → two records");

        let idx = DocumentIndex::from_records(records);
        assert_eq!(idx.len(), 2);
        assert!(idx.get("file:///a.md").is_some());
        assert!(idx.get("file:///b.md").is_some());
    }

    fn make_chunk_record(
        id: &str,
        doc_id: &str,
        store_id: &str,
        uri: &str,
        content_hash: &str,
    ) -> crate::store::ChunkRecord {
        use crate::types::Span;
        crate::store::ChunkRecord {
            id: id.to_string(),
            resource_id: doc_id.to_string(),
            store_id: store_id.to_string(),
            text: "test text".to_string(),
            span: Span::new(0, 9),
            heading_path: vec![],
            embedding: vec![0.0, 0.0, 0.0, 0.0],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-22T00:00:00Z".to_string(),
            modified_at: Some("2026-06-22T00:00:00Z".to_string()),
            content_hash: content_hash.to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: None,
            uri: uri.to_string(),
            metadata: crate::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
        }
    }

    // ---------------------------------------------------------------------------
    // Pipeline tests — run_source_ingestion / index_resource
    //
    // Exercises the Ingestor-driven pipeline using a scripted FakeIngestor in
    // place of real file/URL enumeration.
    // ---------------------------------------------------------------------------
    mod unified_pipeline {
        use super::*;
        use crate::block::{Block, BlockKind, IngestorKind, ResourceKind};
        use crate::embedder::EmbeddedDocument;
        use crate::ingestor::IngestResult;
        use crate::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};
        use crate::progress::{DocOutcome, ProgressEvent};
        use crate::uri::Uri;

        // -----------------------------------------------------------------
        // Log-level capture (suppressed-sweep tests below assert on the
        // actual emitted level, not just on behavior)
        // -----------------------------------------------------------------

        /// A minimal `MakeWriter` capturing formatted log lines into a
        /// shared buffer, installed via `tracing::subscriber::set_default`
        /// — scoped to the current task rather than
        /// `set_global_default`, mirroring
        /// `server::daemon::tests::rejected_logging`'s same pattern.
        #[derive(Clone, Default)]
        struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for LogBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        /// Installs a thread-local subscriber capturing every line at
        /// `DEBUG` and above; drop the returned guard when done observing.
        fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
            let buf = LogBuf::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(buf.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish();
            let guard = tracing::subscriber::set_default(subscriber);
            (buf, guard)
        }

        fn captured_text(buf: &LogBuf) -> String {
            String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
        }

        // -----------------------------------------------------------------
        // Fixtures
        // -----------------------------------------------------------------

        fn make_source_with_preset(store_id: &str, preset: &str) -> Source {
            Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                spec: SourceSpec::Path {
                    root: "/docs".to_string(),
                    include: vec![],
                    exclude: vec![],
                },
                source_preset: preset.to_string(),
            }
        }

        fn make_resource(uri: &str, text: &str, source_id: &str, store_id: &str) -> Resource {
            make_resource_with_blocks(
                uri,
                source_id,
                store_id,
                vec![Block {
                    seq: 0,
                    kind: BlockKind::Text,
                    text: text.to_string(),
                    location: None,
                }],
            )
        }

        fn make_resource_with_blocks(
            uri: &str,
            source_id: &str,
            store_id: &str,
            blocks: Vec<Block>,
        ) -> Resource {
            let joined: String = blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let hash = content_hash(&joined);
            let id = resource_id(uri, &hash);
            Resource {
                id,
                store_id: store_id.to_string(),
                source_id: source_id.to_string(),
                ingestor_kind: IngestorKind::File,
                resource_kind: ResourceKind::Document,
                uri: Uri::parse(uri).unwrap_or_else(|| panic!("invalid test uri: {uri}")),
                external_id: None,
                external_etag: None,
                external_last_modified: None,
                content_hash: hash,
                title: None,
                mime: Some("text/markdown".to_string()),
                metadata: Metadata::Document(DocumentMetadata::default()),
                added_at: "2026-06-10T12:00:00Z".to_string(),
                modified_at: Some("2026-06-10T12:00:00Z".to_string()),
                thread_id: None,
                channel: None,
                participants: vec![],
                origin_store: store_id.to_string(),
                policy_version: "policy-v1".to_string(),
                share_path: None,
                extractor_version: "1".to_string(),
                blocks,
            }
        }

        /// Index a resource directly (bypassing the callback) to seed prior
        /// state in `store/doc_index`, mimicking "already indexed in an
        /// earlier run".
        async fn seed_indexed(
            store: &FakeStore,
            embedder: &FakeEmbedder,
            config: &IngestionConfig,
            source: &Source,
            uri: &str,
            text: &str,
        ) -> DocumentRecord {
            let resource = make_resource(uri, text, &source.id, &config.store_id);
            let deps = IndexResourceDeps {
                store,
                embedder,
                config,
            };
            let outcome = index_resource(&resource, source, None, &deps)
                .await
                .expect("seed index must succeed");
            // Reuse the hash `index_resource` actually persisted rather than
            // recomputing it here — the same "thread it out, don't
            // duplicate" reasoning as `PipelineCallback::on_resource`'s
            // `Written` arm.
            let metadata_hash = match outcome {
                IndexOutcome::Written(_, hash) => hash,
                IndexOutcome::Empty => panic!("seed_indexed: resource must not chunk to empty"),
            };
            // The doc_index key must be the NORMALIZED uri, exactly as
            // `list_indexed_documents` rehydrates it — a raw spelling here
            // diverges from the pipeline's seen-set whenever the path needs
            // percent-encoding (e.g. a directory with a space), and the
            // sweep would delete a live document it just observed.
            DocumentRecord {
                uri: resource.uri.as_str().to_string(),
                resource_id: resource.id.clone(),
                source_id: source.id.clone(),
                content_hash: resource.content_hash.clone(),
                policy_version: config.policy_version.clone(),
                metadata_hash,
                external_etag: resource.external_etag.clone(),
                external_last_modified: resource.external_last_modified.clone(),
            }
        }

        // -----------------------------------------------------------------
        // FakeIngestor — scripted Ingestor for testing run_source_ingestion
        // -----------------------------------------------------------------

        // Test-only fixture enum; the size skew between variants doesn't
        // matter here (small, short-lived Vec<ScriptStep> per test).
        #[allow(clippy::large_enum_variant)]
        enum ScriptStep {
            Discovered(usize),
            Resource(Resource),
            Skipped(String, SkipReason),
            /// Positively confirmed absent at the origin (404/410).
            Gone(String),
        }

        struct FakeIngestor {
            script: std::sync::Mutex<Vec<ScriptStep>>,
            /// What this ingestor claims about enumeration completeness —
            /// `Complete` unless a test is exercising the #156 guard.
            enumeration: Enumeration,
        }

        impl FakeIngestor {
            fn new(script: Vec<ScriptStep>) -> Self {
                Self {
                    script: std::sync::Mutex::new(script),
                    enumeration: Enumeration::Complete,
                }
            }

            /// An ingestor that ran without error but could not observe the
            /// source — the shape a `FileIngestor` over an unmounted volume
            /// reports.
            fn incomplete(reason: &str) -> Self {
                Self {
                    script: std::sync::Mutex::new(vec![]),
                    enumeration: Enumeration::Incomplete {
                        reason: reason.to_string(),
                    },
                }
            }
        }

        #[async_trait::async_trait]
        impl Ingestor for FakeIngestor {
            fn kind(&self) -> IngestorKind {
                IngestorKind::File
            }

            async fn ingest(
                &self,
                _source: &IngestSource,
                callback: &mut dyn IngestCallback,
            ) -> Result<IngestResult, Error> {
                let steps: Vec<ScriptStep> = std::mem::take(&mut *self.script.lock().unwrap());
                let mut produced = 0;
                let mut skipped = 0;
                let mut errors = 0;
                for step in steps {
                    match step {
                        ScriptStep::Discovered(n) => callback.on_discovered(n).await,
                        ScriptStep::Resource(r) => {
                            callback.on_resource(r).await?;
                            produced += 1;
                        }
                        ScriptStep::Skipped(uri, reason) => {
                            // Mirror how a real ingestor bumps its own
                            // `errors` counter in lockstep with every
                            // `on_skipped(SkipReason::Error(_))` call (see
                            // the `run_source_ingestion` debug_assert this
                            // feeds).
                            if matches!(reason, SkipReason::Error(_)) {
                                errors += 1;
                            } else {
                                skipped += 1;
                            }
                            // `on_skipped` now takes an already-canonical
                            // `Uri` (see `Ingestor::on_skipped`'s doc
                            // comment): a real ingestor would build this
                            // from `Uri::parse`/`Uri::from_file_path` itself
                            // before ever reaching the pipeline, so the
                            // fixture does the same rather than accepting a
                            // raw string this trait no longer allows. Every
                            // script in this test module uses a valid
                            // locator, so this `expect` never fires.
                            let uri = Uri::parse(&uri)
                                .unwrap_or_else(|| panic!("invalid test skip uri: {uri}"));
                            callback.on_skipped(&uri, reason).await;
                        }
                        ScriptStep::Gone(uri) => {
                            let uri = Uri::parse(&uri)
                                .unwrap_or_else(|| panic!("invalid test gone uri: {uri}"));
                            callback.on_gone(&uri).await;
                        }
                    }
                }
                Ok(IngestResult {
                    resources_produced: produced,
                    resources_skipped: skipped,
                    errors,
                    enumeration: self.enumeration.clone(),
                    document_validators: None,
                })
            }
        }

        /// Embedder that fails only when a chunk's text contains a marker
        /// substring, delegating to a real `FakeEmbedder` otherwise — lets a
        /// mixed script exercise both a successful resource and a failing one.
        struct SelectiveFailEmbedder {
            fail_marker: &'static str,
            inner: FakeEmbedder,
        }

        #[async_trait::async_trait]
        impl Embedder for SelectiveFailEmbedder {
            async fn embed_documents(
                &self,
                docs: Vec<DocumentChunks>,
            ) -> Result<Vec<EmbeddedDocument>, Error> {
                for doc in &docs {
                    if doc.chunks.iter().any(|c| c.contains(self.fail_marker)) {
                        return Err(Error::Internal {
                            message: "selective embedder failure for testing".to_string(),
                            correlation_id: "selective_fail_embedder".to_string(),
                        });
                    }
                }
                self.inner.embed_documents(docs).await
            }

            fn embedding_dim(&self) -> usize {
                self.inner.embedding_dim()
            }

            fn model_id(&self) -> &str {
                self.inner.model_id()
            }
        }

        fn progress_collector() -> (
            crate::progress::ProgressSink,
            std::sync::Arc<std::sync::Mutex<Vec<ProgressEvent>>>,
        ) {
            let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let events2 = events.clone();
            let sink: crate::progress::ProgressSink = std::sync::Arc::new(move |e| {
                events2.lock().unwrap().push(e);
            });
            (sink, events)
        }

        // -----------------------------------------------------------------
        // 1. Counter parity for a mixed script
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn counter_parity_for_mixed_script() {
            let store = FakeStore::new();
            let embedder = SelectiveFailEmbedder {
                fail_marker: "FAIL_MARKER",
                inner: FakeEmbedder::new(4),
            };
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let good = make_resource(
                "file:///docs/good.md",
                "Some good content to index.",
                &source.id,
                store_id,
            );
            let bad = make_resource(
                "file:///docs/bad.md",
                "This contains FAIL_MARKER and will error.",
                &source.id,
                store_id,
            );

            let ingestor = FakeIngestor::new(vec![
                ScriptStep::Discovered(4),
                ScriptStep::Resource(good),
                ScriptStep::Resource(bad),
                ScriptStep::Skipped(
                    "file:///docs/unchanged.md".to_string(),
                    SkipReason::Unchanged,
                ),
                ScriptStep::Skipped(
                    "file:///docs/binary.bin".to_string(),
                    SkipReason::Unsupported,
                ),
            ]);

            let mut doc_index = DocumentIndex::new();
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_seen, 4, "all four discovered items are seen");
            assert_eq!(result.docs_indexed, 1, "only the good resource indexes");
            assert_eq!(
                result.docs_skipped, 1,
                "on_skipped(Unchanged) counts as skipped"
            );
            assert_eq!(result.unsupported_format_count, 1);
            assert_eq!(
                result.error_count, 1,
                "the failing resource counts as an error"
            );
            assert!(result.chunks_written > 0);
        }

        // -----------------------------------------------------------------
        // 1a. Codex review finding F1 (ingest/url_pipeline.rs) — an
        //     accepted-but-empty extraction reports `SkipReason::Other` and
        //     must land in `docs_skipped`, NOT `unsupported_format_count`:
        //     the two counters mean different things ("extraction produced
        //     nothing" vs "no parser handles this format") and the CLI
        //     reports them as separate fields.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn skip_reason_other_counts_as_docs_skipped_not_unsupported() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let ingestor = FakeIngestor::new(vec![
                ScriptStep::Discovered(1),
                ScriptStep::Skipped(
                    "https://example.com/empty".to_string(),
                    SkipReason::Other("extraction produced no content".to_string()),
                ),
            ]);

            let mut doc_index = DocumentIndex::new();
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_skipped, 1,
                "SkipReason::Other must count as docs_skipped"
            );
            assert_eq!(
                result.unsupported_format_count, 0,
                "SkipReason::Other must NOT count toward unsupported_format_count — \
                 that counter is reserved for SkipReason::Unsupported (no parser \
                 handles the format), a different condition than an \
                 accepted-but-empty extraction"
            );
            assert_eq!(result.error_count, 0);
        }

        // -----------------------------------------------------------------
        // 1b. C8 — SkipReason::Error is counted as an error (not a skip),
        //     while SkipReason::Unchanged still counts as a skip; both keep
        //     their URIs alive across the delete-sweep.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn on_skipped_error_counts_as_error_not_skip_and_survives_sweep() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let error_uri = "file:///docs/transient-failure.md";
            let unchanged_uri = "file:///docs/unchanged.md";

            // Both URIs already have prior indexed content — the run below
            // must leave that content in place (they're reported alive via
            // on_skipped, never seen via on_resource).
            let error_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                error_uri,
                "Content that will transiently fail this run.",
            )
            .await;
            let unchanged_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                unchanged_uri,
                "Content that never changes.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(error_record.clone());
            doc_index.upsert(unchanged_record.clone());

            let good = make_resource(
                "file:///docs/good.md",
                "Brand new good content.",
                &source.id,
                store_id,
            );

            let ingestor = FakeIngestor::new(vec![
                ScriptStep::Discovered(3),
                ScriptStep::Resource(good),
                ScriptStep::Skipped(
                    error_uri.to_string(),
                    SkipReason::Error("transient read failure".to_string()),
                ),
                ScriptStep::Skipped(unchanged_uri.to_string(), SkipReason::Unchanged),
            ]);

            let (sink, events) = progress_collector();
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: Some(sink),
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_indexed, 1, "only the new good resource indexes");
            assert_eq!(
                result.docs_skipped, 1,
                "SkipReason::Unchanged still counts as docs_skipped"
            );
            assert_eq!(
                result.error_count, 1,
                "SkipReason::Error must be counted as an error, not a skip"
            );

            // Both previously-indexed URIs must survive the delete-sweep.
            assert!(
                doc_index.get(error_uri).is_some(),
                "the errored URI must stay alive in the doc_index"
            );
            assert!(
                doc_index.get(unchanged_uri).is_some(),
                "the unchanged URI must stay alive in the doc_index"
            );
            assert!(
                !store
                    .get_chunks_for_resource(&error_record.resource_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "the errored URI's existing chunks must not be swept"
            );
            assert!(
                !store
                    .get_chunks_for_resource(&unchanged_record.resource_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "the unchanged URI's existing chunks must not be swept"
            );

            // Progress event for the errored URI must report DocOutcome::Error,
            // distinct from DocOutcome::Skipped.
            let events = events.lock().unwrap();
            let error_event = events.iter().find_map(|e| match e {
                ProgressEvent::DocumentFinished { uri, outcome } if uri == error_uri => {
                    Some(outcome)
                }
                _ => None,
            });
            assert!(
                matches!(error_event, Some(DocOutcome::Error)),
                "expected DocOutcome::Error for the errored URI, got {error_event:?}"
            );
        }

        // -----------------------------------------------------------------
        // 2. Progress-event sequence parity
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn progress_event_sequence_parity() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let good = make_resource(
                "file:///docs/good.md",
                "Some content to index.",
                &source.id,
                store_id,
            );

            let ingestor = FakeIngestor::new(vec![
                ScriptStep::Discovered(2),
                ScriptStep::Resource(good),
                ScriptStep::Skipped(
                    "file:///docs/unsupported.bin".to_string(),
                    SkipReason::Unsupported,
                ),
            ]);

            let (sink, events) = progress_collector();
            let mut doc_index = DocumentIndex::new();
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: Some(sink),
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            let events = events.lock().unwrap();
            let kinds: Vec<&'static str> = events
                .iter()
                .map(|e| match e {
                    ProgressEvent::SourceStarted { .. } => "source_started",
                    ProgressEvent::Discovered { .. } => "discovered",
                    ProgressEvent::DocumentStarted { .. } => "doc_started",
                    ProgressEvent::DocumentFinished { .. } => "doc_finished",
                    ProgressEvent::SourceFinished { .. } => "source_finished",
                })
                .collect();

            assert_eq!(
                kinds,
                vec![
                    "source_started",
                    "discovered",
                    "doc_started",
                    "doc_finished",
                    "doc_started",
                    "doc_finished",
                    "source_finished",
                ]
            );

            // The indexed resource must report Indexed{chunks > 0}; the
            // unsupported one must report Unsupported.
            match &events[3] {
                ProgressEvent::DocumentFinished {
                    outcome: DocOutcome::Indexed { chunks },
                    ..
                } => assert!(*chunks > 0),
                other => panic!("expected Indexed outcome, got {other:?}"),
            }
            match &events[5] {
                ProgressEvent::DocumentFinished {
                    outcome: DocOutcome::Unsupported,
                    ..
                } => {}
                other => panic!("expected Unsupported outcome, got {other:?}"),
            }
        }

        // -----------------------------------------------------------------
        // 3. Incremental skip via content_hash+policy in the callback
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn callback_skips_unchanged_content_and_policy() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let text = "Stable content that never changes.";
            let uri = "file:///docs/stable.md";
            let record = seed_indexed(&store, &embedder, &config, &source, uri, text).await;
            let chunk_count_before = store.stats().await.unwrap().chunk_count;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record);

            // The ingestor still yields the (unchanged) resource via on_resource —
            // the callback's own skip-check must catch it.
            let resource = make_resource(uri, text, &source.id, store_id);
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(resource)]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_indexed, 0);
            assert_eq!(result.docs_skipped, 1);
            let chunk_count_after = store.stats().await.unwrap().chunk_count;
            assert_eq!(
                chunk_count_before, chunk_count_after,
                "skip must not write any new chunks"
            );
        }

        // -----------------------------------------------------------------
        // 4. Policy-change forces re-index
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn policy_change_forces_reindex() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config_v1 = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let text = "Content whose policy will change.";
            let uri = "file:///docs/policy.md";
            let record = seed_indexed(&store, &embedder, &config_v1, &source, uri, text).await;
            let old_resource_id = record.resource_id.clone();

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record);

            let config_v2 = IngestionConfig {
                store_id: store_id.to_string(),
                policy_version: "policy-v2".to_string(),
                chunker: ChunkerConfig::prose(),
            };

            let resource = make_resource(uri, text, &source.id, store_id);
            let new_resource_id = resource.id.clone();
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(resource)]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config_v2,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_indexed, 1,
                "a policy change must force re-indexing even with unchanged content"
            );

            // Same URI + same content_hash ⇒ same content-addressed resource_id;
            // policy_version isn't a resource_id input, so the id is unchanged,
            // but the chunk's stored policy_version must reflect v2.
            assert_eq!(old_resource_id, new_resource_id);
            let chunks = store
                .get_chunks_for_resource(&new_resource_id)
                .await
                .unwrap();
            assert!(!chunks.is_empty());
            assert!(chunks.iter().all(|c| c.policy_version == "policy-v2"));
        }

        // -----------------------------------------------------------------
        // 4b. Cross-process rehydration: DocumentIndex::from_records +
        //     list_indexed_documents skips unchanged and reindexes changed
        //     resources on a simulated second process invocation.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn rehydrated_index_skips_unchanged_and_reindexes_changed() {
            use crate::store::RetrievalStore;

            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let stable_uri = "file:///docs/stable.md";
            let changing_uri = "file:///docs/changing.md";

            // First "process": full index via the scripted ingestor.
            let mut doc_index1 = DocumentIndex::new();
            let ingestor1 = FakeIngestor::new(vec![
                ScriptStep::Resource(make_resource(
                    stable_uri,
                    "Stable document content.",
                    &source.id,
                    store_id,
                )),
                ScriptStep::Resource(make_resource(
                    changing_uri,
                    "Original content.",
                    &source.id,
                    store_id,
                )),
            ]);
            let deps1 = SourceIngestionDeps {
                doc_index: &mut doc_index1,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result1 = run_source_ingestion(&source, &ingestor1, deps1)
                .await
                .unwrap();
            assert_eq!(result1.docs_indexed, 2);

            // Simulate a new process: rehydrate DocumentIndex from the store
            // rather than reusing the in-memory one from the first run.
            let records = store.list_indexed_documents().await.unwrap();
            assert_eq!(records.len(), 2, "store must have 2 distinct documents");
            let mut doc_index2 = DocumentIndex::from_records(records);

            // Second "process": re-run with one resource changed.
            let ingestor2 = FakeIngestor::new(vec![
                ScriptStep::Resource(make_resource(
                    stable_uri,
                    "Stable document content.",
                    &source.id,
                    store_id,
                )),
                ScriptStep::Resource(make_resource(
                    changing_uri,
                    "Completely new content.",
                    &source.id,
                    store_id,
                )),
            ]);
            let deps2 = SourceIngestionDeps {
                doc_index: &mut doc_index2,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result2 = run_source_ingestion(&source, &ingestor2, deps2)
                .await
                .unwrap();

            assert_eq!(
                result2.docs_indexed, 1,
                "only the changed doc should be re-indexed after rehydration"
            );
            assert_eq!(result2.docs_skipped, 1, "stable doc should be skipped");
        }

        // -----------------------------------------------------------------
        // 5/6. Delete-sweep: not-yielded URI is deleted; yielded URI is kept
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn delete_sweep_removes_uri_not_yielded_keeps_yielded() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let kept_uri = "file:///docs/kept.md";
            let gone_uri = "file:///docs/gone.md";
            let kept_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                kept_uri,
                "Kept content.",
            )
            .await;
            let gone_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                gone_uri,
                "Gone content.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(kept_record.clone());
            doc_index.upsert(gone_record.clone());

            // This run only yields `kept_uri` — `gone_uri` is simply absent,
            // exactly like a deleted file or a 404'd URL.
            let kept_resource = make_resource(kept_uri, "Kept content.", &source.id, store_id);
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(kept_resource)]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_deleted, 1);
            let gone_chunks = store
                .get_chunks_for_resource(&gone_record.resource_id)
                .await
                .unwrap();
            assert!(
                gone_chunks.is_empty(),
                "swept resource's chunks must be gone"
            );
            let kept_chunks = store
                .get_chunks_for_resource(&kept_record.resource_id)
                .await
                .unwrap();
            assert!(
                !kept_chunks.is_empty(),
                "yielded resource must survive the sweep"
            );
            assert!(doc_index.get(gone_uri).is_none());
            assert!(doc_index.get(kept_uri).is_some());
        }

        // -----------------------------------------------------------------
        // #185 / #156: "I observed nothing" is not "it was deleted".
        //
        // Three levels of the same conflation, guarded independently:
        //   - the sink   — a zero-chunk resource neither writes nor deletes;
        //   - guard 1    — an incomplete enumeration suppresses the sweep;
        //   - guard 2    — a run that saw none of the source's own URIs
        //                  suppresses the sweep whatever the ingestor claims.
        // -----------------------------------------------------------------

        /// #185 end-to-end: a zero-block `Resource` reaching `on_resource`
        /// must be reported as a skip, must not delete the URI's indexed
        /// content, and — the subtle part — must leave `doc_index` pointing
        /// at the OLD resource. Upserting the empty resource's id/hash while
        /// the store still holds the old resource's rows would leave the
        /// index referencing a resource_id with no rows behind it.
        #[tokio::test]
        async fn zero_block_resource_leaves_doc_index_pointing_at_old_resource() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let uri = "file:///docs/emptied.md";
            let old_record =
                seed_indexed(&store, &embedder, &config, &source, uri, "Original body.").await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(old_record.clone());

            // The file is still there and still enumerated — it just extracted
            // to nothing this run.
            let empty_resource = make_resource_with_blocks(uri, &source.id, store_id, vec![]);
            assert_ne!(
                empty_resource.id, old_record.resource_id,
                "sanity: the empty resource must have its own id, or this test \
                 could not distinguish 'index updated' from 'index left alone'"
            );
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(empty_resource)]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 0,
                "an empty extraction deletes nothing"
            );
            assert_eq!(
                result.docs_indexed, 0,
                "nothing was written, so nothing was indexed"
            );
            assert_eq!(result.docs_skipped, 1, "the empty resource is a skip");
            assert_eq!(result.error_count, 0, "an empty extraction is not an error");

            let old_chunks = store
                .get_chunks_for_resource(&old_record.resource_id)
                .await
                .unwrap();
            assert!(
                !old_chunks.is_empty(),
                "the previously indexed content must still be searchable"
            );

            let record = doc_index.get(uri).expect("the URI must survive the sweep");
            assert_eq!(
                record.resource_id, old_record.resource_id,
                "doc_index must still point at the resource whose rows the \
                 store actually holds"
            );
            assert_eq!(record.content_hash, old_record.content_hash);
        }

        /// Guard 1 (#156): an ingestor that reports `Enumeration::Incomplete`
        /// has told us it could not see the source. Its zero observations are
        /// no evidence of deletion, so the sweep must not run.
        #[tokio::test]
        async fn unavailable_enumeration_skips_sweep() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let uri = "file:///volumes/archive/book.md";
            let record = seed_indexed(&store, &embedder, &config, &source, uri, "Book text.").await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            // Zero callbacks of any kind — exactly what `FileIngestor` does
            // when its root is an unmounted volume.
            let ingestor =
                FakeIngestor::incomplete("source root is not reachable: /volumes/archive");

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 0,
                "an unreachable source must not delete its documents — this is \
                 the #156 incident in miniature"
            );
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(
                !chunks.is_empty(),
                "chunks must survive an unreachable root"
            );
            assert!(
                doc_index.get(uri).is_some(),
                "the doc_index record must survive too, or the next successful \
                 run would re-index everything from scratch"
            );
        }

        /// Guard 2 (#156): source-shape-agnostic backstop. Even when the
        /// ingestor claims a *complete* enumeration, a run that observed none
        /// of the URIs this source owns is far more likely to be a broken
        /// connector than a source whose entire contents vanished at once.
        #[tokio::test]
        async fn zero_seen_run_does_not_sweep_source_with_history() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let a = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                "file:///docs/a.md",
                "Alpha.",
            )
            .await;
            let b = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                "file:///docs/b.md",
                "Bravo.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(a.clone());
            doc_index.upsert(b.clone());

            // A well-behaved-looking run that nevertheless yielded nothing.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Discovered(0)]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted,
                0,
                "a run that saw none of the source's {} known URIs must not \
                 sweep them",
                doc_index.len()
            );
            for record in [&a, &b] {
                let chunks = store
                    .get_chunks_for_resource(&record.resource_id)
                    .await
                    .unwrap();
                assert!(!chunks.is_empty(), "chunks for {} must survive", record.uri);
            }
        }

        /// This behavior must stay exactly as it is: for a path/url source,
        /// the zero-seen backstop is the same shape of "this run should have
        /// produced full evidence and didn't" as guard 1, so it warns
        /// unconditionally, regardless of the feed branch's own move to
        /// `debug!` for the same guard (feed's routine steady state is a
        /// 304, which has no equivalent here).
        #[tokio::test]
        async fn zero_seen_suppression_on_a_path_source_still_logs_at_warn() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let a = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                "file:///docs/a.md",
                "Alpha.",
            )
            .await;
            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(a);

            let ingestor = FakeIngestor::new(vec![ScriptStep::Discovered(0)]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                fetcher: &UnreachableFetcher,
            };

            let (buf, guard) = capture_logs();
            run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();
            drop(guard);

            let captured = captured_text(&buf);
            assert!(
                captured.contains("WARN") && captured.contains("skipping delete-sweep"),
                "the path/url zero-seen backstop must still log at WARN, unchanged by \
                 the feed branch's move to debug for the same guard; captured: {captured}"
            );
        }

        /// Guard 2 must not over-suppress: seeing *any* owned URI licenses the
        /// sweep for the rest. (`delete_sweep_removes_uri_not_yielded_keeps_yielded`
        /// covers the same shape; this states the guard's boundary directly,
        /// with a source that owns several URIs and reports only one.)
        #[tokio::test]
        async fn sweep_still_runs_when_any_owned_uri_is_seen() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let kept_uri = "file:///docs/kept.md";
            let gone_a = "file:///docs/gone-a.md";
            let gone_b = "file:///docs/gone-b.md";

            let kept = seed_indexed(&store, &embedder, &config, &source, kept_uri, "Kept.").await;
            let a = seed_indexed(&store, &embedder, &config, &source, gone_a, "Gone A.").await;
            let b = seed_indexed(&store, &embedder, &config, &source, gone_b, "Gone B.").await;

            let mut doc_index = DocumentIndex::new();
            for record in [&kept, &a, &b] {
                doc_index.upsert(record.clone());
            }

            // One of three URIs observed — the other two really were deleted.
            let kept_resource = make_resource(kept_uri, "Kept.", &source.id, store_id);
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(kept_resource)]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 2,
                "legitimate deletion must still work — the guards suppress the \
                 sweep only when the run observed nothing at all"
            );
            assert!(doc_index.get(gone_a).is_none());
            assert!(doc_index.get(gone_b).is_none());
            assert!(doc_index.get(kept_uri).is_some());
        }

        // -----------------------------------------------------------------
        // DeletionPolicy::Retain — the default. Nothing is ever removed
        // unless the operator passes `--delete` (rsync semantics).
        // -----------------------------------------------------------------

        /// The default policy removes nothing and reports what `--delete`
        /// would have removed. This is the same fixture as
        /// `delete_sweep_removes_uri_not_yielded_keeps_yielded`, differing
        /// only in the policy — so the two together isolate the flag's effect.
        #[tokio::test]
        async fn retain_policy_keeps_absent_documents_and_counts_them_prunable() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let kept_uri = "file:///docs/kept.md";
            let gone_uri = "file:///docs/gone.md";
            let kept = seed_indexed(&store, &embedder, &config, &source, kept_uri, "Kept.").await;
            let gone = seed_indexed(&store, &embedder, &config, &source, gone_uri, "Gone.").await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(kept.clone());
            doc_index.upsert(gone.clone());

            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
                kept_uri, "Kept.", &source.id, store_id,
            ))]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Retain,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 0,
                "the default policy must never delete"
            );
            assert_eq!(
                result.docs_prunable, 1,
                "the absent document must be reported as prunable so the CLI \
                 can tell the user what --delete would remove"
            );
            let chunks = store
                .get_chunks_for_resource(&gone.resource_id)
                .await
                .unwrap();
            assert!(!chunks.is_empty(), "retained document's chunks stay");
            assert!(
                doc_index.get(gone_uri).is_some(),
                "a retained document must stay in the index too, or the next \
                 run would re-index it as new"
            );
        }

        /// Retention covers positively-confirmed deletions as well. An
        /// archived copy of a page that has since 404'd is often the most
        /// valuable thing in the index — "the origin dropped it" is not "you
        /// wanted it dropped."
        #[tokio::test]
        async fn retain_policy_keeps_confirmed_gone_documents() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Url,
                spec: SourceSpec::Url {
                    url: "https://example.com/article".to_string(),
                    refresh_interval_secs: None,
                },
                source_preset: "prose".to_string(),
            };

            let url = "https://example.com/article";
            let record =
                seed_indexed(&store, &embedder, &config, &source, url, "Article body.").await;
            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            let ingestor = FakeIngestor::new(vec![ScriptStep::Gone(url.to_string())]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Retain,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_deleted, 0);
            assert_eq!(result.docs_prunable, 1);
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(
                !chunks.is_empty(),
                "a 404'd article stays searchable by default"
            );
        }

        /// A guard-suppressed sweep must NOT inflate `docs_prunable`: those
        /// documents would not be removed even under `--delete`, so telling
        /// the user "N could be pruned" would be a lie that invites them to
        /// pass the flag expecting a cleanup that cannot happen.
        #[tokio::test]
        async fn suppressed_sweep_reports_nothing_prunable() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                "file:///volumes/archive/a.md",
                "Body.",
            )
            .await;
            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            let ingestor =
                FakeIngestor::incomplete("source root is not reachable: /volumes/archive");
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Retain,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_deleted, 0);
            assert_eq!(
                result.docs_prunable, 0,
                "an unreachable root makes nothing prunable — --delete would \
                 not remove these either"
            );
        }

        /// Guard 2 must not fire for a source with no history: a brand-new
        /// source that legitimately enumerates zero documents has nothing to
        /// preserve, and suppressing its (no-op) sweep would be meaningless.
        /// Stated as a test so the "N > 0" half of the condition can't be
        /// dropped silently.
        #[tokio::test]
        async fn zero_seen_run_on_source_without_history_is_harmless() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");
            let other = make_source_with_preset(store_id, "prose");

            // A sibling source's document — this source owns nothing.
            let foreign = seed_indexed(
                &store,
                &embedder,
                &config,
                &other,
                "file:///other/x.md",
                "Foreign.",
            )
            .await;
            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(foreign.clone());

            let ingestor = FakeIngestor::new(vec![ScriptStep::Discovered(0)]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_deleted, 0);
            let chunks = store
                .get_chunks_for_resource(&foreign.resource_id)
                .await
                .unwrap();
            assert!(
                !chunks.is_empty(),
                "another source's document is never this source's to sweep"
            );
        }

        // -----------------------------------------------------------------
        // 5b. Regression: delete-sweep must fire for a file under a
        // space-containing root. Before the sweep filtered by `source_id`,
        // it matched URIs against a prefix built from the raw
        // (non-percent-encoded) canonical root, which never matched the
        // percent-encoded `Resource.uri` a real file ingestor produces —
        // so a deleted file under such a root was silently never swept
        // (stale chunks live forever).
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn delete_sweep_removes_file_under_space_containing_root() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);

            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("My Docs")).unwrap();
            std::fs::write(
                dir.path().join("My Docs").join("note.md"),
                b"Space root content.",
            )
            .unwrap();
            // A second file that survives this run. Without it the source
            // would own exactly one URI and observe none of them, tripping
            // the #156 zero-seen guard — which would mask what this test is
            // actually about (URI encoding in the sweep's ownership check).
            std::fs::write(
                dir.path().join("My Docs").join("keep.md"),
                b"Still here content.",
            )
            .unwrap();
            let root = dir.path().join("My Docs").canonicalize().unwrap();

            let source = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                spec: SourceSpec::Path {
                    root: root.to_str().unwrap().to_string(),
                    include: vec![],
                    exclude: vec![],
                },
                source_preset: "prose".to_string(),
            };

            // Enumerate for real — this is exactly how the URI the doc_index
            // stores is shaped in production (`FoundFile.uri` is already a
            // normalized `Uri`, built via `Uri::from_file_path`).
            let found = enumerate_path_source(root.to_str().unwrap(), &[], &[])
                .unwrap()
                .files()
                .to_vec();
            assert_eq!(found.len(), 2);
            let uri_of = |name: &str| {
                found
                    .iter()
                    .find(|f| f.path.ends_with(name))
                    .unwrap_or_else(|| panic!("{name} must be enumerated"))
                    .uri
                    .clone()
            };
            let normalized_uri = uri_of("note.md");
            let kept_uri = uri_of("keep.md");
            assert!(
                normalized_uri.as_str().contains("My%20Docs"),
                "sanity: the space must be percent-encoded in the indexed URI"
            );

            let record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                normalized_uri.as_str(),
                "Space root content.",
            )
            .await;
            let kept_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                kept_uri.as_str(),
                "Still here content.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());
            doc_index.upsert(kept_record.clone());

            // Simulate `note.md` having been deleted from disk: this run
            // yields only `keep.md`.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
                kept_uri.as_str(),
                "Still here content.",
                &source.id,
                store_id,
            ))]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 1,
                "the file under the space-containing root must be swept"
            );
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(chunks.is_empty(), "swept resource's chunks must be gone");
            assert!(doc_index.get(normalized_uri.as_str()).is_none());
            assert!(
                doc_index.get(kept_uri.as_str()).is_some(),
                "the still-present file under the same root must survive"
            );
        }

        /// Same shape as the space-root sweep above, but with a reserved URI
        /// delimiter in the root. `Uri::from_file_path` encodes `#` as `%23`,
        /// while URI-shape heuristics built on `Uri::parse` truncate at `#`
        /// (it opens a fragment) — historically that divergence made the
        /// sweep silently skip such records, leaving the deleted file's
        /// chunks searchable forever. Ownership by `source_id` is immune to
        /// the root's encoding; this pins that.
        #[cfg(unix)]
        #[tokio::test]
        async fn delete_sweep_removes_file_under_hash_containing_root() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);

            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("my#notes")).unwrap();
            std::fs::write(
                dir.path().join("my#notes").join("note.md"),
                b"Hash root content.",
            )
            .unwrap();
            // Second file survives this run — see the space-root test above
            // for why a lone owned URI would trip the #156 zero-seen guard
            // and mask what this test is pinning.
            std::fs::write(
                dir.path().join("my#notes").join("keep.md"),
                b"Still here content.",
            )
            .unwrap();
            let root = dir.path().join("my#notes").canonicalize().unwrap();

            let source = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                spec: SourceSpec::Path {
                    root: root.to_str().unwrap().to_string(),
                    include: vec![],
                    exclude: vec![],
                },
                source_preset: "prose".to_string(),
            };

            let found = enumerate_path_source(root.to_str().unwrap(), &[], &[])
                .unwrap()
                .files()
                .to_vec();
            assert_eq!(found.len(), 2);
            let uri_of = |name: &str| {
                found
                    .iter()
                    .find(|f| f.path.ends_with(name))
                    .unwrap_or_else(|| panic!("{name} must be enumerated"))
                    .uri
                    .clone()
            };
            let normalized_uri = uri_of("note.md");
            let kept_uri = uri_of("keep.md");
            assert!(
                normalized_uri.as_str().contains("my%23notes"),
                "sanity: the `#` must be percent-encoded in the indexed URI"
            );

            let record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                normalized_uri.as_str(),
                "Hash root content.",
            )
            .await;
            let kept_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                kept_uri.as_str(),
                "Still here content.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());
            doc_index.upsert(kept_record.clone());

            // `note.md` is gone from disk: this run yields only `keep.md`.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
                kept_uri.as_str(),
                "Still here content.",
                &source.id,
                store_id,
            ))]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 1,
                "the file under the `#`-containing root must be swept"
            );
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(chunks.is_empty(), "swept resource's chunks must be gone");
            assert!(doc_index.get(normalized_uri.as_str()).is_none());
        }

        // -----------------------------------------------------------------
        // 6b. C0 regression: delete-sweep boundary safety across sibling
        //     path sources whose roots are string prefixes of each other
        //     (e.g. /data/blog vs /data/blog-drafts). Sweeping source A must
        //     never delete source B's live resources just because B's root
        //     string happens to start with A's root string.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn delete_sweep_does_not_cross_sibling_prefix_sources() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);

            let base = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(base.path().join("blog")).unwrap();
            std::fs::create_dir_all(base.path().join("blog-drafts")).unwrap();
            let blog_root = base.path().join("blog").canonicalize().unwrap();
            let blog_drafts_root = base.path().join("blog-drafts").canonicalize().unwrap();

            let source_a = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                spec: SourceSpec::Path {
                    root: blog_root.to_str().unwrap().to_string(),
                    include: vec![],
                    exclude: vec![],
                },
                source_preset: "prose".to_string(),
            };
            let source_b = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                spec: SourceSpec::Path {
                    root: blog_drafts_root.to_str().unwrap().to_string(),
                    include: vec![],
                    exclude: vec![],
                },
                source_preset: "prose".to_string(),
            };

            let uri_a = format!("file://{}/post.md", blog_root.display());
            let uri_a_kept = format!("file://{}/kept.md", blog_root.display());
            let uri_b = format!("file://{}/draft.md", blog_drafts_root.display());

            // Both sources' documents share the same store-level doc_index —
            // exactly the shared-store scenario the finding describes.
            let record_a =
                seed_indexed(&store, &embedder, &config, &source_a, &uri_a, "Blog post.").await;
            // A second document under source A that survives this run. Source
            // A must observe at least one of its own URIs or the #156
            // zero-seen guard suppresses its sweep entirely, which would make
            // this test vacuous rather than failing loudly.
            let record_a_kept = seed_indexed(
                &store,
                &embedder,
                &config,
                &source_a,
                &uri_a_kept,
                "Kept post.",
            )
            .await;
            let record_b = seed_indexed(
                &store,
                &embedder,
                &config,
                &source_b,
                &uri_b,
                "Draft content.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record_a.clone());
            doc_index.upsert(record_a_kept.clone());
            doc_index.upsert(record_b.clone());

            // Sweep source A only: `post.md` is gone from disk, `kept.md`
            // still there. Source B's ingestor does NOT run this cycle.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(make_resource(
                &uri_a_kept,
                "Kept post.",
                &source_a.id,
                store_id,
            ))]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source_a, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 1,
                "only source A's own (now-absent) document is swept"
            );
            let a_chunks = store
                .get_chunks_for_resource(&record_a.resource_id)
                .await
                .unwrap();
            assert!(a_chunks.is_empty(), "source A's document must be deleted");

            let b_chunks = store
                .get_chunks_for_resource(&record_b.resource_id)
                .await
                .unwrap();
            assert!(
                !b_chunks.is_empty(),
                "source B's document must survive sweeping source A, even though \
                 B's root string starts with A's root string"
            );
            assert!(
                doc_index.get(&record_b.uri).is_some(),
                "source B's doc_index record must remain"
            );
        }

        /// Percent-encoding twin roots: source A's root is the *literal*
        /// directory name `foo%23`, source B's root is `foo#`. B's documents
        /// are stored under `file://…/foo%23/…` (canonical
        /// `Uri::from_file_path` encodes `#` as `%23`) — byte-identical to
        /// what a `Uri::parse`-built prefix for A's root produces, since
        /// `%23` is already a valid percent-encoding that `Url::parse`
        /// preserves. Any string-prefix heuristic therefore attributes B's
        /// live rows to A, and sweeping only source A deletes B's documents.
        /// The sweep must decide ownership by `source_id`, not by URI shape.
        #[cfg(unix)]
        #[tokio::test]
        async fn delete_sweep_does_not_cross_percent_encoded_twin_roots() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);

            let base = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(base.path().join("foo%23")).unwrap();
            std::fs::create_dir_all(base.path().join("foo#")).unwrap();
            std::fs::write(
                base.path().join("foo#").join("doc.md"),
                b"Twin root content.",
            )
            .unwrap();
            let root_a = base.path().join("foo%23").canonicalize().unwrap();
            let root_b = base.path().join("foo#").canonicalize().unwrap();

            let source_a = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                spec: SourceSpec::Path {
                    root: root_a.to_str().unwrap().to_string(),
                    include: vec![],
                    exclude: vec![],
                },
                source_preset: "prose".to_string(),
            };
            let source_b = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Path,
                spec: SourceSpec::Path {
                    root: root_b.to_str().unwrap().to_string(),
                    include: vec![],
                    exclude: vec![],
                },
                source_preset: "prose".to_string(),
            };

            // Enumerate B's root for real, so the stored URI is shaped exactly
            // as production shapes it.
            let found = enumerate_path_source(root_b.to_str().unwrap(), &[], &[])
                .unwrap()
                .files()
                .to_vec();
            assert_eq!(found.len(), 1);
            let uri_b = found[0].uri.as_str().to_string();
            assert!(
                uri_b.contains("foo%23/"),
                "sanity: B's canonical URI must encode `#` as `%23`, making it \
                 collide with A's literal `foo%23` root"
            );

            let record_b = seed_indexed(
                &store,
                &embedder,
                &config,
                &source_b,
                &uri_b,
                "Twin root content.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record_b.clone());

            // Sweep source A only (its directory is empty; B does not run
            // this cycle — e.g. `index --source A`).
            let ingestor = FakeIngestor::new(vec![]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source_a, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 0,
                "sweeping source A must not delete source B's live document, \
                 even though A's literal `foo%23` root and B's encoded `foo#` \
                 root produce byte-identical URI prefixes"
            );
            let b_chunks = store
                .get_chunks_for_resource(&record_b.resource_id)
                .await
                .unwrap();
            assert!(
                !b_chunks.is_empty(),
                "source B's chunks must survive sweeping source A"
            );
            assert!(
                doc_index.get(&record_b.uri).is_some(),
                "source B's doc_index record must remain"
            );
        }

        // -----------------------------------------------------------------
        // 7. on_skipped(Unchanged) marks the URI seen — survives the sweep
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn on_skipped_unchanged_survives_delete_sweep() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let uri = "file:///docs/prefiltered.md";
            let record = seed_indexed(&store, &embedder, &config, &source, uri, "Content.").await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            // The ingestor pre-filters this URI itself (e.g. mtime unchanged)
            // and never calls on_resource for it at all.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Skipped(
                uri.to_string(),
                SkipReason::Unchanged,
            )]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_deleted, 0);
            assert_eq!(result.docs_skipped, 1);
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(
                !chunks.is_empty(),
                "on_skipped(Unchanged) must not delete existing chunks"
            );
            assert!(doc_index.get(uri).is_some());
        }

        // -----------------------------------------------------------------
        // 8. A confirmed-Gone URL is deleted (Url-kind source).
        //
        // Renamed from `gone_url_style_absence_is_swept`: since #156 the
        // deletion no longer rides on *absence* — the ingestor reports the
        // 404/410 positively via `on_gone`, and that path is exempt from the
        // sweep guards precisely because nothing about it is inferred.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn confirmed_gone_url_is_deleted() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Url,
                spec: SourceSpec::Url {
                    url: "https://example.com/page".to_string(),
                    refresh_interval_secs: None,
                },
                source_preset: "prose".to_string(),
            };

            let url = "https://example.com/page";
            let record = seed_indexed(&store, &embedder, &config, &source, url, "Page body.").await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            // The URL now 404s/410s. `UrlIngestor` reports that positively via
            // `on_gone` rather than by staying silent: since #156 an absence
            // alone no longer licenses a delete, but a confirmed 410 is
            // knowledge — the origin answered — so it deletes regardless of
            // the sweep guards.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Gone(url.to_string())]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.docs_deleted, 1);
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(chunks.is_empty());
        }

        // -----------------------------------------------------------------
        // 8f. C1: feed sources are exempt from the delete-sweep. A feed only
        // ever exposes its most-recent N entries, so a zero-callback run
        // (absent entries scrolled off the window, or a feed-level 304 Not
        // Modified) must NOT delete previously-indexed entries — while a url
        // source that positively confirms its URL is Gone must still delete
        // it. Test 8 above covers the url half alone; this test additionally
        // proves the two behaviors coexist correctly in the same
        // store/doc_index.
        //
        // Note what changed with #156: the two scenarios are no longer
        // "identically-shaped zero-callback runs" distinguished only by
        // source kind. The url source now *says* the URL is gone. Silence
        // means the same thing for both kinds now — no evidence — which is
        // why the feed exemption and the sweep guards can coexist without
        // one having to special-case the other.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn feed_zero_callback_run_is_not_swept_but_confirmed_gone_url_is_deleted() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);

            let feed_source = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Feed,
                spec: SourceSpec::Feed {
                    url: "https://example.com/feed.xml".to_string(),
                    max_entries: None,
                    fetch_full_content: true,
                    refresh_interval_secs: None,
                },
                source_preset: "prose".to_string(),
            };
            let url_source = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Url,
                spec: SourceSpec::Url {
                    url: "https://example.com/page".to_string(),
                    refresh_interval_secs: None,
                },
                source_preset: "prose".to_string(),
            };

            let feed_entry_uri = "https://example.com/feed.xml#entry:1";
            let url_uri = "https://example.com/page";

            let feed_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &feed_source,
                feed_entry_uri,
                "Feed entry body.",
            )
            .await;
            let url_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &url_source,
                url_uri,
                "Page body.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(feed_record.clone());
            doc_index.upsert(url_record.clone());

            // The feed's ingestor yields nothing at all — a feed-level 304 Not
            // Modified, or the entry simply having scrolled off the feed's
            // window. Silence, carrying no information.
            let feed_ingestor = FakeIngestor::new(vec![]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let feed_result = run_source_ingestion(&feed_source, &feed_ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                feed_result.docs_deleted, 0,
                "feed sources are exempt from the delete-sweep — a zero-callback \
                 run must not delete"
            );
            let feed_chunks = store
                .get_chunks_for_resource(&feed_record.resource_id)
                .await
                .unwrap();
            assert!(
                !feed_chunks.is_empty(),
                "feed entry's chunks must survive an unswept run"
            );
            assert!(doc_index.get(feed_entry_uri).is_some());

            // The url source's fetch came back 404/410 — knowledge, reported
            // positively.
            let url_ingestor = FakeIngestor::new(vec![ScriptStep::Gone(url_uri.to_string())]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let url_result = run_source_ingestion(&url_source, &url_ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                url_result.docs_deleted, 1,
                "a confirmed-Gone URL in the very same store/doc_index is still \
                 deleted — the feed exemption is about absence, not about \
                 refusing to act on knowledge"
            );
            let url_chunks = store
                .get_chunks_for_resource(&url_record.resource_id)
                .await
                .unwrap();
            assert!(
                url_chunks.is_empty(),
                "swept url resource's chunks must be gone"
            );
        }

        #[tokio::test]
        async fn source_location_feed_arm_returns_url() {
            let source = Source {
                id: new_ulid(),
                store_id: "store-1".to_string(),
                kind: SourceKind::Feed,
                spec: SourceSpec::Feed {
                    url: "https://example.com/feed.xml".to_string(),
                    max_entries: None,
                    fetch_full_content: true,
                    refresh_interval_secs: None,
                },
                source_preset: "prose".to_string(),
            };
            assert_eq!(source_location(&source), "https://example.com/feed.xml");
        }

        // -----------------------------------------------------------------
        // 8b-8e (removed by the `on_skipped(&Uri, ...)` signature change):
        // these four tests fed a RAW locator string through
        // `ScriptStep::Skipped` to prove `PipelineCallback::on_skipped`
        // normalized it before using it for `seen`/progress bookkeeping.
        // Once `Ingestor::on_skipped` takes `&Uri` instead of `&str`, there
        // is no longer any way to construct that raw input at all —
        // `FakeIngestor` itself must call `Uri::parse` on the script's
        // string before handing it to `on_skipped`, so any space/casing
        // divergence is already gone by the time production code sees it.
        // The tests would still pass with the normalization call deleted
        // from `on_skipped` entirely (which this commit does): there is no
        // longer a single-line revert of production code that makes any of
        // them fail, which makes them tautological guards, not regression
        // tests. They are deleted rather than kept as dead weight.
        //
        // The unparseable-locator fallback test is replaced by
        // `ingest::url_ingestor`'s `invalid_config_url_fails_fast`, which
        // tests the only place that class of input can still occur: a raw,
        // never-validated config string, now rejected eagerly by the
        // hoisted `Uri::parse` at the top of `UrlIngestor::ingest`.
        //
        // The durable, non-tautological regression coverage for the
        // original bug lives in
        // `ingest/tests/file_ingestor_sweep_regression.rs`, which drives the
        // real `FileIngestor` over a real space-named file end to end and
        // does not go through `FakeIngestor` at all.

        // -----------------------------------------------------------------
        // 9. A per-resource error doesn't abort the run — later resources
        //    still index
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn per_resource_error_does_not_abort_later_resources_still_index() {
            let store = FakeStore::new();
            let embedder = SelectiveFailEmbedder {
                fail_marker: "FAIL_MARKER",
                inner: FakeEmbedder::new(4),
            };
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let first = make_resource(
                "file:///docs/first.md",
                "First good content.",
                &source.id,
                store_id,
            );
            let bad = make_resource(
                "file:///docs/bad.md",
                "This has FAIL_MARKER in it.",
                &source.id,
                store_id,
            );
            let last = make_resource(
                "file:///docs/last.md",
                "Last good content.",
                &source.id,
                store_id,
            );
            let first_id = first.id.clone();
            let last_id = last.id.clone();

            let ingestor = FakeIngestor::new(vec![
                ScriptStep::Resource(first),
                ScriptStep::Resource(bad),
                ScriptStep::Resource(last),
            ]);

            let mut doc_index = DocumentIndex::new();
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.error_count, 1);
            assert_eq!(result.docs_indexed, 2, "the two good resources both index");
            assert!(!store
                .get_chunks_for_resource(&first_id)
                .await
                .unwrap()
                .is_empty());
            assert!(!store
                .get_chunks_for_resource(&last_id)
                .await
                .unwrap()
                .is_empty());
        }

        // -----------------------------------------------------------------
        // 10. Embed-failure ⇒ error counted, no delete of existing chunks
        //     (crash-safety, A6)
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn embed_failure_preserves_existing_chunks_and_counts_error() {
            let store = FakeStore::new();
            let good_embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let uri = "file:///docs/doc.md";
            let record = seed_indexed(
                &store,
                &good_embedder,
                &config,
                &source,
                uri,
                "Original content for the document.",
            )
            .await;
            let original_id = record.resource_id.clone();

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record);

            let changed = make_resource(
                uri,
                "Changed content that triggers re-indexing.",
                &source.id,
                store_id,
            );
            let ingestor = FakeIngestor::new(vec![ScriptStep::Resource(changed)]);

            let failing_embedder = FailingEmbedder;
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &failing_embedder,
                config: &config,
                progress: None,
                deletion: DeletionPolicy::Prune,
                document_validators: FetchMetadata::default(),
                stored_inputs_digest: None,
                fetcher: &UnreachableFetcher,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(result.error_count, 1);
            assert_eq!(result.docs_indexed, 0);
            let chunks = store.get_chunks_for_resource(&original_id).await.unwrap();
            assert!(
                !chunks.is_empty(),
                "a failed re-index must never delete the previously-indexed chunks"
            );
            // doc_index must still point at the old (still-present) resource_id.
            assert_eq!(doc_index.get(uri).unwrap().resource_id, original_id);
        }

        /// F4: a short embedder response (fewer vectors than chunks) returns
        /// an Internal error from `index_resource`.
        #[tokio::test]
        async fn index_resource_short_embedder_returns_error() {
            let store = FakeStore::new();
            let short_embedder = ShortEmbedder { dim: 4 };
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let resource = make_resource(
                "file:///docs/short.md",
                "Content that produces at least one chunk.",
                &source.id,
                store_id,
            );

            let deps = IndexResourceDeps {
                store: &store,
                embedder: &short_embedder,
                config: &config,
            };
            let result = index_resource(&resource, &source, None, &deps).await;

            assert!(
                result.is_err(),
                "must return an error when embedder returns fewer vectors than chunks"
            );
            assert!(
                matches!(result.unwrap_err(), Error::Internal { .. }),
                "error must be Internal"
            );
        }

        // -----------------------------------------------------------------
        // 10b. Replace wiring (issue #79): a single upsert_chunks_and_blocks
        //      call folds the delete in, rather than a separate delete call.
        // -----------------------------------------------------------------

        /// One recorded `upsert_chunks_and_blocks` call: `(store_id, resource_id,
        /// records.len(), replaces_resource_id)`.
        type UpsertCall = (String, String, usize, Option<String>);

        /// Wraps a `FakeStore`, recording every `delete_by_resource` and
        /// `upsert_chunks_and_blocks` call so tests can assert on *how*
        /// `index_resource` drives the store, not just the end state.
        ///
        /// `upsert_chunks_and_blocks` can be told to fail via `fail_next_upsert`;
        /// when it does, it returns an error *without* touching the underlying
        /// `FakeStore` at all (neither delete nor insert), simulating the
        /// all-or-nothing behavior a real atomic transaction provides. This lets
        /// tests verify that `index_resource` itself never performs a separate
        /// delete before calling `upsert_chunks_and_blocks` — if it did, the old
        /// resource would be gone even though the replace as a whole failed.
        struct RecordingStore {
            inner: FakeStore,
            delete_calls: tokio::sync::Mutex<Vec<String>>,
            upsert_calls: tokio::sync::Mutex<Vec<UpsertCall>>,
            fail_next_upsert: std::sync::atomic::AtomicBool,
        }

        impl RecordingStore {
            fn new() -> Self {
                Self {
                    inner: FakeStore::new(),
                    delete_calls: tokio::sync::Mutex::new(Vec::new()),
                    upsert_calls: tokio::sync::Mutex::new(Vec::new()),
                    fail_next_upsert: std::sync::atomic::AtomicBool::new(false),
                }
            }

            fn fail_next_upsert(&self) {
                self.fail_next_upsert
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }

            async fn delete_calls(&self) -> Vec<String> {
                self.delete_calls.lock().await.clone()
            }

            async fn upsert_calls(&self) -> Vec<UpsertCall> {
                self.upsert_calls.lock().await.clone()
            }
        }

        #[async_trait::async_trait]
        impl RetrievalStore for RecordingStore {
            async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
                self.inner.upsert_chunks(records).await
            }

            async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
                self.delete_calls.lock().await.push(resource_id.to_string());
                self.inner.delete_by_resource(resource_id).await
            }

            async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
                self.inner.delete_by_store(store_id).await
            }

            async fn dense_search(
                &self,
                query_vector: &[f32],
                limit: usize,
                filters: &[crate::store::MetadataFilter],
            ) -> Result<Vec<crate::store::SearchResult>, Error> {
                self.inner.dense_search(query_vector, limit, filters).await
            }

            async fn bm25_search(
                &self,
                query_text: &str,
                limit: usize,
                filters: &[crate::store::MetadataFilter],
            ) -> Result<Vec<crate::store::SearchResult>, Error> {
                self.inner.bm25_search(query_text, limit, filters).await
            }

            async fn stats(&self) -> Result<crate::store::StoreStats, Error> {
                self.inner.stats().await
            }

            async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
                self.inner.get_chunk(chunk_id).await
            }

            async fn get_chunks_for_resource(
                &self,
                resource_id: &str,
            ) -> Result<Vec<ChunkRecord>, Error> {
                self.inner.get_chunks_for_resource(resource_id).await
            }

            async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
                self.inner.list_indexed_documents().await
            }

            async fn update_resource_metadata(
                &self,
                store_id: &str,
                resource_id: &str,
                record: &crate::store::ResourceRecord,
            ) -> Result<(), Error> {
                self.inner
                    .update_resource_metadata(store_id, resource_id, record)
                    .await
            }

            async fn get_resource_record(
                &self,
                store_id: &str,
                resource_id: &str,
            ) -> Result<Option<crate::store::ResourceRecord>, Error> {
                self.inner.get_resource_record(store_id, resource_id).await
            }

            async fn upsert_chunks_and_blocks(
                &self,
                store_id: &str,
                resource_id: &str,
                records: Vec<ChunkRecord>,
                blocks: &[crate::block::Block],
                replaces_resource_id: Option<&str>,
                _external_last_modified: Option<&str>,
            ) -> Result<usize, Error> {
                self.upsert_calls.lock().await.push((
                    store_id.to_string(),
                    resource_id.to_string(),
                    records.len(),
                    replaces_resource_id.map(str::to_string),
                ));

                if self
                    .fail_next_upsert
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(Error::Internal {
                        message: "simulated upsert failure".to_string(),
                        correlation_id: "recording_store_simulated_failure".to_string(),
                    });
                }

                // Simulate the atomic contract: delete-then-insert, both only
                // observable together since we only reach here when not failing.
                if let Some(old_id) = replaces_resource_id {
                    self.inner.delete_by_resource(old_id).await?;
                }
                let count = self.inner.upsert_chunks(records).await?;
                self.inner
                    .upsert_blocks(store_id, resource_id, blocks)
                    .await?;
                Ok(count)
            }
        }

        #[tokio::test]
        async fn index_resource_replace_uses_single_call_not_separate_delete() {
            let store = RecordingStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");
            let uri = "file:///docs/notes.md";

            let resource_v1 = make_resource(uri, "Version one content.", &source.id, store_id);
            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            index_resource(&resource_v1, &source, None, &deps)
                .await
                .unwrap();
            let old_doc_id = resource_v1.id.clone();

            let resource_v2 = make_resource(
                uri,
                "Version two content - completely different.",
                &source.id,
                store_id,
            );
            index_resource(&resource_v2, &source, Some(&old_doc_id), &deps)
                .await
                .unwrap();

            assert!(
                store.delete_calls().await.is_empty(),
                "index_resource must never call delete_by_resource directly on a \
                 content-changed replace — the delete must be folded into the \
                 upsert_chunks_and_blocks call"
            );

            let upserts = store.upsert_calls().await;
            assert_eq!(upserts.len(), 2, "one upsert call per index_resource call");
            assert_eq!(
                upserts[0].3, None,
                "first index (no prior document) must not pass replaces_resource_id"
            );
            assert_eq!(
                upserts[1].3,
                Some(old_doc_id),
                "changed-content re-index must pass the old resource_id as \
                 replaces_resource_id"
            );
        }

        #[tokio::test]
        async fn index_resource_replace_failure_leaves_old_document_intact() {
            let store = RecordingStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");
            let uri = "file:///docs/notes.md";

            let resource_v1 = make_resource(uri, "Version one content.", &source.id, store_id);
            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            index_resource(&resource_v1, &source, None, &deps)
                .await
                .unwrap();
            let old_doc_id = resource_v1.id.clone();

            let old_chunks_before = store.get_chunks_for_resource(&old_doc_id).await.unwrap();
            assert_eq!(old_chunks_before.len(), 1);

            // Arm the store to fail the *next* upsert_chunks_and_blocks call —
            // i.e. the replace triggered by the content change below.
            store.fail_next_upsert();

            let resource_v2 = make_resource(
                uri,
                "Version two content - completely different.",
                &source.id,
                store_id,
            );
            let result = index_resource(&resource_v2, &source, Some(&old_doc_id), &deps).await;
            assert!(result.is_err(), "the simulated upsert failure must surface");

            // The old document's chunks must still be retrievable — the failed
            // replace must not have removed them via a separate delete call.
            let old_chunks_after = store.get_chunks_for_resource(&old_doc_id).await.unwrap();
            assert_eq!(
                old_chunks_after.len(),
                1,
                "old document chunks must survive a failed replace"
            );
        }

        // -----------------------------------------------------------------
        // 11. window_block_seqs flow through to upserted ChunkRecords for a
        //     messages-preset resource
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn window_block_seqs_flow_through_for_messages_preset() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "messages");
            let config = IngestionConfig {
                store_id: store_id.to_string(),
                policy_version: "policy-v1".to_string(),
                chunker: ChunkerConfig {
                    preset: "messages".to_string(),
                    target_tokens: Some(512),
                    overlap_tokens: Some(0),
                    window_turns: Some(2),
                    stride_turns: Some(1),
                },
            };

            let blocks: Vec<Block> = (0..5)
                .map(|i| Block {
                    seq: i,
                    kind: BlockKind::Message {
                        sender: "alice".to_string(),
                        timestamp: None,
                        message_id: None,
                        reply_to: None,
                    },
                    text: format!("message number {i}"),
                    location: None,
                })
                .collect();

            let resource =
                make_resource_with_blocks("file:///chat/thread.json", &source.id, store_id, blocks);

            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            index_resource(&resource, &source, None, &deps)
                .await
                .unwrap();

            let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
            assert!(!chunks.is_empty());
            assert!(
                chunks.iter().any(|c| c.window_block_seqs.len() >= 2),
                "at least one window chunk must span multiple blocks; got: {:?}",
                chunks
                    .iter()
                    .map(|c| &c.window_block_seqs)
                    .collect::<Vec<_>>()
            );
        }

        // -----------------------------------------------------------------
        // 12. Preset gate (#60) — direct unit tests on effective_chunker_config
        // -----------------------------------------------------------------

        #[test]
        fn preset_gate_explicit_code_source_wins_over_md_extension() {
            let base = ChunkerConfig::code();
            let cfg = effective_chunker_config("code", &base, Some("notes.md"), None);
            assert_eq!(cfg.preset, "code");
        }

        #[test]
        fn preset_gate_default_prose_source_auto_routes_rs_file_to_code() {
            let base = ChunkerConfig::prose();
            let cfg = effective_chunker_config("prose", &base, Some("main.rs"), None);
            assert_eq!(cfg.preset, "code");
        }

        #[test]
        fn preset_gate_messages_source_wins_regardless_of_filename() {
            let base = ChunkerConfig::messages();
            let cfg = effective_chunker_config("messages", &base, Some("transcript.md"), None);
            assert_eq!(cfg.preset, "messages");
            assert_eq!(cfg.resolved_window_turns(), 6);
        }

        /// Integration-level check that the preset gate is actually wired into
        /// `index_resource`: an explicit `code` source must not apply the
        /// prose splitter's heading-path attribution to a Markdown file.
        #[tokio::test]
        async fn index_resource_respects_explicit_code_source_preset() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "code");
            let config = IngestionConfig {
                store_id: store_id.to_string(),
                policy_version: "policy-v1".to_string(),
                chunker: ChunkerConfig::code(),
            };

            let resource = make_resource(
                "file:///docs/notes.md",
                "# Heading\n\nSome prose-looking text under a heading.",
                &source.id,
                store_id,
            );

            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            index_resource(&resource, &source, None, &deps)
                .await
                .unwrap();

            let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
            assert!(!chunks.is_empty());
            // The code chunker never derives heading_path (unlike chunk_prose,
            // which would attribute "Heading" here).
            assert!(
                chunks.iter().all(|c| c.heading_path.is_empty()),
                "an explicit code source must route through the code chunker, \
                 not the heading-path-aware prose chunker"
            );
        }

        // -----------------------------------------------------------------
        // 13. Title propagation: Resource.title/metadata → ChunkRecord.metadata title
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn title_propagates_from_resource_title_when_metadata_has_none() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);

            let mut resource = make_resource(
                "file:///docs/titled.md",
                "Body content for the titled document.",
                &source.id,
                store_id,
            );
            resource.title = Some("My Great Title".to_string());
            // metadata's own Dublin Core title is left None (default).

            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            index_resource(&resource, &source, None, &deps)
                .await
                .unwrap();

            let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
            assert!(!chunks.is_empty());
            for c in &chunks {
                assert_eq!(c.metadata.title(), Some("My Great Title"));
            }
        }

        #[tokio::test]
        async fn title_from_metadata_is_not_overwritten_by_resource_title() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);

            let mut resource = make_resource(
                "file:///docs/titled2.md",
                "Body content for the second titled document.",
                &source.id,
                store_id,
            );
            resource.title = Some("Fallback Title".to_string());
            resource.metadata = Metadata::Document(DocumentMetadata {
                dublin_core: DublinCoreMetadata {
                    title: Some("Authoritative Title".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            });

            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            index_resource(&resource, &source, None, &deps)
                .await
                .unwrap();

            let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
            assert!(!chunks.is_empty());
            for c in &chunks {
                assert_eq!(c.metadata.title(), Some("Authoritative Title"));
            }
        }

        // -----------------------------------------------------------------
        // #185: an empty replacement is refused by the sink — it neither
        // writes nor deletes. This test asserted the opposite until #185:
        // "replacing with an empty resource must delete the old chunks" was
        // the documented behavior, and it is exactly how a file that
        // transiently extracts to nothing erased its own indexed content.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn index_resource_empty_blocks_keeps_old_content() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);

            let old_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                "file:///docs/e.md",
                "Body.",
            )
            .await;

            let empty_resource =
                make_resource_with_blocks("file:///docs/e.md", &source.id, store_id, vec![]);

            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            let outcome = index_resource(
                &empty_resource,
                &source,
                Some(&old_record.resource_id),
                &deps,
            )
            .await
            .unwrap();

            assert_eq!(outcome, IndexOutcome::Empty);
            let old_chunks = store
                .get_chunks_for_resource(&old_record.resource_id)
                .await
                .unwrap();
            assert!(
                !old_chunks.is_empty(),
                "an empty replacement must not delete the old chunks: the sink \
                 cannot tell 'this file is legitimately empty now' apart from \
                 'extraction produced nothing this run', and only one of those \
                 is evidence the content is gone (#185)"
            );
        }

        /// #103: `index_resource` copies each block's `location.page` onto the
        /// chunk records it writes, keyed by block seq.
        #[tokio::test]
        async fn index_resource_copies_block_page_onto_chunks() {
            use crate::block::{Block, BlockKind, BlockLocation};

            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);

            let page_block = |seq: u32, text: &str, page: u32| Block {
                seq,
                kind: BlockKind::Text,
                text: text.to_string(),
                location: Some(BlockLocation {
                    page: Some(page),
                    ..Default::default()
                }),
            };

            let blocks = vec![
                page_block(0, "Alpha content lives on the first page here.", 1),
                page_block(1, "Bravo content lives on the second page here.", 2),
                // A block with no location at all: its chunks must get page None.
                Block {
                    seq: 2,
                    kind: BlockKind::Text,
                    text: "Charlie content has no page info recorded.".to_string(),
                    location: None,
                },
            ];

            let resource =
                make_resource_with_blocks("file:///docs/paged.pdf", &source.id, store_id, blocks);
            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            let written = index_resource(&resource, &source, None, &deps)
                .await
                .unwrap();
            assert!(
                matches!(written, IndexOutcome::Written(n, _) if n >= 3),
                "expected at least one chunk per block, got {written:?}"
            );

            let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();

            // Each chunk's page is that of its originating block seq.
            let page_for_seq = |seq: u32| -> Vec<Option<u32>> {
                chunks
                    .iter()
                    .filter(|c| c.block_seq == seq)
                    .map(|c| c.page)
                    .collect()
            };
            assert!(
                page_for_seq(0).iter().all(|p| *p == Some(1)),
                "block 0 → page 1"
            );
            assert!(
                page_for_seq(1).iter().all(|p| *p == Some(2)),
                "block 1 → page 2"
            );
            assert!(
                page_for_seq(2).iter().all(|p| p.is_none()),
                "block 2 has no location → page None"
            );
        }

        // -----------------------------------------------------------------
        // Codex R2: fetched_at is the resource's `added_at` (ingestion time),
        //           never its `modified_at` (a feed-claimed date).
        // -----------------------------------------------------------------

        /// `Provenance.fetched_at` is defined as *acquisition* time, and the
        /// libsql backend binds it to `resources.added_at` — the column
        /// `MetadataFilter::DateAfter`/`DateBefore` (`DateAxis::Added`) filter
        /// on and that every citation reports. `index_resource` used to read
        /// `resource.modified_at`, so a 2020 feed entry ingested today claimed
        /// a 2020 acquisition time and fell outside a "fetched since last
        /// week" filter. Only the feed connector makes the two fields differ
        /// (`file`/`url` set both to the same value), which is why this stayed
        /// latent until the Atom/RSS ingestor landed.
        ///
        /// See specs/02-domain-model.md §4 and its "Timestamps" rule in the
        /// Feed connector section.
        #[tokio::test]
        async fn index_resource_fetched_at_is_added_at_not_modified_at() {
            const INGESTED_AT: &str = "2026-08-05T00:00:00Z";
            const FEED_CLAIMED: &str = "2020-01-01T00:00:00Z";

            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);

            let mut resource = make_resource(
                "https://blog.example.com/2020/old-post",
                "An old post that a feed is only surfacing to us today.",
                &source.id,
                store_id,
            );
            resource.added_at = INGESTED_AT.to_string();
            resource.modified_at = Some(FEED_CLAIMED.to_string());

            let deps = IndexResourceDeps {
                store: &store,
                embedder: &embedder,
                config: &config,
            };
            index_resource(&resource, &source, None, &deps)
                .await
                .unwrap();

            let chunks = store.get_chunks_for_resource(&resource.id).await.unwrap();
            assert!(!chunks.is_empty(), "the resource must produce chunks");
            for c in &chunks {
                assert_eq!(
                    c.fetched_at, INGESTED_AT,
                    "fetched_at must be the resource's added_at (ingestion time)"
                );
                assert_ne!(
                    c.fetched_at, FEED_CLAIMED,
                    "fetched_at must never be the feed-claimed modified_at"
                );
            }
        }

        // -----------------------------------------------------------------
        // 14. lookup_fetch_metadata — the conditional-GET replay seam and
        //     its suppression rule (specs/04-search-pipeline.md §1)
        // -----------------------------------------------------------------

        /// A `PipelineCallback` wired to nothing but what
        /// `lookup_fetch_metadata` itself touches (`doc_index` and
        /// `config.policy_version`) — the store/embedder are never called on
        /// this path, so `FakeStore`/`FakeEmbedder` stand in inertly.
        fn make_pipeline_callback<'a>(
            source: &'a Source,
            doc_index: &'a mut DocumentIndex,
            store: &'a FakeStore,
            embedder: &'a FakeEmbedder,
            config: &'a IngestionConfig,
        ) -> PipelineCallback<'a> {
            PipelineCallback {
                source,
                doc_index,
                store,
                embedder,
                config,
                progress: None,
                result: IngestionResult::default(),
                seen: std::collections::HashSet::new(),
                gone: std::collections::HashSet::new(),
                discovered_total: 0,
                next_index: 0,
                skip_error_count: 0,
            }
        }

        #[tokio::test]
        async fn lookup_fetch_metadata_returns_stored_validators_when_policy_matches() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(DocumentRecord {
                uri: "https://example.com/doc".to_string(),
                resource_id: "res-1".to_string(),
                source_id: source.id.clone(),
                content_hash: "hash-1".to_string(),
                policy_version: config.policy_version.clone(),
                metadata_hash: "mhash-1".to_string(),
                external_etag: Some("\"abc\"".to_string()),
                external_last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
            });

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse("https://example.com/doc").unwrap();
            let meta = callback.lookup_fetch_metadata(&uri).await;

            assert_eq!(meta.etag.as_deref(), Some("\"abc\""));
            assert_eq!(
                meta.last_modified.as_deref(),
                Some("Wed, 21 Oct 2015 07:28:00 GMT")
            );
        }

        /// The suppression rule, and the one behavior in this seam that must
        /// never regress: a mismatched `policy_version` never replays a
        /// stored validator. A 304 returns no bytes, so a
        /// resource that needs re-chunking under a changed policy could
        /// never be re-chunked if it were allowed to answer 304 — silently
        /// freezing the document at the old policy forever.
        #[tokio::test]
        async fn lookup_fetch_metadata_returns_empty_when_policy_version_differs() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let mut config = make_ingestion_config(store_id);
            config.policy_version = "policy-v2".to_string();
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(DocumentRecord {
                uri: "https://example.com/doc".to_string(),
                resource_id: "res-1".to_string(),
                source_id: source.id.clone(),
                content_hash: "hash-1".to_string(),
                // Stored under the OLD policy — the run's config above is v2.
                policy_version: "policy-v1".to_string(),
                metadata_hash: "mhash-1".to_string(),
                external_etag: Some("\"abc\"".to_string()),
                external_last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
            });

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse("https://example.com/doc").unwrap();
            let meta = callback.lookup_fetch_metadata(&uri).await;

            assert_eq!(
                meta.etag, None,
                "a policy_version mismatch must suppress the stored ETag — replaying \
                 it would let a 304 permanently freeze this resource at the old policy"
            );
            assert_eq!(meta.last_modified, None);
        }

        #[tokio::test]
        async fn lookup_fetch_metadata_returns_empty_when_no_prior_record() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let mut doc_index = DocumentIndex::new();

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse("https://example.com/never-indexed").unwrap();
            let meta = callback.lookup_fetch_metadata(&uri).await;

            assert_eq!(meta.etag, None);
            assert_eq!(meta.last_modified, None);
        }

        // -----------------------------------------------------------------
        // 15. on_validators_refreshed — persisting a 304-refreshed validator
        // -----------------------------------------------------------------

        /// Indexes `resource` (via `index_resource`, so it lands real chunks
        /// in `store`) with `external_etag` overridden to `etag`, and seeds
        /// `doc_index` with the matching `DocumentRecord` — mirroring what
        /// `on_resource`'s own `Written` arm would have stamped, without
        /// going through the callback (these tests exercise
        /// `on_validators_refreshed` directly).
        async fn seed_indexed_with_etag(
            store: &FakeStore,
            embedder: &FakeEmbedder,
            config: &IngestionConfig,
            source: &Source,
            uri: &str,
            text: &str,
            etag: &str,
        ) -> DocumentRecord {
            let mut resource = make_resource(uri, text, &source.id, &config.store_id);
            resource.external_etag = Some(etag.to_string());
            let deps = IndexResourceDeps {
                store,
                embedder,
                config,
            };
            let outcome = index_resource(&resource, source, None, &deps)
                .await
                .expect("seed index must succeed");
            let metadata_hash = match outcome {
                IndexOutcome::Written(_, hash) => hash,
                IndexOutcome::Empty => panic!("seed_indexed_with_etag: must not chunk to empty"),
            };
            DocumentRecord {
                uri: resource.uri.as_str().to_string(),
                resource_id: resource.id.clone(),
                source_id: source.id.clone(),
                content_hash: resource.content_hash.clone(),
                policy_version: config.policy_version.clone(),
                metadata_hash,
                external_etag: resource.external_etag.clone(),
                external_last_modified: resource.external_last_modified.clone(),
            }
        }

        #[tokio::test]
        async fn on_validators_refreshed_with_rotated_etag_updates_stored_row() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let uri_str = "https://example.com/rotating";
            let text = "Stable content that never changes.";

            let mut doc_index = DocumentIndex::new();
            let seeded =
                seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1")
                    .await;
            let resource_id = seeded.resource_id.clone();
            doc_index.upsert(seeded);

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse(uri_str).unwrap();
            callback
                .on_validators_refreshed(
                    &uri,
                    &FetchMetadata {
                        etag: Some("v2".to_string()),
                        last_modified: None,
                    },
                )
                .await;

            let chunks = store.get_chunks_for_resource(&resource_id).await.unwrap();
            assert!(!chunks.is_empty());
            assert!(
                chunks
                    .iter()
                    .all(|c| c.external_etag.as_deref() == Some("v2")),
                "the stored row must carry the rotated ETag the 304 itself reported"
            );

            let cached = callback.doc_index.get(uri_str).unwrap();
            assert_eq!(cached.external_etag.as_deref(), Some("v2"));
        }

        #[tokio::test]
        async fn on_validators_refreshed_bare_304_leaves_stored_row_untouched() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let uri_str = "https://example.com/unchanged";
            let text = "Stable content that never changes.";

            let mut doc_index = DocumentIndex::new();
            let seeded =
                seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1")
                    .await;
            let resource_id = seeded.resource_id.clone();
            doc_index.upsert(seeded);

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse(uri_str).unwrap();
            // A bare 304 — both fields None — must be read as "keep what's
            // stored," never "clear it." `process_url` only calls
            // `on_validators_refreshed` when at least one field is `Some`,
            // but this pins the callback's own half of that contract too:
            // it must be a no-op even if called directly with an empty
            // `FetchMetadata`.
            callback
                .on_validators_refreshed(&uri, &FetchMetadata::default())
                .await;

            let chunks = store.get_chunks_for_resource(&resource_id).await.unwrap();
            assert!(
                chunks
                    .iter()
                    .all(|c| c.external_etag.as_deref() == Some("v1")),
                "a bare 304 must leave the previously stored ETag untouched"
            );
            let cached = callback.doc_index.get(uri_str).unwrap();
            assert_eq!(cached.external_etag.as_deref(), Some("v1"));
        }

        /// A 304 may rotate one validator and say nothing about the other.
        /// RFC 9111 makes silence mean "unchanged", so the field the response
        /// omitted must survive — dropping it would disable half of
        /// conditional GET for that resource on every subsequent run.
        ///
        /// This asserts against the `ResourceRecord` actually handed to the
        /// store rather than against read-back chunk state, because
        /// `external_last_modified` is deliberately not a denormalized
        /// `ChunkRecord` field and so has no per-chunk copy to read back.
        #[tokio::test]
        async fn on_validators_refreshed_preserves_the_validator_a_304_omitted() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let uri_str = "https://example.com/partial";
            let text = "Content whose validators rotate one at a time.";

            let mut doc_index = DocumentIndex::new();
            let mut seeded =
                seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1")
                    .await;
            seeded.external_last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());
            doc_index.upsert(seeded);

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse(uri_str).unwrap();

            // A 304 rotating only the ETag must leave Last-Modified alone.
            callback
                .on_validators_refreshed(
                    &uri,
                    &FetchMetadata {
                        etag: Some("v2".to_string()),
                        last_modified: None,
                    },
                )
                .await;

            let updates = store.metadata_updates().await;
            let (_, record) = updates.last().expect("the refresh must reach the store");
            assert_eq!(record.external_etag.as_deref(), Some("v2"));
            assert_eq!(
                record.external_last_modified.as_deref(),
                Some("Wed, 21 Oct 2015 07:28:00 GMT"),
                "a 304 that omitted Last-Modified must not clear the stored one"
            );

            // And the mirror image: rotating only Last-Modified must leave
            // the ETag alone.
            callback
                .on_validators_refreshed(
                    &uri,
                    &FetchMetadata {
                        etag: None,
                        last_modified: Some("Thu, 22 Oct 2015 07:28:00 GMT".to_string()),
                    },
                )
                .await;

            let updates = store.metadata_updates().await;
            let (_, record) = updates.last().expect("the refresh must reach the store");
            assert_eq!(
                record.external_etag.as_deref(),
                Some("v2"),
                "a 304 that omitted ETag must not clear the stored one"
            );
            assert_eq!(
                record.external_last_modified.as_deref(),
                Some("Thu, 22 Oct 2015 07:28:00 GMT")
            );
        }

        /// A well-behaved origin repeats the validator it already issued on
        /// every 304 for unchanged content, so this is the common case, not
        /// an edge one. Writing anyway would rewrite the resource row and
        /// bump `index_updated_at` — publicly visible as
        /// `DocumentInfo.index_updated_at` — on a run that changed nothing.
        ///
        /// Asserted on the store's call log rather than on final state: a
        /// blind rewrite of identical validators leaves the row looking
        /// exactly the same, so a state assertion would pass with the guard
        /// removed.
        #[tokio::test]
        async fn on_validators_refreshed_repeating_the_stored_validators_writes_nothing() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let uri_str = "https://example.com/repeating";
            let text = "Stable content that never changes.";

            let mut doc_index = DocumentIndex::new();
            let mut seeded =
                seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1")
                    .await;
            seeded.external_last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());
            doc_index.upsert(seeded);

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse(uri_str).unwrap();
            let outcome = callback
                .on_validators_refreshed(
                    &uri,
                    &FetchMetadata {
                        etag: Some("v1".to_string()),
                        last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
                    },
                )
                .await;

            assert_eq!(outcome, MetadataWriteOutcome::Unchanged);
            assert!(
                store.metadata_updates().await.is_empty(),
                "a 304 repeating the validators already stored must not reach the store"
            );
        }

        /// The half of the guard a `compute_metadata_hash` comparison would
        /// have broken: `external_last_modified` is deliberately not one of
        /// that hash's inputs, so a 304 rotating only `Last-Modified` yields
        /// an identical hash while still needing to be persisted. The guard
        /// compares the validator pair itself for exactly this reason.
        #[tokio::test]
        async fn on_validators_refreshed_rotating_only_last_modified_still_writes() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let uri_str = "https://example.com/lm-only";
            let text = "Stable content that never changes.";

            let mut doc_index = DocumentIndex::new();
            let mut seeded =
                seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1")
                    .await;
            seeded.external_last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string());
            doc_index.upsert(seeded);

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse(uri_str).unwrap();
            let outcome = callback
                .on_validators_refreshed(
                    &uri,
                    &FetchMetadata {
                        etag: Some("v1".to_string()),
                        last_modified: Some("Thu, 22 Oct 2015 07:28:00 GMT".to_string()),
                    },
                )
                .await;

            assert_eq!(outcome, MetadataWriteOutcome::Written);
            let updates = store.metadata_updates().await;
            let (_, record) = updates.last().expect("the refresh must reach the store");
            assert_eq!(
                record.external_last_modified.as_deref(),
                Some("Thu, 22 Oct 2015 07:28:00 GMT")
            );
        }

        /// `SkipReason::MetadataUpdated` is not a skip. It counts where
        /// `on_resource`'s own metadata-only branch counts, so a metadata
        /// write reads identically whether it arrived with a body or behind
        /// a 304 — and never lands in `docs_skipped` as well, which would
        /// break the partition of `docs_seen`.
        #[tokio::test]
        async fn on_skipped_metadata_updated_counts_as_an_update_not_a_skip() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let mut doc_index = DocumentIndex::new();
            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);

            let uri = Uri::parse("https://example.com/refreshed").unwrap();
            callback.on_skipped(&uri, SkipReason::MetadataUpdated).await;

            assert_eq!(callback.result.docs_metadata_updated, 1);
            assert_eq!(callback.result.docs_skipped, 0);
            assert_eq!(callback.result.error_count, 0);
            assert_eq!(callback.result.docs_seen, 1);
            assert!(
                callback.seen.contains("https://example.com/refreshed"),
                "a metadata-updated URI is still alive and must survive the delete-sweep"
            );
        }

        /// The metadata_hash trap, pinned: `external_etag` IS an input to
        /// `compute_metadata_hash`, so rotating it via a 304 refresh without
        /// also refreshing the *cached* `metadata_hash` in `doc_index` would
        /// desync the two. The next metadata-unchanged fetch (a normal 200
        /// whose own reported ETag now matches what the 304 already
        /// rotated to) would then see a spurious mismatch and route through
        /// a needless metadata-only update — churn this test would catch as
        /// a wrongly nonzero `docs_metadata_updated`.
        #[tokio::test]
        async fn on_validators_refreshed_keeps_metadata_hash_in_sync_no_churn_next_run() {
            let store_id = "store-1";
            let source = make_source_with_preset(store_id, "prose");
            let config = make_ingestion_config(store_id);
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let uri_str = "https://example.com/no-churn";
            let text = "Stable content that never changes.";

            let mut doc_index = DocumentIndex::new();
            let seeded =
                seed_indexed_with_etag(&store, &embedder, &config, &source, uri_str, text, "v1")
                    .await;
            doc_index.upsert(seeded);

            let mut callback =
                make_pipeline_callback(&source, &mut doc_index, &store, &embedder, &config);
            let uri = Uri::parse(uri_str).unwrap();
            callback
                .on_validators_refreshed(
                    &uri,
                    &FetchMetadata {
                        etag: Some("v2".to_string()),
                        last_modified: None,
                    },
                )
                .await;

            // A subsequent run's ordinary 200 fetch: identical content, and
            // the origin now consistently reports the SAME "v2" ETag the
            // 304 already rotated to.
            let mut resource_next_run = make_resource(uri_str, text, &source.id, store_id);
            resource_next_run.external_etag = Some("v2".to_string());
            callback.on_resource(resource_next_run).await.unwrap();

            assert_eq!(
                callback.result.docs_skipped, 1,
                "content and metadata are both unchanged relative to the refreshed \
                 state — this must be an ordinary skip"
            );
            assert_eq!(
                callback.result.docs_metadata_updated, 0,
                "a correctly-synced metadata_hash must not churn a metadata-only \
                 update on the very next unchanged fetch"
            );
        }

        // -----------------------------------------------------------------
        // Feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out
        // feed entries: the liveness sweep")
        // -----------------------------------------------------------------
        mod feed_liveness_sweep {
            use super::*;
            use crate::store::{MetadataFilter, ResourceRecord, SearchResult, StoreStats};

            #[derive(Debug, Clone)]
            struct LivenessRow {
                resource_id: String,
                uri: String,
                external_id: Option<String>,
                external_etag: Option<String>,
                external_last_modified: Option<String>,
                last_checked_at: Option<String>,
            }

            /// A discovered feed entry: stamped with the entry's own id, as
            /// `FeedIngestor` stamps every entry it yields.
            fn row(resource_id: &str, uri: &str, last_checked_at: Option<&str>) -> LivenessRow {
                LivenessRow {
                    resource_id: resource_id.to_string(),
                    uri: uri.to_string(),
                    external_id: Some(format!("urn:entry:{resource_id}")),
                    external_etag: None,
                    external_last_modified: None,
                    last_checked_at: last_checked_at.map(str::to_string),
                }
            }

            /// The feed's own document, as single-document mode
            /// (`fetch_full_content: false`) stores it: a `feed` resource
            /// under the feed URL, and the only one carrying no
            /// `external_id`.
            fn feed_root_row(
                resource_id: &str,
                uri: &str,
                last_checked_at: Option<&str>,
            ) -> LivenessRow {
                LivenessRow {
                    external_id: None,
                    ..row(resource_id, uri, last_checked_at)
                }
            }

            /// A minimal `RetrievalStore` double for the liveness sweep: an
            /// in-memory candidate table plus call recorders, so tests can
            /// assert both the sweep's *decisions* (delete vs. touch vs.
            /// leave alone) and its *restraint* (never queries the store at
            /// all when a guard suppresses it, never fetches a candidate the
            /// recheck floor or `seen` rules out).
            /// `(resource_id, etag, last_modified)` recorded per
            /// `touch_resource_liveness` call.
            type TouchCall = (String, Option<String>, Option<String>);

            struct LivenessStore {
                rows: tokio::sync::Mutex<Vec<LivenessRow>>,
                delete_calls: tokio::sync::Mutex<Vec<String>>,
                touch_calls: tokio::sync::Mutex<Vec<TouchCall>>,
                list_calls: std::sync::atomic::AtomicUsize,
            }

            impl LivenessStore {
                fn new(rows: Vec<LivenessRow>) -> Self {
                    Self {
                        rows: tokio::sync::Mutex::new(rows),
                        delete_calls: tokio::sync::Mutex::new(Vec::new()),
                        touch_calls: tokio::sync::Mutex::new(Vec::new()),
                        list_calls: std::sync::atomic::AtomicUsize::new(0),
                    }
                }

                fn list_call_count(&self) -> usize {
                    self.list_calls.load(std::sync::atomic::Ordering::SeqCst)
                }
            }

            #[async_trait::async_trait]
            impl RetrievalStore for LivenessStore {
                async fn upsert_chunks(&self, _records: Vec<ChunkRecord>) -> Result<usize, Error> {
                    Ok(0)
                }

                async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
                    self.delete_calls.lock().await.push(resource_id.to_string());
                    let mut rows = self.rows.lock().await;
                    let before = rows.len();
                    rows.retain(|r| r.resource_id != resource_id);
                    Ok(before - rows.len())
                }

                async fn delete_by_store(&self, _store_id: &str) -> Result<usize, Error> {
                    Ok(0)
                }

                async fn dense_search(
                    &self,
                    _query_vector: &[f32],
                    _limit: usize,
                    _filters: &[MetadataFilter],
                ) -> Result<Vec<SearchResult>, Error> {
                    Ok(Vec::new())
                }

                async fn bm25_search(
                    &self,
                    _query_text: &str,
                    _limit: usize,
                    _filters: &[MetadataFilter],
                ) -> Result<Vec<SearchResult>, Error> {
                    Ok(Vec::new())
                }

                async fn stats(&self) -> Result<StoreStats, Error> {
                    Ok(StoreStats {
                        chunk_count: 0,
                        document_count: 0,
                    })
                }

                async fn get_chunk(&self, _chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
                    Ok(None)
                }

                async fn get_chunks_for_resource(
                    &self,
                    _resource_id: &str,
                ) -> Result<Vec<ChunkRecord>, Error> {
                    Ok(Vec::new())
                }

                async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
                    Ok(Vec::new())
                }

                async fn update_resource_metadata(
                    &self,
                    _store_id: &str,
                    _resource_id: &str,
                    _record: &ResourceRecord,
                ) -> Result<(), Error> {
                    unimplemented!("not exercised by the liveness sweep")
                }

                async fn get_resource_record(
                    &self,
                    _store_id: &str,
                    _resource_id: &str,
                ) -> Result<Option<ResourceRecord>, Error> {
                    unimplemented!("not exercised by the liveness sweep")
                }

                async fn list_stale_feed_resources(
                    &self,
                    _store_id: &str,
                    _source_id: &str,
                    checked_before: &str,
                    limit: usize,
                ) -> Result<Vec<StaleFeedResource>, Error> {
                    self.list_calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let rows = self.rows.lock().await;
                    let mut candidates: Vec<&LivenessRow> = rows
                        .iter()
                        // A URI carrying a fragment (a link-less entry's
                        // synthetic `{feed_url}#entry:{id}`) is never a
                        // candidate — mirrors `store-libsql`'s
                        // `instr(uri, '#') = 0` and the `uri LIKE
                        // 'http(s)://%'` scheme filter in the real query.
                        .filter(|r| !r.uri.contains('#'))
                        .filter(|r| r.uri.starts_with("http://") || r.uri.starts_with("https://"))
                        // Only discovered entries. The feed's own document
                        // carries no `external_id` — mirrors the real
                        // query's `external_id IS NOT NULL`.
                        .filter(|r| r.external_id.is_some())
                        .filter(|r| {
                            r.last_checked_at
                                .as_deref()
                                .is_none_or(|checked| checked < checked_before)
                        })
                        .collect();
                    // `None` sorts before `Some`, matching SQLite's plain
                    // `ORDER BY last_checked_at ASC` (NULL first) — see
                    // `store-libsql`'s `list_stale_feed_resources` for the
                    // real query this mirrors.
                    candidates.sort_by(|a, b| a.last_checked_at.cmp(&b.last_checked_at));
                    Ok(candidates
                        .into_iter()
                        .take(limit)
                        .map(|r| StaleFeedResource {
                            resource_id: r.resource_id.clone(),
                            uri: r.uri.clone(),
                            external_etag: r.external_etag.clone(),
                            external_last_modified: r.external_last_modified.clone(),
                        })
                        .collect())
                }

                async fn touch_resource_liveness(
                    &self,
                    _store_id: &str,
                    resource_id: &str,
                    etag: Option<&str>,
                    last_modified: Option<&str>,
                ) -> Result<(), Error> {
                    self.touch_calls.lock().await.push((
                        resource_id.to_string(),
                        etag.map(str::to_string),
                        last_modified.map(str::to_string),
                    ));
                    let mut rows = self.rows.lock().await;
                    if let Some(r) = rows.iter_mut().find(|r| r.resource_id == resource_id) {
                        r.external_etag = etag.map(str::to_string);
                        r.external_last_modified = last_modified.map(str::to_string);
                        r.last_checked_at =
                            Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
                    }
                    Ok(())
                }
            }

            #[derive(Clone, Copy)]
            enum ScriptedFetchOutcome {
                Gone,
                NotModified,
                Downloaded,
                Blocked,
                TransportError,
            }

            /// Records every URL fetched, in call order — the recheck-floor
            /// and batch-cap tests below assert directly on `calls`.
            struct ScriptedFetcher {
                default_outcome: ScriptedFetchOutcome,
                calls: tokio::sync::Mutex<Vec<String>>,
            }

            impl ScriptedFetcher {
                fn new(default_outcome: ScriptedFetchOutcome) -> Self {
                    Self {
                        default_outcome,
                        calls: tokio::sync::Mutex::new(Vec::new()),
                    }
                }
            }

            #[async_trait::async_trait]
            impl UrlFetcher for ScriptedFetcher {
                async fn fetch(
                    &self,
                    url: &str,
                    _metadata: &FetchMetadata,
                ) -> Result<FetchResult, Error> {
                    self.calls.lock().await.push(url.to_string());
                    match self.default_outcome {
                        ScriptedFetchOutcome::Gone => Ok(FetchResult::Gone),
                        ScriptedFetchOutcome::NotModified => Ok(FetchResult::NotModified {
                            etag: None,
                            last_modified: None,
                        }),
                        ScriptedFetchOutcome::Downloaded => Ok(FetchResult::Downloaded {
                            bytes: Vec::new(),
                            content_type: None,
                            etag: Some("\"fresh\"".to_string()),
                            last_modified: None,
                            final_url: None,
                        }),
                        ScriptedFetchOutcome::Blocked => Ok(FetchResult::Blocked),
                        ScriptedFetchOutcome::TransportError => Err(Error::Internal {
                            message: "simulated transport error".to_string(),
                            correlation_id: "liveness_sweep_test_fetch_error".to_string(),
                        }),
                    }
                }
            }

            fn old_timestamp() -> String {
                "2020-01-01T00:00:00Z".to_string()
            }

            // -------------------------------------------------------------
            // Per-candidate outcomes
            // -------------------------------------------------------------

            #[tokio::test]
            async fn gone_candidate_is_deleted_and_counted() {
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert_eq!(result.docs_deleted, 1);
                assert_eq!(result.feed_entries_liveness_checked, 1);
                assert_eq!(*store.delete_calls.lock().await, vec!["r1".to_string()]);
                assert!(store.touch_calls.lock().await.is_empty());
            }

            #[tokio::test]
            async fn not_modified_candidate_is_touched_not_deleted() {
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::NotModified);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert_eq!(result.docs_deleted, 0);
                assert_eq!(result.feed_entries_liveness_checked, 1);
                assert!(store.delete_calls.lock().await.is_empty());
                assert_eq!(store.touch_calls.lock().await.len(), 1);
            }

            /// A 200 is touched (validators + `last_checked_at` refreshed),
            /// never deleted, and — the point of this test — never
            /// re-indexed: nothing in this test's `LivenessStore` exposes an
            /// `upsert_chunks`/`upsert_chunks_and_blocks` write path that
            /// records a call, so a passing assertion on `touch_calls` alone
            /// (no other store method touched) already proves no re-index
            /// happened.
            #[tokio::test]
            async fn downloaded_candidate_is_touched_not_deleted_and_not_reindexed() {
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Downloaded);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert_eq!(result.docs_deleted, 0);
                assert!(store.delete_calls.lock().await.is_empty());
                let touches = store.touch_calls.lock().await;
                assert_eq!(touches.len(), 1);
                assert_eq!(touches[0].0, "r1");
                assert_eq!(touches[0].1.as_deref(), Some("\"fresh\""));
            }

            #[tokio::test]
            async fn blocked_candidate_is_left_completely_untouched() {
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Blocked);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert_eq!(result.docs_deleted, 0);
                assert_eq!(
                    result.feed_entries_liveness_checked, 1,
                    "still counted as probed even though nothing moved"
                );
                assert!(store.delete_calls.lock().await.is_empty());
                assert!(store.touch_calls.lock().await.is_empty());
            }

            #[tokio::test]
            async fn transport_error_leaves_the_resource_untouched() {
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::TransportError);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert_eq!(result.docs_deleted, 0);
                assert_eq!(result.feed_entries_liveness_checked, 1);
                assert!(store.delete_calls.lock().await.is_empty());
                assert!(store.touch_calls.lock().await.is_empty());
            }

            // -------------------------------------------------------------
            // Throttle: recheck floor and `seen`
            // -------------------------------------------------------------

            #[tokio::test]
            async fn candidate_newer_than_the_recheck_floor_is_never_fetched() {
                // Checked a minute ago — well inside the bare 24h floor
                // (`refresh_interval_secs: None`).
                let recent = (Utc::now() - chrono::Duration::seconds(60))
                    .to_rfc3339_opts(SecondsFormat::Secs, true);
                let store =
                    LivenessStore::new(vec![row("r1", "https://a.example.com/", Some(&recent))]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert!(
                    fetcher.calls.lock().await.is_empty(),
                    "a resource checked well inside the recheck floor must never be fetched"
                );
                assert_eq!(result.feed_entries_liveness_checked, 0);
            }

            /// A configured `refresh_interval_secs` above the bare 24h floor
            /// raises the effective floor — a resource checked 25h ago (past
            /// the bare floor, but not past a configured 30-day one) must
            /// still not be fetched.
            #[tokio::test]
            async fn configured_refresh_interval_raises_the_recheck_floor_above_24h() {
                let twenty_five_hours_ago = (Utc::now() - chrono::Duration::hours(25))
                    .to_rfc3339_opts(SecondsFormat::Secs, true);
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&twenty_five_hours_ago),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    Some(30 * 24 * 60 * 60), // 30 days
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert!(
                    fetcher.calls.lock().await.is_empty(),
                    "a 30-day configured refresh interval must raise the floor above the bare 24h default"
                );
            }

            /// A `refresh_interval_secs` above `i64::MAX` must not overflow
            /// the `as i64` cast the recheck-floor computation used to use: a
            /// wrapped-negative value would push `checked_before` into the
            /// future, making every resource a candidate — the opposite of
            /// the throttle's purpose. A resource checked one minute ago must
            /// stay well inside any correctly computed floor regardless of
            /// how large the configured interval is.
            #[tokio::test]
            async fn recheck_floor_with_u64_max_refresh_interval_never_lands_in_the_future() {
                let one_minute_ago = (Utc::now() - chrono::Duration::seconds(60))
                    .to_rfc3339_opts(SecondsFormat::Secs, true);
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&one_minute_ago),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    Some(u64::MAX),
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert!(
                    fetcher.calls.lock().await.is_empty(),
                    "an overflowing refresh_interval_secs must never push checked_before into \
                     the future — that would make every resource a candidate"
                );
            }

            /// `refresh_interval_secs: Some(0)` must not drop the recheck
            /// floor below the bare 24h minimum — the `.max(...)` call
            /// guards this, but only if the value it is maxed against
            /// actually reaches `checked_before` afterward.
            #[tokio::test]
            async fn recheck_floor_with_zero_configured_refresh_interval_never_drops_below_24h() {
                let twenty_three_hours_ago = (Utc::now() - chrono::Duration::hours(23))
                    .to_rfc3339_opts(SecondsFormat::Secs, true);
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&twenty_three_hours_ago),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    Some(0),
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert!(
                    fetcher.calls.lock().await.is_empty(),
                    "a configured refresh_interval_secs of 0 must not drop the recheck floor \
                     below the bare 24h minimum"
                );
            }

            #[tokio::test]
            async fn a_candidate_still_in_this_runs_seen_set_is_never_fetched() {
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let mut seen = std::collections::HashSet::new();
                seen.insert("https://a.example.com/".to_string());
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert!(
                    fetcher.calls.lock().await.is_empty(),
                    "a candidate this run's own ingestion pass already observed must not be probed"
                );
                assert_eq!(result.feed_entries_liveness_checked, 0);
            }

            // -------------------------------------------------------------
            // Batch cap
            // -------------------------------------------------------------

            #[tokio::test]
            async fn over_cap_batch_processes_only_the_cap_oldest_first() {
                let mut rows = Vec::new();
                let mut expected_order = Vec::new();
                for i in 0..(FEED_LIVENESS_BATCH_LIMIT + 5) {
                    let resource_id = format!("r{i:03}");
                    let uri = format!("https://{i:03}.example.com/");
                    // Strictly increasing timestamps -> strictly oldest-first
                    // order is unambiguous.
                    let checked_at = format!("2020-01-{:02}T00:00:00Z", (i % 28) + 1);
                    rows.push(row(&resource_id, &uri, Some(&checked_at)));
                    expected_order.push((checked_at, uri));
                }
                expected_order.sort();
                let expected_uris: Vec<String> = expected_order
                    .into_iter()
                    .take(FEED_LIVENESS_BATCH_LIMIT)
                    .map(|(_, uri)| uri)
                    .collect();

                let store = LivenessStore::new(rows);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::NotModified);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                let calls = fetcher.calls.lock().await.clone();
                assert_eq!(calls.len(), FEED_LIVENESS_BATCH_LIMIT);
                assert_eq!(
                    calls, expected_uris,
                    "the batch cap must keep exactly the oldest N candidates, in oldest-first order"
                );
                assert_eq!(
                    result.feed_entries_liveness_checked as usize,
                    FEED_LIVENESS_BATCH_LIMIT
                );
            }

            // -------------------------------------------------------------
            // Fragment URIs (link-less entries)
            // -------------------------------------------------------------

            /// A link-less entry's synthetic `{feed_url}#entry:{id}` URI must
            /// never be probed, even when it is the oldest (never-checked)
            /// candidate and the feed root would answer 404: HTTP never sends
            /// a fragment on the wire, so probing it verbatim would actually
            /// request the feed root, and a positive `Gone` there must not
            /// delete the entry's resource.
            #[tokio::test]
            async fn fragment_uri_candidate_is_never_fetched_or_deleted() {
                let store = LivenessStore::new(vec![row(
                    "r-fragment",
                    "https://feed.example.com/feed.xml#entry:entry-1",
                    None, // never-checked — would otherwise sort first
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert!(
                    fetcher.calls.lock().await.is_empty(),
                    "a fragment URI must never be fetched — a fragment is never sent on \
                     the wire, so the request would actually hit the feed root"
                );
                assert_eq!(result.feed_entries_liveness_checked, 0);
                assert_eq!(
                    result.docs_deleted, 0,
                    "the entry's resource must not be deleted on a signal that has \
                     nothing to do with it"
                );
                assert_eq!(
                    store.rows.lock().await.len(),
                    1,
                    "the resource must still exist in the store after the sweep"
                );
            }

            /// The feed's own document, in single-document mode, is a
            /// `feed` resource under the feed URL — so it matches every
            /// candidate predicate except the one that exists for it. A
            /// 404/410 on the feed URL would otherwise delete the source's
            /// entire index through a mechanism written to prune a single
            /// entry.
            #[tokio::test]
            async fn feed_root_candidate_is_never_fetched_or_deleted() {
                let store = LivenessStore::new(vec![feed_root_row(
                    "r-feed-root",
                    "https://feed.example.com/feed.xml",
                    None, // never-checked — would otherwise sort first
                )]);
                let fetcher = ScriptedFetcher::new(ScriptedFetchOutcome::Gone);
                let mut doc_index = DocumentIndex::new();
                let seen = std::collections::HashSet::new();
                let mut result = IngestionResult::default();

                run_feed_liveness_sweep(
                    "src-1",
                    "store-1",
                    None,
                    &seen,
                    &mut doc_index,
                    &store,
                    &fetcher,
                    &mut result,
                )
                .await
                .unwrap();

                assert!(
                    fetcher.calls.lock().await.is_empty(),
                    "the feed's own document must never be probed by the entry sweep"
                );
                assert_eq!(result.feed_entries_liveness_checked, 0);
                assert_eq!(
                    result.docs_deleted, 0,
                    "a 404 on the feed URL must not delete a single-document index"
                );
                assert_eq!(store.rows.lock().await.len(), 1);
            }

            // -------------------------------------------------------------
            // Guards (run through the full `run_source_ingestion`, since
            // both guards live there, not inside `run_feed_liveness_sweep`
            // itself)
            // -------------------------------------------------------------

            fn make_feed_source(store_id: &str) -> Source {
                Source {
                    id: new_ulid(),
                    store_id: store_id.to_string(),
                    kind: SourceKind::Feed,
                    spec: SourceSpec::Feed {
                        url: "https://feed.example.com/feed.xml".to_string(),
                        max_entries: None,
                        fetch_full_content: true,
                        refresh_interval_secs: None,
                    },
                    source_preset: "prose".to_string(),
                }
            }

            fn seed_doc_index_owned_by(doc_index: &mut DocumentIndex, source_id: &str, uri: &str) {
                doc_index.upsert(DocumentRecord {
                    uri: uri.to_string(),
                    resource_id: format!("{uri}-resource"),
                    source_id: source_id.to_string(),
                    content_hash: "hash".to_string(),
                    policy_version: "policy-v1".to_string(),
                    metadata_hash: "mhash".to_string(),
                    external_etag: None,
                    external_last_modified: None,
                });
            }

            /// The most important test in this module alongside the next
            /// one: an ingestor that could not observe its source must
            /// suppress the liveness sweep before it ever queries the store
            /// — an `UnreachableFetcher` alone would not distinguish "the
            /// sweep ran and found nothing" from "the sweep never ran",
            /// which is exactly what `LivenessStore::list_call_count`
            /// exists to tell apart.
            #[tokio::test]
            async fn incomplete_enumeration_guard_suppresses_the_sweep() {
                let source = make_feed_source("store-1");
                let config = make_ingestion_config("store-1");
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let embedder = FakeEmbedder::new(4);
                let mut doc_index = DocumentIndex::new();
                seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

                let ingestor = FakeIngestor::incomplete("feed unreachable");
                let deps = SourceIngestionDeps {
                    doc_index: &mut doc_index,
                    store: &store,
                    embedder: &embedder,
                    config: &config,
                    progress: None,
                    deletion: DeletionPolicy::Prune,
                    document_validators: FetchMetadata::default(),
                    stored_inputs_digest: None,
                    fetcher: &UnreachableFetcher,
                };
                run_source_ingestion(&source, &ingestor, deps)
                    .await
                    .unwrap();

                assert_eq!(
                    store.list_call_count(),
                    0,
                    "Enumeration::Incomplete must suppress the liveness sweep before it \
                     ever queries the store"
                );
            }

            /// The steady-state feed-304 case: zero entry callbacks fire, so
            /// `seen` is empty — the same condition that suppresses the
            /// presumed-gone sweep for path/url sources also suppresses the
            /// liveness sweep here, and for the same reason
            /// (specs/02-domain-model.md, "Conditional GET and pruning").
            #[tokio::test]
            async fn zero_seen_guard_suppresses_the_sweep_on_a_feed_304() {
                let source = make_feed_source("store-1");
                let config = make_ingestion_config("store-1");
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let embedder = FakeEmbedder::new(4);
                let mut doc_index = DocumentIndex::new();
                seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

                // Complete enumeration (the default), but zero callbacks —
                // an empty script, mirroring what `FeedIngestor` does on a
                // bare feed-document 304.
                let ingestor = FakeIngestor::new(vec![]);
                let deps = SourceIngestionDeps {
                    doc_index: &mut doc_index,
                    store: &store,
                    embedder: &embedder,
                    config: &config,
                    progress: None,
                    deletion: DeletionPolicy::Prune,
                    document_validators: FetchMetadata::default(),
                    stored_inputs_digest: None,
                    fetcher: &UnreachableFetcher,
                };
                run_source_ingestion(&source, &ingestor, deps)
                    .await
                    .unwrap();

                assert_eq!(
                    store.list_call_count(),
                    0,
                    "a zero-seen run must suppress the liveness sweep"
                );
            }

            /// The routine case: a feed under `--delete` whose document
            /// answered 304 must not warn on every run — that trains
            /// operators to ignore the level. It still logs, just at
            /// `debug!`, and never at `warn!` for this cause.
            #[tokio::test]
            async fn zero_seen_guard_on_a_feed_logs_at_debug_not_warn() {
                let source = make_feed_source("store-1");
                let config = make_ingestion_config("store-1");
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let embedder = FakeEmbedder::new(4);
                let mut doc_index = DocumentIndex::new();
                seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

                let ingestor = FakeIngestor::new(vec![]);
                let deps = SourceIngestionDeps {
                    doc_index: &mut doc_index,
                    store: &store,
                    embedder: &embedder,
                    config: &config,
                    progress: None,
                    deletion: DeletionPolicy::Prune,
                    document_validators: FetchMetadata::default(),
                    fetcher: &UnreachableFetcher,
                };

                let (buf, guard) = capture_logs();
                run_source_ingestion(&source, &ingestor, deps)
                    .await
                    .unwrap();
                drop(guard);

                let captured = captured_text(&buf);
                assert!(
                    captured.contains("DEBUG") && captured.contains("skipping feed liveness sweep"),
                    "the routine feed-304 zero-seen backstop must log at DEBUG; captured: {captured}"
                );
                assert!(
                    !captured.contains("WARN"),
                    "must not also warn for the same routine suppression; captured: {captured}"
                );
            }

            /// The anomalous case must keep warning even on the feed branch:
            /// an ingestor that could not observe its source at all is never
            /// routine, regardless of source kind.
            #[tokio::test]
            async fn incomplete_enumeration_guard_on_a_feed_still_logs_at_warn() {
                let source = make_feed_source("store-1");
                let config = make_ingestion_config("store-1");
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let embedder = FakeEmbedder::new(4);
                let mut doc_index = DocumentIndex::new();
                seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

                let ingestor = FakeIngestor::incomplete("feed unreachable");
                let deps = SourceIngestionDeps {
                    doc_index: &mut doc_index,
                    store: &store,
                    embedder: &embedder,
                    config: &config,
                    progress: None,
                    deletion: DeletionPolicy::Prune,
                    document_validators: FetchMetadata::default(),
                    fetcher: &UnreachableFetcher,
                };

                let (buf, guard) = capture_logs();
                run_source_ingestion(&source, &ingestor, deps)
                    .await
                    .unwrap();
                drop(guard);

                let captured = captured_text(&buf);
                assert!(
                    captured.contains("WARN") && captured.contains("skipping feed liveness sweep"),
                    "an incomplete enumeration is genuinely anomalous and must still warn; \
                     captured: {captured}"
                );
            }

            #[tokio::test]
            async fn deletion_retain_performs_zero_liveness_fetches() {
                let source = make_feed_source("store-1");
                let config = make_ingestion_config("store-1");
                let store = LivenessStore::new(vec![row(
                    "r1",
                    "https://a.example.com/",
                    Some(&old_timestamp()),
                )]);
                let embedder = FakeEmbedder::new(4);
                let mut doc_index = DocumentIndex::new();
                seed_doc_index_owned_by(&mut doc_index, &source.id, "https://a.example.com/");

                // Both guards pass this time (this run observes the owned
                // URI via a Skipped callback) — proving `Retain` alone, not
                // a guard, is what keeps this at zero fetches.
                let ingestor = FakeIngestor::new(vec![ScriptStep::Skipped(
                    "https://a.example.com/".to_string(),
                    SkipReason::Unchanged,
                )]);
                let deps = SourceIngestionDeps {
                    doc_index: &mut doc_index,
                    store: &store,
                    embedder: &embedder,
                    config: &config,
                    progress: None,
                    deletion: DeletionPolicy::Retain,
                    document_validators: FetchMetadata::default(),
                    stored_inputs_digest: None,
                    fetcher: &UnreachableFetcher,
                };
                run_source_ingestion(&source, &ingestor, deps)
                    .await
                    .unwrap();

                assert_eq!(
                    store.list_call_count(),
                    0,
                    "DeletionPolicy::Retain must never reach the liveness sweep at all — \
                     there is no free preview signal for this mechanism"
                );
            }
        }
    }
}
