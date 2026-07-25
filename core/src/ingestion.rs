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
use crate::ingestor::{IngestCallback, IngestSource, Ingestor, SkipReason};
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

/// Enumerate files in a `path`-kind source, applying include/exclude globs.
///
/// Returns a list of found files sorted by path for determinism.
///
/// # Errors
/// Returns `Error::Internal` if the root path cannot be read.
pub fn enumerate_path_source(
    root: &str,
    include: &[String],
    exclude: &[String],
) -> Result<Vec<FoundFile>, Error> {
    let root_path = Path::new(root);

    if !root_path.exists() {
        // Non-existent root is OK: treat as empty source (0 files, no error)
        return Ok(vec![]);
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
    Ok(found)
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
            // `format!("file://{}", path.display())`. It only returns `None`
            // for a non-absolute path, which `abs_path` isn't expected to be
            // given the `canonicalize()` above — but fall back defensively
            // rather than panic or silently drop the file.
            let uri = Uri::from_file_path(&abs_path).unwrap_or_else(|| {
                let fallback = format!("file://{}", abs_path.display());
                Uri::parse(&fallback).expect("fallback file:// URI must parse")
            });
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
    // Suppress the default panic hook's stderr output for expected third-party panics
    // (e.g. pdf-extract on malformed PDFs).  The caller emits a clean WARN line instead.
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
    },
    /// Server returned 304 Not Modified (conditional GET).
    NotModified,
    /// Document gone (404/410 after retry). Should trigger deletion.
    Gone,
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
/// Returns the number of chunks written.
pub async fn index_resource(
    resource: &Resource,
    source: &Source,
    replaces_resource_id: Option<&str>,
    deps: &IndexResourceDeps<'_>,
) -> Result<usize, Error> {
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
        // No chunks produced (empty resource): delete any old chunks if this
        // is a replace, write nothing new.
        if let Some(old_id) = replaces_resource_id {
            deps.store.delete_by_resource(old_id).await?;
        }
        return Ok(0);
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
        fetched_at: resource.modified_at.clone(),
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

    Ok(written)
}

/// Dependencies for [`run_source_ingestion`]: the mutable incremental-skip
/// index plus everything [`index_resource`] needs, grouped for a single run.
pub struct SourceIngestionDeps<'a> {
    pub doc_index: &'a mut DocumentIndex,
    pub store: &'a dyn RetrievalStore,
    pub embedder: &'a dyn Embedder,
    pub config: &'a IngestionConfig,
    pub progress: Option<crate::progress::ProgressSink>,
}

/// Run the unified ingestion pipeline for one source, driven by a caller-supplied
/// `&dyn Ingestor` (issue #117; specs/01-architecture.md §1).
///
/// Streams `Resource`s one at a time via [`PipelineCallback`] — no buffering of
/// an entire source's resources in memory. Per resource: skip-check (unchanged
/// `content_hash` + `policy_version`) → [`index_resource`] → counters/progress.
/// Per-resource errors become stats counters and progress events, never abort
/// the run. After `ingestor.ingest()` returns, runs the delete-sweep: any URI
/// previously indexed for this source that was neither yielded nor reported via
/// `on_skipped` this run is deleted — this is how a file deletion, source
/// removal, or (once ingestors report it) a Gone URL is swept.
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
        discovered_total: 0,
        next_index: 0,
        skip_error_count: 0,
    };

    let ingest_result = ingestor.ingest(&ingest_source, &mut callback).await?;

    let PipelineCallback {
        mut result,
        seen,
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

    // Delete-sweep: any URI known to this source's doc_index that was neither
    // yielded (on_resource) nor reported skipped (on_skipped) this run is
    // gone — delete it. A deleted file simply isn't enumerated again; a Gone
    // URL is simply never yielded. Restricting to `is_uri_from_source` guards
    // against sweeping another source's URIs out of a shared doc_index.
    let existing_uris = doc_index.uris();
    for uri in existing_uris {
        if !is_uri_from_source(&uri, source) {
            continue;
        }
        if seen.contains(&uri) {
            continue;
        }
        if let Some(old_record) = doc_index.remove(&uri) {
            let deleted = store.delete_by_resource(&old_record.resource_id).await?;
            if deleted > 0 {
                result.docs_deleted += 1;
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
            Ok(chunks_written) => {
                self.result.docs_indexed += 1;
                self.result.chunks_written += chunks_written as u64;
                self.doc_index.upsert(DocumentRecord {
                    uri: uri.clone(),
                    resource_id: resource.id.clone(),
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

    async fn on_skipped(&mut self, uri: &str, reason: SkipReason) {
        // Shadow the raw locator with its normalized form immediately, so
        // nothing below (the `seen` bookkeeping the delete-sweep reads, or
        // either progress event) can accidentally reach the un-normalized
        // `&str`. See `normalize_uri` for why the fallback it uses is safe.
        let uri = normalize_uri(uri);
        self.seen.insert(uri.clone());
        self.result.docs_seen += 1;
        self.start_document(&uri);

        match reason {
            SkipReason::Unchanged => {
                // Still alive, just unchanged — never re-index, never sweep.
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.clone(),
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
                    uri: uri.clone(),
                    outcome: crate::progress::DocOutcome::Unsupported,
                });
            }
            SkipReason::Other(_) => {
                // No direct old-path analog; nearest classification is a
                // (non-format, non-error) skip. Alive either way (marked seen
                // above), so it survives the sweep regardless.
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.clone(),
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
                    uri: uri.clone(),
                    outcome: crate::progress::DocOutcome::Error,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Normalize a raw URI/locator string to the same representation
/// `Resource.uri` carries everywhere else in the pipeline (percent-encoded
/// path bytes, lower-cased host, etc. — see `core/src/uri.rs`).
///
/// This is the single owner of the "parse with raw fallback" invariant: every
/// caller that needs to compare a raw locator (from an ingestor callback, a
/// config value, ...) against a `DocumentIndex`/store key must normalize
/// through this function first, or a raw string containing a space,
/// non-ASCII character, or non-canonical casing will never match its own
/// normalized resource and its document will silently survive (or be
/// dropped from) the delete-sweep incorrectly.
///
/// The `unwrap_or_else` fallback to the raw string is provably inert:
/// `DocumentIndex` is populated only by `on_resource`'s `doc_index.upsert`,
/// keyed by `resource.uri.as_str()`; every ingestor returns `Err` rather than
/// reaching `on_resource` if its own `Uri::parse` fails. Every
/// `DocumentIndex` key is therefore a successful parse's output, and a
/// string that *fails* to parse here can never equal one. The fallback only
/// prevents a panic or a dropped bookkeeping entry — it can never cause a
/// false match.
fn normalize_uri(raw: &str) -> String {
    crate::uri::Uri::parse(raw)
        .map(|u| u.as_str().to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// Check if a URI belongs to a given source.
///
/// For path sources, checks if the URI starts with `file://` + canonical root path.
/// For URL sources, checks if the URI matches the source URL.
///
/// # Normalization (delete-sweep false-negative fix)
///
/// `uri` here is always one already stored in the `DocumentIndex`/store — i.e.
/// `Resource.uri.as_str()`, which is a *normalized* `url::Url` string (percent
/// -encoded path bytes, lower-cased host, a trailing `/` added when the URL
/// crate considers the path empty, etc. — see `core/src/uri.rs`). Both arms
/// below MUST compare against that same normalized representation, not a raw
/// string built from config/filesystem data, or a root/URL containing a space,
/// non-ASCII character, or non-canonical casing/trailing-slash would never
/// match its own indexed resources and its documents would never be swept on
/// delete (silent under-deletion). Normalization only changes byte
/// *representation*, never decodes `/` vs `%2F`, so the boundary-aware
/// string comparison (exact match, or match immediately followed by a literal
/// `/`) remains sound: a percent-encoded slash in a URI can never be mistaken
/// for a path boundary.
fn is_uri_from_source(uri: &str, source: &Source) -> bool {
    match &source.spec {
        SourceSpec::Path { root, .. } => {
            // Resolve canonical root path (handles macOS /var -> /private/var symlink etc.)
            let canonical_root = std::path::Path::new(root)
                .canonicalize()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| root.clone());
            // Trim any trailing '/' so the boundary check below is
            // well-defined regardless of whether `root` came in (or
            // canonicalized) with a trailing separator.
            let canonical_root = canonical_root.trim_end_matches('/');
            let raw_prefix = format!("file://{}", canonical_root);

            // Normalize through the exact same `Uri`/`url::Url` pipeline that
            // produced the indexed URIs (`enumerate_path_source` builds
            // `file://<abs_path>`, then `FileIngestor` runs it through
            // `Uri::parse` before handing the `Resource` to core — see
            // ingest/src/file_ingestor.rs). Doing the same here keeps both
            // sides byte-for-byte comparable. If parsing fails for some
            // reason, fall back to the raw string via `normalize_uri`: a
            // false negative here only means "don't delete" (safe
            // direction), never a panic.
            let file_prefix = normalize_uri(&raw_prefix).trim_end_matches('/').to_string();

            // Boundary-aware match (C0): a plain `starts_with` here would
            // let a sibling source whose root is a *string* prefix of this
            // one (e.g. root=/data/blog vs root=/data/blog-drafts) be
            // misattributed as "from this source". That misattribution is
            // catastrophic during the delete-sweep in `run_source_ingestion`:
            // sweeping source A would delete source B's live resources
            // whenever B's ingestor didn't also run this cycle. Require an
            // exact match on the root itself, or a match followed by a path
            // separator, so only true descendants of `root` match.
            uri == file_prefix || uri.starts_with(&format!("{file_prefix}/"))
        }
        SourceSpec::Url { url, .. } => {
            // Normalize the configured URL the same way the indexed URI was
            // normalized (`normalize_uri`), so e.g. an uppercase host or a
            // missing trailing slash in config still matches. Already
            // boundary-safe either way: exact equality can't suffer the
            // string-prefix misattribution the path arm above guards
            // against. Falls back to raw comparison if the configured URL
            // fails to parse (should not happen for a validated source, but
            // never worse than the old behavior).
            uri == normalize_uri(url)
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

    #[test]
    fn enumerate_nonexistent_root_returns_empty() {
        let result = enumerate_path_source("/this/path/does/not/exist", &[], &[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn enumerate_path_source_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"# A").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"hello").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &[]).unwrap();
        assert_eq!(files.len(), 2, "should find both files");
    }

    #[test]
    fn enumerate_path_source_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), b"# Notes").unwrap();
        std::fs::write(dir.path().join("data.bin"), b"\x00\x01\x02").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &["*.md".to_string()], &[]).unwrap();
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
        let files = enumerate_path_source(root, &[], &["**/node_modules/**".to_string()]).unwrap();
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
        let files = enumerate_path_source(root, &[], &[".DS_Store".to_string()]).unwrap();
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
                .unwrap();
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
        let files = enumerate_path_source(root, &[], &["**/.DS_Store".to_string()]).unwrap();
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
        let files = enumerate_path_source(root, &["*.md".to_string()], &[]).unwrap();
        assert!(
            files.is_empty(),
            "bare *.md include must not match at depth"
        );
        // `**/*.md` does match.
        let files = enumerate_path_source(root, &["**/*.md".to_string()], &[]).unwrap();
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
        let files = enumerate_path_source(root, &[], &["**/node_modules".to_string()]).unwrap();
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
        let files = enumerate_path_source(root, &[], &[]).unwrap();
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
        let files = enumerate_path_source(root, &["*.md".to_string()], &[]).unwrap();
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
            window_block_seqs: vec![],
        }
    }

    // ---------------------------------------------------------------------------
    // C0 — is_uri_from_source boundary-aware matching
    // ---------------------------------------------------------------------------

    fn path_source(root: &str) -> Source {
        Source {
            id: new_ulid(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: root.to_string(),
                include: vec![],
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        }
    }

    fn url_source(url: &str) -> Source {
        Source {
            id: new_ulid(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Url,
            spec: SourceSpec::Url {
                url: url.to_string(),
                refresh_interval_secs: None,
            },
            source_preset: "prose".to_string(),
        }
    }

    #[test]
    fn is_uri_from_source_sibling_string_prefix_root_does_not_match() {
        // Regression (C0): root="/tmp/x/blog" is a *string* prefix of
        // "/tmp/x/blog-drafts", but the latter is NOT a path descendant of
        // the former. A plain `starts_with` on the raw prefix would
        // misattribute blog-drafts's URIs to the "blog" source.
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("blog")).unwrap();
        std::fs::create_dir_all(base.path().join("blog-drafts")).unwrap();

        let blog_root = base.path().join("blog").canonicalize().unwrap();
        let source = path_source(blog_root.to_str().unwrap());

        // The nested file need not exist; canonicalize only the parent dir.
        let blog_drafts_root = base.path().join("blog-drafts").canonicalize().unwrap();
        let sibling_uri = format!("file://{}/draft.md", blog_drafts_root.display());

        assert!(
            !is_uri_from_source(&sibling_uri, &source),
            "blog-drafts URI must NOT be attributed to the blog source"
        );
    }

    #[test]
    fn is_uri_from_source_exact_root_matches() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let source = path_source(canonical.to_str().unwrap());
        let uri = format!("file://{}", canonical.display());
        assert!(
            is_uri_from_source(&uri, &source),
            "the root URI itself must match its own source"
        );
    }

    #[test]
    fn is_uri_from_source_nested_file_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let source = path_source(canonical.to_str().unwrap());
        let uri = format!("file://{}/a/b/note.md", canonical.display());
        assert!(
            is_uri_from_source(&uri, &source),
            "a nested descendant file must match its source's root"
        );
    }

    #[test]
    fn is_uri_from_source_url_exact_match_only() {
        let source = url_source("https://example.com/blog");
        assert!(is_uri_from_source("https://example.com/blog", &source));
        // A URI that is merely a string-prefix-extension of the source URL
        // must not match either — equality is already boundary-safe here,
        // but pin the behavior explicitly alongside the path-arm fix.
        assert!(!is_uri_from_source(
            "https://example.com/blog-drafts/1",
            &source
        ));
        assert!(!is_uri_from_source("https://example.com/blog/1", &source));
    }

    // ---------------------------------------------------------------------------
    // Normalization fix — path root / config URL must compare against the same
    // normalized representation the indexed URIs use (percent-encoding, host
    // case-folding, url-crate trailing-slash-on-empty-path), or the
    // delete-sweep silently never matches (under-deletion).
    // ---------------------------------------------------------------------------

    /// Pin exactly what `url::Url` (via `crate::uri::Uri`) does to a `file://`
    /// URI built from a path containing a space and to an `https://` URI with
    /// an uppercase host — this is the normalization the fix must match.
    #[test]
    fn uri_normalization_pins_percent_encoding_and_host_lowercasing() {
        let file_uri = crate::uri::Uri::parse("file:///a/My Docs/x.md").unwrap();
        assert_eq!(file_uri.as_str(), "file:///a/My%20Docs/x.md");

        let http_uri = crate::uri::Uri::parse("https://EXAMPLE.com/Path").unwrap();
        assert_eq!(http_uri.as_str(), "https://example.com/Path");

        let no_path_uri = crate::uri::Uri::parse("https://example.com").unwrap();
        assert_eq!(no_path_uri.as_str(), "https://example.com/");
    }

    #[test]
    fn is_uri_from_source_path_root_with_space_matches_percent_encoded_resource_uri() {
        // Regression: a root containing a space (or any percent-encodable
        // char) previously never matched its own resources' normalized
        // URIs, so a deleted file under it was never swept.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("My Docs")).unwrap();
        let root = dir.path().join("My Docs").canonicalize().unwrap();
        let source = path_source(root.to_str().unwrap());

        // Build the resource URI exactly the way enumerate_path_source +
        // FileIngestor do: raw `file://<abs_path>` then `Uri::parse`.
        let raw = format!("file://{}/notes.md", root.display());
        let resource_uri = crate::uri::Uri::parse(&raw).unwrap();

        assert!(
            is_uri_from_source(resource_uri.as_str(), &source),
            "space-containing root must match its own percent-encoded resource URI"
        );
    }

    #[test]
    fn is_uri_from_source_path_root_itself_with_space_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("My Docs")).unwrap();
        let root = dir.path().join("My Docs").canonicalize().unwrap();
        let source = path_source(root.to_str().unwrap());

        let raw = format!("file://{}", root.display());
        let resource_uri = crate::uri::Uri::parse(&raw).unwrap();

        assert!(is_uri_from_source(resource_uri.as_str(), &source));
    }

    #[test]
    fn is_uri_from_source_url_uppercase_host_matches_normalized_resource_uri() {
        // Config says "EXAMPLE.com"; the indexed resource URI (as it would
        // have been normalized by Uri::parse when the resource was fetched)
        // is lower-cased. The comparison must normalize the config side too.
        let source = url_source("https://EXAMPLE.com/blog");
        let resource_uri = crate::uri::Uri::parse("https://EXAMPLE.com/blog").unwrap();
        assert_eq!(resource_uri.as_str(), "https://example.com/blog");
        assert!(is_uri_from_source(resource_uri.as_str(), &source));
    }

    #[test]
    fn is_uri_from_source_url_missing_trailing_slash_matches_normalized_resource_uri() {
        // url::Url adds a trailing "/" when the path is empty; config
        // omitting it must still match the normalized indexed URI.
        let source = url_source("https://example.com");
        let resource_uri = crate::uri::Uri::parse("https://example.com").unwrap();
        assert_eq!(resource_uri.as_str(), "https://example.com/");
        assert!(is_uri_from_source(resource_uri.as_str(), &source));
    }

    #[test]
    fn is_uri_from_source_percent_encoded_slash_cannot_fake_a_path_boundary() {
        // A literal "%2F" (percent-encoded '/') appearing right after the
        // root string must NOT be treated as a path separator: comparison is
        // purely string-level (never percent-decoded), so "root%2Fevil" is
        // correctly rejected as a distinct, non-descendant string — the
        // boundary check requires an actual '/' byte immediately after the
        // root, not a decoded one.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("blog")).unwrap();
        let root = dir.path().join("blog").canonicalize().unwrap();
        let source = path_source(root.to_str().unwrap());

        let fake_uri = format!("file://{}%2Fevil", root.display());
        assert!(
            !is_uri_from_source(&fake_uri, &source),
            "a percent-encoded slash must not be treated as a path boundary"
        );
    }

    #[test]
    fn is_uri_from_source_sibling_string_prefix_root_still_does_not_match_after_normalization() {
        // Re-pin C0 alongside the normalization fix: normalizing both sides
        // must not reintroduce the sibling string-prefix bug.
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("blog")).unwrap();
        std::fs::create_dir_all(base.path().join("blog-drafts")).unwrap();

        let blog_root = base.path().join("blog").canonicalize().unwrap();
        let source = path_source(blog_root.to_str().unwrap());

        let blog_drafts_root = base.path().join("blog-drafts").canonicalize().unwrap();
        let raw = format!("file://{}/draft.md", blog_drafts_root.display());
        let sibling_uri = crate::uri::Uri::parse(&raw).unwrap();

        assert!(!is_uri_from_source(sibling_uri.as_str(), &source));
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
        }

        struct FakeIngestor {
            script: std::sync::Mutex<Vec<ScriptStep>>,
        }

        impl FakeIngestor {
            fn new(script: Vec<ScriptStep>) -> Self {
                Self {
                    script: std::sync::Mutex::new(script),
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
                            callback.on_skipped(&uri, reason).await;
                        }
                    }
                }
                Ok(IngestResult {
                    resources_produced: produced,
                    resources_skipped: skipped,
                    errors,
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
        // 5b. Regression: delete-sweep must fire for a file under a
        // space-containing root. Before the normalization fix,
        // `is_uri_from_source`'s Path arm built its prefix from the raw
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
            let found = enumerate_path_source(root.to_str().unwrap(), &[], &[]).unwrap();
            assert_eq!(found.len(), 1);
            let normalized_uri = &found[0].uri;
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

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            // Simulate the file having been deleted from disk: this run's
            // ingestor yields nothing at all.
            let ingestor = FakeIngestor::new(vec![]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
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
            let uri_b = format!("file://{}/draft.md", blog_drafts_root.display());

            // Both sources' documents share the same store-level doc_index —
            // exactly the shared-store scenario the finding describes.
            let record_a =
                seed_indexed(&store, &embedder, &config, &source_a, &uri_a, "Blog post.").await;
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
            doc_index.upsert(record_b.clone());

            // Sweep source A only: its ingestor finds nothing at all this
            // run (simulating, e.g., every file under blog/ having been
            // deleted). Source B's ingestor does NOT run this cycle.
            let ingestor = FakeIngestor::new(vec![]);
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
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
        // 8. URL-Gone-style absence is swept (Url-kind source)
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn gone_url_style_absence_is_swept() {
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

            // The URL now 404s/410s — the ingestor simply never yields it
            // (and never reports it via on_skipped either).
            let ingestor = FakeIngestor::new(vec![]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
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
        // 8b. Regression: `on_skipped` inserts the RAW locator the ingestor
        // handed it into `seen`, while the delete-sweep iterates
        // `doc_index.uris()` — always the NORMALIZED `Resource.uri`
        // representation (percent-encoded path bytes, lower-cased host).
        // When a raw locator differs from its normalized form, `seen`
        // and the sweep's key space disagree and a live document gets
        // deleted out from under a skip that should have kept it alive.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn on_skipped_space_in_path_survives_sweep() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            // What a real file ingestor would pass to `on_skipped` — the raw,
            // non-percent-encoded path.
            let raw_uri = "file:///docs/my file.md";
            let normalized_uri = crate::uri::Uri::parse(raw_uri).unwrap();
            assert_eq!(
                normalized_uri.as_str(),
                "file:///docs/my%20file.md",
                "sanity: the space must be percent-encoded in the normalized form"
            );

            // Seed prior state exactly as `on_resource` would have written it:
            // keyed by the NORMALIZED uri (see `Resource.uri.as_str()` at
            // ingestion.rs:1010).
            let record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                normalized_uri.as_str(),
                "Space path content.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            // A transient read failure this run: the real ingestor reports it
            // via the raw (unnormalized) locator string.
            let ingestor = FakeIngestor::new(vec![ScriptStep::Skipped(
                raw_uri.to_string(),
                SkipReason::Error("transient read failure".to_string()),
            )]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 0,
                "a transient read error must never delete a live document merely \
                 because its raw locator and normalized URI differ"
            );
            assert!(
                doc_index.get(normalized_uri.as_str()).is_some(),
                "the doc_index entry must survive the sweep"
            );
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(
                !chunks.is_empty(),
                "the existing chunks must not be swept away"
            );
        }

        // -----------------------------------------------------------------
        // 8c. Same class of bug, `SourceKind::Url`: an uppercase host in the
        // raw locator an ingestor reports via `on_skipped` (e.g.
        // `SkipReason::Unchanged`, the steady state for a URL source) must
        // still match the lower-cased host stored in the doc_index, or the
        // document is deleted on every unchanged run.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn on_skipped_uppercase_host_survives_sweep() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = Source {
                id: new_ulid(),
                store_id: store_id.to_string(),
                kind: SourceKind::Url,
                spec: SourceSpec::Url {
                    url: "https://example.com/docs".to_string(),
                    refresh_interval_secs: None,
                },
                source_preset: "prose".to_string(),
            };

            let normalized_uri = "https://example.com/docs";
            let record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                normalized_uri,
                "Docs page body.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(record.clone());

            // The ingestor's own unchanged-check reports this URL as
            // unchanged, using the raw (uppercase-host) locator it was
            // configured with.
            let raw_uri = "https://Example.com/docs";
            assert_ne!(
                raw_uri, normalized_uri,
                "sanity: the raw locator's host casing must differ from normalized"
            );
            let ingestor = FakeIngestor::new(vec![ScriptStep::Skipped(
                raw_uri.to_string(),
                SkipReason::Unchanged,
            )]);

            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: None,
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 0,
                "SkipReason::Unchanged must never delete a live document merely \
                 because the raw locator's host casing differs from the \
                 normalized, lower-cased host stored in the doc_index"
            );
            assert!(doc_index.get(normalized_uri).is_some());
            let chunks = store
                .get_chunks_for_resource(&record.resource_id)
                .await
                .unwrap();
            assert!(!chunks.is_empty());
        }

        // -----------------------------------------------------------------
        // 8d. `on_skipped`'s progress events must carry the same normalized
        // URI representation that `on_resource` uses, not the raw locator —
        // otherwise a progress consumer (CLI output, HTTP job status) can't
        // correlate a `DocumentStarted`/`DocumentFinished` pair for a
        // skipped item with the URI actually tracked in the doc_index.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn on_skipped_progress_events_carry_normalized_uri() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let raw_uri = "file:///docs/My File.MD";
            let normalized_uri = crate::uri::Uri::parse(raw_uri).unwrap();
            let normalized_uri = normalized_uri.as_str().to_string();
            assert_ne!(
                raw_uri, normalized_uri,
                "sanity: raw and normalized must differ for this fixture"
            );

            let mut doc_index = DocumentIndex::new();
            let ingestor = FakeIngestor::new(vec![ScriptStep::Skipped(
                raw_uri.to_string(),
                SkipReason::Unsupported,
            )]);

            let (sink, events) = progress_collector();
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: Some(sink),
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();
            assert_eq!(result.docs_deleted, 0);

            let events = events.lock().unwrap();
            let started_uri = events.iter().find_map(|e| match e {
                ProgressEvent::DocumentStarted { uri, .. } => Some(uri.clone()),
                _ => None,
            });
            let finished_uri = events.iter().find_map(|e| match e {
                ProgressEvent::DocumentFinished { uri, .. } => Some(uri.clone()),
                _ => None,
            });
            assert_eq!(
                started_uri.as_deref(),
                Some(normalized_uri.as_str()),
                "DocumentStarted must carry the normalized URI, not the raw locator"
            );
            assert_eq!(
                finished_uri.as_deref(),
                Some(normalized_uri.as_str()),
                "DocumentFinished must carry the normalized URI, not the raw locator"
            );
        }

        // -----------------------------------------------------------------
        // 8e. Pin the fallback contract: a locator that isn't a parseable URI
        // at all must never panic the pipeline, and must not disturb an
        // unrelated, already-indexed document swept in the same run. This
        // must hold both before and after the `normalize_uri` fix lands.
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn on_skipped_unparseable_locator_falls_back_without_panic() {
            let store = FakeStore::new();
            let embedder = FakeEmbedder::new(4);
            let store_id = "store-1";
            let config = make_ingestion_config(store_id);
            let source = make_source_with_preset(store_id, "prose");

            let other_uri = "file:///docs/other.md";
            let other_record = seed_indexed(
                &store,
                &embedder,
                &config,
                &source,
                other_uri,
                "Other content.",
            )
            .await;

            let mut doc_index = DocumentIndex::new();
            doc_index.upsert(other_record.clone());

            let bogus = "not a valid uri";
            let ingestor = FakeIngestor::new(vec![
                ScriptStep::Skipped(
                    bogus.to_string(),
                    SkipReason::Error("garbage locator".to_string()),
                ),
                ScriptStep::Skipped(other_uri.to_string(), SkipReason::Unchanged),
            ]);

            let (sink, events) = progress_collector();
            let deps = SourceIngestionDeps {
                doc_index: &mut doc_index,
                store: &store,
                embedder: &embedder,
                config: &config,
                progress: Some(sink),
            };
            let result = run_source_ingestion(&source, &ingestor, deps)
                .await
                .unwrap();

            assert_eq!(
                result.docs_deleted, 0,
                "an unparseable locator must never panic, and the unrelated \
                 already-indexed document must survive the sweep"
            );
            assert_eq!(result.error_count, 1);
            let other_chunks = store
                .get_chunks_for_resource(&other_record.resource_id)
                .await
                .unwrap();
            assert!(!other_chunks.is_empty());
            assert!(doc_index.get(other_uri).is_some());

            // Fallback contract: a locator that fails URI normalization must
            // still surface verbatim (identity fallback), never panic.
            let events = events.lock().unwrap();
            let bogus_finished = events
                .iter()
                .any(|e| matches!(e, ProgressEvent::DocumentFinished { uri, .. } if uri == bogus));
            assert!(
                bogus_finished,
                "the unparseable locator must still surface verbatim via DocumentFinished"
            );
        }

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
        // Extra: empty-resource replace deletes old chunks and writes nothing
        // -----------------------------------------------------------------

        #[tokio::test]
        async fn index_resource_empty_blocks_deletes_old_and_writes_nothing() {
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
            let written = index_resource(
                &empty_resource,
                &source,
                Some(&old_record.resource_id),
                &deps,
            )
            .await
            .unwrap();

            assert_eq!(written, 0);
            let old_chunks = store
                .get_chunks_for_resource(&old_record.resource_id)
                .await
                .unwrap();
            assert!(
                old_chunks.is_empty(),
                "replacing with an empty resource must delete the old chunks"
            );
        }
    }
}
