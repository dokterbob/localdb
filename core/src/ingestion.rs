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

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::block::Resource;
use crate::chunker::{chunk_blocks, CharSizer, ChunkSizer, ChunkerConfig, TokenSizer};
use crate::embedder::{DocumentChunks, Embedder};
use crate::error::Error;
use crate::ids::new_ulid;
use crate::ingestor::{Enumeration, IngestCallback, IngestSource, Ingestor, SkipReason};
use crate::store::{ChunkRecord, RetrievalStore};
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
#[derive(Debug, Default, Clone)]
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
#[derive(Debug, Clone, Default)]
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
    NotModified,
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

/// Mark an IndexJob as failed with an error message.
pub fn fail_index_job(job: &mut IndexJob, error: String) {
    job.state = IndexJobState::Failed;
    job.error = Some(error);
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
        use std::time::{SystemTime, UNIX_EPOCH};
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format_secs_rfc3339(duration.as_secs())
    }
    #[cfg(test)]
    {
        "2026-06-10T12:00:00Z".to_string()
    }
}

/// Format a Unix timestamp as RFC 3339 (UTC, no sub-second precision),
/// without requiring chrono.
fn format_secs_rfc3339(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = secs_to_ymd_hms(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

fn secs_to_ymd_hms(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;

    // Gregorian calendar calculation
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y_adj = if mo <= 2 { y + 1 } else { y };

    (y_adj, mo, d, h, m, s)
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
/// Returns [`IndexOutcome::Written`] with the number of chunks written, or
/// [`IndexOutcome::Empty`] if the resource produced no chunks at all.
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
        // FetchedAfter`/`FetchedBefore` filter on and every citation reports.
        // See specs/02-domain-model.md §4.
        fetched_at: resource.added_at.clone(),
        content_hash: resource.content_hash.clone(),
        share_path: vec![],
    };

    // Title propagation: resource.title backfills the metadata's Dublin Core
    // title when the resource's own metadata doesn't already carry one.
    let mut record_metadata = resource.metadata.clone();
    if record_metadata.dublin_core().title.is_none() {
        if let Some(title) = &resource.title {
            record_metadata.dublin_core_mut().title = Some(title.clone());
        }
    }

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
        )
        .await?;

    Ok(IndexOutcome::Written(written))
}

/// What [`index_resource`] did with a resource.
///
/// `Empty` is a distinct outcome rather than `Written(0)` because the caller
/// must treat it differently (#185): a resource that chunked to nothing is
/// *not* an indexed document, and recording it as one — bumping
/// `docs_indexed`, upserting its hash into the `DocumentIndex` — is what
/// turned "this file extracted to nothing" into "this file's indexed content
/// is gone."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOutcome {
    /// The resource was chunked, embedded, and written: this many chunks.
    Written(usize),
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
    } = deps;

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
    // deletion and the sweep must still run for them.
    if !matches!(source.spec, SourceSpec::Feed { .. }) {
        // Two further suppressions, both from issue #156, both stating for
        // path/url sources the rule the feed exemption above states for feeds:
        // the sweep infers deletion from absence, so it may only run when the
        // absence is *informative*.
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

        let suppressed_because = match &ingest_result.enumeration {
            // Guard 1 — enumeration completeness. The ingestor itself reported
            // that it could not observe the source (an unmounted volume, an
            // unreachable root, an API that failed part-way). Its silence
            // about a URI says nothing about whether that URI still exists.
            // This is the guard that fires for the reported incident:
            // `/Volumes/Archive` unmounted, `FileIngestor` enumerated zero
            // files, and the sweep deleted every document the source owned.
            Enumeration::Incomplete { reason } => Some(reason.clone()),
            // Guard 2 — zero-seen backstop, source-shape-agnostic. Even with a
            // *complete* enumeration claimed, a source that previously owned
            // documents and observed none of them this run is far more likely
            // to be a broken connector than a source whose entire contents
            // vanished at once. This does not subsume guard 1: a connector
            // that enumerates 3 of 500 items before failing has a non-empty
            // `seen` set, so only guard 1 protects the other 497.
            //
            // Deliberate trade-off: a source whose files really were all
            // deleted or renamed in one run keeps its stale documents until
            // the source is re-created. The warning below says so.
            Enumeration::Complete if !owned_uris.is_empty() && !any_owned_uri_seen => {
                Some("this run observed none of the documents this source owns".to_string())
            }
            Enumeration::Complete => None,
        };

        if let Some(reason) = suppressed_because {
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
                reason,
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
    }

    if let Some(sink) = &progress {
        sink(crate::progress::ProgressEvent::SourceFinished {
            result: result.clone(),
        });
    }

    Ok(result)
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

        // Skip-check: unchanged content_hash + same policy_version → skip.
        // Ingestors may ALSO skip earlier via `on_skipped`; both paths mark
        // the URI seen so the delete-sweep leaves it alone.
        if let Some(existing) = self.doc_index.get(&uri) {
            if existing.content_hash == resource.content_hash
                && existing.policy_version == self.config.policy_version
            {
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::Skipped,
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
            Ok(IndexOutcome::Written(chunks_written)) => {
                self.result.docs_indexed += 1;
                self.result.chunks_written += chunks_written as u64;
                self.doc_index.upsert(DocumentRecord {
                    uri: uri.clone(),
                    resource_id: resource.id.clone(),
                    source_id: resource.source_id.clone(),
                    content_hash: resource.content_hash.clone(),
                    policy_version: self.config.policy_version.clone(),
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
        };
        idx.upsert(rec);
        let removed = idx.remove("file:///test.md");
        assert!(removed.is_some());
        assert!(idx.is_empty());
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
        assert!(job.completed_at.is_some());
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
                content_hash: hash,
                title: None,
                mime: Some("text/markdown".to_string()),
                metadata: Metadata::Document(DocumentMetadata::default()),
                added_at: "2026-06-10T12:00:00Z".to_string(),
                modified_at: "2026-06-10T12:00:00Z".to_string(),
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
            index_resource(&resource, source, None, &deps)
                .await
                .expect("seed index must succeed");
            DocumentRecord {
                uri: uri.to_string(),
                resource_id: resource.id.clone(),
                source_id: source.id.clone(),
                content_hash: resource.content_hash.clone(),
                policy_version: config.policy_version.clone(),
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
                doc_index.get(&uri_b).is_some(),
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
                doc_index.get(&uri_b).is_some(),
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

            async fn upsert_chunks_and_blocks(
                &self,
                store_id: &str,
                resource_id: &str,
                records: Vec<ChunkRecord>,
                blocks: &[crate::block::Block],
                replaces_resource_id: Option<&str>,
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
                matches!(written, IndexOutcome::Written(n) if n >= 3),
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
        /// `MetadataFilter::FetchedAfter`/`FetchedBefore` filter on and that
        /// every citation reports. `index_resource` used to read
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
            resource.modified_at = FEED_CLAIMED.to_string();

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
    }
}
