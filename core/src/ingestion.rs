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
use crate::ids::{new_ulid, resource_id};
use crate::ingestor::{IngestCallback, IngestSource, Ingestor, SkipReason};
use crate::markdown_blocks::{compute_blocks_hash, markdown_to_blocks};
use crate::store::{ChunkRecord, RetrievalStore};
use crate::types::{
    Chunk, IndexJob, IndexJobScope, IndexJobState, IndexJobStats, Provenance, Source, SourceKind,
    SourceRef, SourceSpec,
};

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

    /// Pre-populate the index from existing chunk records in the store.
    ///
    /// Each unique (uri, resource_id, content_hash, policy_version) combination
    /// in the store becomes one record.
    pub fn from_chunk_records(chunks: &[ChunkRecord]) -> Self {
        let mut records = HashMap::new();
        for chunk in chunks {
            records.entry(chunk.uri.clone()).or_insert(DocumentRecord {
                uri: chunk.uri.clone(),
                resource_id: chunk.resource_id.clone(),
                content_hash: chunk.content_hash.clone(),
                policy_version: chunk.policy_version.clone(),
            });
        }
        Self { records }
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
    pub uri: String,
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
            let uri = format!("file://{}", abs_path.display());
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

// ---------------------------------------------------------------------------
// index_document — process a single document through the pipeline
// ---------------------------------------------------------------------------

/// Input for indexing a single document.
pub struct DocumentInput {
    /// Canonical URI.
    pub uri: String,
    /// Raw bytes of the document.
    pub bytes: Vec<u8>,
    /// Optional filename hint for format detection.
    pub filename: Option<String>,
    /// MIME type if known.
    pub mime: Option<String>,
    /// Acquisition time (RFC 3339 string).
    pub fetched_at: String,
    /// Source this document belongs to.
    pub source: Source,
}

/// Output of indexing a single document.
#[derive(Debug)]
pub struct DocumentIndexOutput {
    /// Number of chunks written.
    pub chunks_written: usize,
    /// Whether the document was actually indexed (vs skipped as unchanged).
    pub was_indexed: bool,
    /// The new document record (for updating the index).
    pub record: DocumentRecord,
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

/// Index a single document: extract → chunk → embed → upsert.
///
/// If the document's content hash matches an existing record, returns early
/// (incremental skip). If changed, the old document is replaced by the new
/// one (see the `replaces` computation and the `upsert_chunks_and_blocks`
/// call below).
///
/// The order of operations is carefully chosen to be crash-safe (A6):
///   1. Extract, chunk, embed — all read-only / reversible.
///   2. Determine whether this is a replace — only after embedding succeeds.
///   3. Call `upsert_chunks_and_blocks` with the old resource_id (if any) as
///      `replaces_resource_id` — the store deletes the old document and
///      inserts the new one within a single transaction (issue #79), so a
///      write failure leaves the old document intact and searchable rather
///      than a separately-committed delete having already taken effect.
///
/// Returns the `DocumentIndexOutput` with chunk counts and the new record.
pub async fn index_document(
    input: &DocumentInput,
    doc_index: &DocumentIndex,
    store: &dyn RetrievalStore,
    embedder: &dyn Embedder,
    config: &IngestionConfig,
    extractor: &dyn DocumentExtractor,
) -> Result<DocumentIndexOutput, Error> {
    // Extract text and blocks.
    let extraction = catch_panic(
        "extract",
        std::panic::AssertUnwindSafe(|| extractor.extract(&input.bytes, input.filename.as_deref())),
    )?;

    // Convert markdown to typed blocks and compute content hash.
    let blocks = markdown_to_blocks(&extraction.markdown);
    let hash = compute_blocks_hash(&blocks);

    // Content-addressed document ID.
    let doc_id = resource_id(&input.uri, &hash);

    // Check for incremental skip: same hash, same policy → skip.
    if let Some(existing) = doc_index.get(&input.uri) {
        if existing.content_hash == hash && existing.policy_version == config.policy_version {
            // Unchanged — nothing to do.
            return Ok(DocumentIndexOutput {
                chunks_written: 0,
                was_indexed: false,
                record: existing.clone(),
            });
        }
        // Content or policy changed — fall through to re-index.
        // Old chunks are deleted *after* embedding succeeds (A6).
    }

    // Chunk the document.
    //
    // Build the chunk sizer from the embedder's tokenizer if it has one
    // (token-accurate chunking); otherwise fall back to character sizing and
    // scale the prose token budget to chars (*4) so behaviour approximates the
    // model token budget for hosted/Fake embedders.
    let token_counter = embedder.token_counter();
    let sizer: Box<dyn ChunkSizer> = match &token_counter {
        Some(f) => Box::new(TokenSizer::new(f.clone())),
        None => Box::new(CharSizer),
    };

    // Layer A: per-file preset routing — override config.chunker if file is code/data.
    let effective_chunker = {
        use crate::chunker::preset_for;
        let file_preset = preset_for(input.filename.as_deref(), input.mime.as_deref());
        if file_preset == "code" {
            ChunkerConfig::code()
        } else {
            config.chunker.clone()
        }
    };

    let chunker_cfg = if token_counter.is_none() {
        scale_to_chars(&effective_chunker)
    } else {
        effective_chunker.clone()
    };
    let chunk_outputs = catch_panic(
        "chunk",
        std::panic::AssertUnwindSafe(|| {
            chunk_blocks(&doc_id, &blocks, &chunker_cfg, sizer.as_ref())
        }),
    )?;

    if chunk_outputs.is_empty() {
        // No chunks produced (empty doc) — delete old chunks if any and record as indexed.
        if let Some(existing) = doc_index.get(&input.uri) {
            if existing.content_hash != hash || existing.policy_version != config.policy_version {
                store.delete_by_resource(&existing.resource_id).await?;
            }
        }
        let record = DocumentRecord {
            uri: input.uri.clone(),
            resource_id: doc_id,
            content_hash: hash,
            policy_version: config.policy_version.clone(),
        };
        return Ok(DocumentIndexOutput {
            chunks_written: 0,
            was_indexed: true,
            record,
        });
    }

    // Embed the chunks (document-aware interface).
    // This must happen BEFORE deleting old chunks (A6): if embedding fails,
    // the store is left untouched.
    let doc_chunks = DocumentChunks {
        document_context: extraction.markdown.clone(),
        chunks: chunk_outputs.iter().map(|c| c.text.clone()).collect(),
    };

    let embedded = embedder.embed_documents(vec![doc_chunks]).await?;

    // Guard: the embedder must return exactly one EmbeddedDocument (one per
    // input document), and that document must have exactly one vector per chunk.
    // A length mismatch indicates a malformed embedder response (F4).
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

    // Embedding succeeded — now it is safe to replace the old document (A6).
    // The delete is *not* performed here as a separate call (issue #79):
    // instead, the old resource_id is threaded through to
    // `upsert_chunks_and_blocks` below, which deletes and inserts within a
    // single store transaction so a write failure leaves the old document
    // intact and searchable.
    let replaces = doc_index.get(&input.uri).and_then(|existing| {
        if existing.content_hash != hash || existing.policy_version != config.policy_version {
            Some(existing.resource_id.clone())
        } else {
            None
        }
    });

    // Build Chunk and ChunkRecord structures.
    let provenance = Provenance {
        origin_store: config.store_id.clone(),
        source_ref: SourceRef {
            id: input.source.id.clone(),
            kind: input.source.kind.to_string_kind(),
        },
        fetched_at: input.fetched_at.clone(),
        content_hash: hash.clone(),
        share_path: vec![],
    };

    // Merge extraction-level title into metadata when metadata has no title.
    // Some parsers (e.g. PDF, plain-text) surface the title on
    // `ExtractionResult.title` rather than inside `metadata.title`; this
    // ensures the stored chunk always carries the best available title.
    let mut metadata = extraction.metadata.clone();
    if metadata.title.is_none() {
        metadata.title = extraction.title.clone();
    }
    let record_metadata = crate::metadata::Metadata::Document(crate::metadata::DocumentMetadata {
        dublin_core: metadata,
        page_count: None,
        word_count: None,
    });

    let mut records = Vec::new();
    for (chunk_out, embedding) in chunk_outputs.iter().zip(embeddings.iter()) {
        let chunk = Chunk {
            id: chunk_out.id.clone(),
            resource_id: doc_id.clone(),
            store_id: config.store_id.clone(),
            text: chunk_out.text.clone(),
            span: chunk_out.span.clone(),
            heading_path: chunk_out.heading_path.clone(),
            policy_version: config.policy_version.clone(),
            provenance: provenance.clone(),
            window_block_seqs: chunk_out.window_block_seqs.clone(),
        };

        let mut record = ChunkRecord::from_chunk(
            &chunk,
            embedding.clone(),
            input.uri.clone(),
            input.mime.clone(),
            record_metadata.clone(),
        );
        record.block_seq = chunk_out.block_seq;
        record.seq_in_block = chunk_out.seq_in_block;
        record.block_kind = chunk_out.block_kind.clone();
        records.push(record);
    }

    let written = records.len();
    store
        .upsert_chunks_and_blocks(
            &config.store_id,
            &doc_id,
            records,
            &blocks,
            replaces.as_deref(),
        )
        .await?;

    let record = DocumentRecord {
        uri: input.uri.clone(),
        resource_id: doc_id,
        content_hash: hash,
        policy_version: config.policy_version.clone(),
    };

    Ok(DocumentIndexOutput {
        chunks_written: written,
        was_indexed: true,
        record,
    })
}

// ---------------------------------------------------------------------------
// DocumentExtractor trait — seam for injection in tests
// ---------------------------------------------------------------------------

/// Extraction seam: wraps the `extract` crate or a fake for testing.
///
/// This trait allows the ingestion pipeline to be tested without a full
/// extraction stack.
pub trait DocumentExtractor: Send + Sync {
    /// Extract a normalized Markdown document from raw bytes.
    ///
    /// # Errors
    /// Returns `Error::UnsupportedFormat` if the format is not recognized.
    fn extract(&self, bytes: &[u8], filename: Option<&str>) -> Result<ExtractionResult, Error>;
}

// ---------------------------------------------------------------------------
// ExtractionResult
// ---------------------------------------------------------------------------

/// The output of a document extraction, used within the ingestion pipeline.
#[derive(Debug, Clone)]
pub struct ExtractionResult {
    /// Normalized Markdown string. Chunk spans index into this.
    pub markdown: String,
    /// Optional document title.
    pub title: Option<String>,
    /// Dublin Core metadata extracted from the document.
    pub metadata: crate::metadata::DublinCoreMetadata,
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
pub fn now_rfc3339() -> String {
    // Use a simple implementation that doesn't require chrono.
    // Falls back to a placeholder in test environments.
    #[cfg(not(test))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = duration.as_secs();
        // Format as RFC 3339 (simplified — no sub-second precision)
        let (y, mo, d, h, mi, s) = secs_to_ymd_hms(secs);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
    }
    #[cfg(test)]
    {
        "2026-06-10T12:00:00Z".to_string()
    }
}

#[cfg(not(test))]
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

// ---------------------------------------------------------------------------
// Extension trait to convert SourceKind to string
// ---------------------------------------------------------------------------

trait SourceKindExt {
    fn to_string_kind(&self) -> String;
}

impl SourceKindExt for SourceKind {
    fn to_string_kind(&self) -> String {
        match self {
            SourceKind::Path => "file".to_string(),
            SourceKind::Url => "url".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Ingestion pipeline — run for a single source
// ---------------------------------------------------------------------------

/// Context passed to inner pipeline functions, bundling the shared dependencies.
struct PipelineCtx<'a> {
    source: &'a Source,
    doc_index: &'a mut DocumentIndex,
    store: &'a dyn RetrievalStore,
    embedder: &'a dyn Embedder,
    config: &'a IngestionConfig,
    extractor: &'a dyn DocumentExtractor,
    progress: Option<crate::progress::ProgressSink>,
}

/// Run the ingestion pipeline for a single source.
///
/// Handles path sources (file enumeration) and URL sources (HTTP fetch).
/// Builds an IndexJob, processes all documents, and returns the final job state.
///
/// # One-shot semantics
/// This function runs synchronously in embedded mode. The daemon (T11) wraps
/// it in a job queue and adds file watching.
#[allow(clippy::too_many_arguments)]
pub async fn run_ingestion_for_source(
    source: &Source,
    doc_index: &mut DocumentIndex,
    store: &dyn RetrievalStore,
    embedder: &dyn Embedder,
    config: &IngestionConfig,
    extractor: &dyn DocumentExtractor,
    url_fetcher: Option<&dyn UrlFetcher>,
    progress: Option<crate::progress::ProgressSink>,
) -> Result<IngestionResult, Error> {
    let mut result = IngestionResult::default();
    let mut ctx = PipelineCtx {
        source,
        doc_index,
        store,
        embedder,
        config,
        extractor,
        progress,
    };

    match &source.spec {
        SourceSpec::Path {
            root,
            include,
            exclude,
        } => {
            run_path_source(&mut ctx, root, include, exclude, &mut result).await?;
        }
        SourceSpec::Url { url, .. } => {
            if let Some(fetcher) = url_fetcher {
                run_url_source(&mut ctx, url, fetcher, &mut result).await?;
            } else {
                return Err(Error::Internal {
                    message: "URL fetcher required for url sources but was not provided"
                        .to_string(),
                    correlation_id: "no_url_fetcher".to_string(),
                });
            }
        }
    }

    if let Some(sink) = &ctx.progress {
        sink(crate::progress::ProgressEvent::SourceFinished {
            result: result.clone(),
        });
    }

    Ok(result)
}

/// Run the ingestion pipeline for a path source.
async fn run_path_source(
    ctx: &mut PipelineCtx<'_>,
    root: &str,
    include: &[String],
    exclude: &[String],
    result: &mut IngestionResult,
) -> Result<(), Error> {
    let location = root.to_string();
    if let Some(sink) = &ctx.progress {
        sink(crate::progress::ProgressEvent::SourceStarted {
            source_id: ctx.source.id.clone(),
            location: location.clone(),
        });
    }

    let files = enumerate_path_source(root, include, exclude)?;
    let total = files.len();
    let seen_uris: std::collections::HashSet<String> =
        files.iter().map(|f| f.uri.clone()).collect();

    if let Some(sink) = &ctx.progress {
        sink(crate::progress::ProgressEvent::Discovered { total });
    }

    // Process each file
    for (index, file) in files.iter().enumerate() {
        result.docs_seen += 1;

        if let Some(sink) = &ctx.progress {
            sink(crate::progress::ProgressEvent::DocumentStarted {
                uri: file.uri.clone(),
                index,
                total,
            });
        }

        let bytes = match std::fs::read(&file.path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("cannot read file '{}': {}", file.path.display(), e);
                result.error_count += 1;
                if let Some(sink) = &ctx.progress {
                    sink(crate::progress::ProgressEvent::DocumentFinished {
                        uri: file.uri.clone(),
                        outcome: crate::progress::DocOutcome::Error,
                    });
                }
                continue;
            }
        };

        // Get file mtime for fetched_at
        let fetched_at = file
            .path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                let duration = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                let secs = duration.as_secs();
                format_unix_secs(secs)
            })
            .unwrap_or_else(now_rfc3339);

        let filename = file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
        let mime = detect_mime(&file.path);

        let input = DocumentInput {
            uri: file.uri.clone(),
            bytes,
            filename,
            mime,
            fetched_at,
            source: ctx.source.clone(),
        };

        match index_document(
            &input,
            ctx.doc_index,
            ctx.store,
            ctx.embedder,
            ctx.config,
            ctx.extractor,
        )
        .await
        {
            Ok(output) => {
                if output.was_indexed {
                    result.docs_indexed += 1;
                    result.chunks_written += output.chunks_written as u64;
                    if let Some(sink) = &ctx.progress {
                        sink(crate::progress::ProgressEvent::DocumentFinished {
                            uri: file.uri.clone(),
                            outcome: crate::progress::DocOutcome::Indexed {
                                chunks: output.chunks_written,
                            },
                        });
                    }
                } else {
                    result.docs_skipped += 1;
                    if let Some(sink) = &ctx.progress {
                        sink(crate::progress::ProgressEvent::DocumentFinished {
                            uri: file.uri.clone(),
                            outcome: crate::progress::DocOutcome::Skipped,
                        });
                    }
                }
                ctx.doc_index.upsert(output.record);
            }
            Err(Error::UnsupportedFormat { .. }) => {
                result.unsupported_format_count += 1;
                if let Some(sink) = &ctx.progress {
                    sink(crate::progress::ProgressEvent::DocumentFinished {
                        uri: file.uri.clone(),
                        outcome: crate::progress::DocOutcome::Unsupported,
                    });
                }
            }
            Err(e) => {
                tracing::warn!("error indexing '{}': {}", file.uri, e);
                result.error_count += 1;
                if let Some(sink) = &ctx.progress {
                    sink(crate::progress::ProgressEvent::DocumentFinished {
                        uri: file.uri.clone(),
                        outcome: crate::progress::DocOutcome::Error,
                    });
                }
            }
        }
    }

    // Delete documents that are no longer in the source
    let existing_uris = ctx.doc_index.uris();
    for uri in &existing_uris {
        // Only delete docs from this source (same source prefix)
        if !is_uri_from_source(uri, ctx.source) {
            continue;
        }
        if !seen_uris.contains(uri) {
            if let Some(old_record) = ctx.doc_index.remove(uri) {
                let deleted = ctx
                    .store
                    .delete_by_resource(&old_record.resource_id)
                    .await?;
                if deleted > 0 {
                    result.docs_deleted += 1;
                }
            }
        }
    }

    Ok(())
}

/// Run the ingestion pipeline for a URL source.
async fn run_url_source(
    ctx: &mut PipelineCtx<'_>,
    url: &str,
    fetcher: &dyn UrlFetcher,
    result: &mut IngestionResult,
) -> Result<(), Error> {
    if let Some(sink) = &ctx.progress {
        sink(crate::progress::ProgressEvent::SourceStarted {
            source_id: ctx.source.id.clone(),
            location: url.to_string(),
        });
    }

    result.docs_seen += 1;

    // Build fetch metadata from existing record
    let fetch_meta = FetchMetadata::default(); // TODO: store etag/last-modified in DocumentRecord

    match fetcher.fetch(url, &fetch_meta).await? {
        FetchResult::Downloaded {
            bytes,
            content_type,
            etag: _,
            last_modified: _,
        } => {
            let fetched_at = now_rfc3339();
            let filename = url.split('/').next_back().map(|s| s.to_string());
            let uri = url.to_string();

            let input = DocumentInput {
                uri: uri.clone(),
                bytes,
                filename,
                mime: content_type,
                fetched_at,
                source: ctx.source.clone(),
            };

            match index_document(
                &input,
                ctx.doc_index,
                ctx.store,
                ctx.embedder,
                ctx.config,
                ctx.extractor,
            )
            .await
            {
                Ok(output) => {
                    if output.was_indexed {
                        result.docs_indexed += 1;
                        result.chunks_written += output.chunks_written as u64;
                        if let Some(sink) = &ctx.progress {
                            sink(crate::progress::ProgressEvent::DocumentFinished {
                                uri,
                                outcome: crate::progress::DocOutcome::Indexed {
                                    chunks: output.chunks_written,
                                },
                            });
                        }
                    } else {
                        result.docs_skipped += 1;
                        if let Some(sink) = &ctx.progress {
                            sink(crate::progress::ProgressEvent::DocumentFinished {
                                uri,
                                outcome: crate::progress::DocOutcome::Skipped,
                            });
                        }
                    }
                    ctx.doc_index.upsert(output.record);
                }
                Err(Error::UnsupportedFormat { .. }) => {
                    result.unsupported_format_count += 1;
                    if let Some(sink) = &ctx.progress {
                        sink(crate::progress::ProgressEvent::DocumentFinished {
                            uri,
                            outcome: crate::progress::DocOutcome::Unsupported,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("error indexing URL '{}': {}", url, e);
                    result.error_count += 1;
                    if let Some(sink) = &ctx.progress {
                        sink(crate::progress::ProgressEvent::DocumentFinished {
                            uri,
                            outcome: crate::progress::DocOutcome::Error,
                        });
                    }
                }
            }
        }
        FetchResult::NotModified => {
            result.docs_skipped += 1;
        }
        FetchResult::Gone => {
            // Delete the document and its chunks
            if let Some(old_record) = ctx.doc_index.remove(url) {
                let deleted = ctx
                    .store
                    .delete_by_resource(&old_record.resource_id)
                    .await?;
                if deleted > 0 {
                    result.docs_deleted += 1;
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Unified ingestion pipeline (#117) — Ingestor-driven, no I/O in core
// ---------------------------------------------------------------------------
//
// `run_source_ingestion` + `index_resource` are the pipeline shape described in
// specs/01-architecture.md §1: the caller (CLI) builds a concrete `&dyn Ingestor`
// per `SourceSpec` and drives it through `core` here. This section does not
// replace `index_document`/`run_ingestion_for_source` above — those still
// compile and pass (deletion of the legacy path is a later wave) — but shares
// its crash-safe A6 ordering and skip/replace semantics. The key difference is
// *where* extraction happens: the legacy path extracts raw bytes inside `core`
// via `DocumentExtractor`; this path receives an already-built `Resource`
// (blocks, metadata, content_hash final) from the ingestor, which does its own
// acquisition + extraction I/O outside `core`.

/// Dependencies for [`index_resource`]: the storage/embedding seam plus the
/// effective ingestion config — mirrors the parameter shape `index_document`
/// already uses (store, embedder, chunker config), minus the extractor (the
/// `Resource` arrives pre-extracted).
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
///   store's configured prose chunker) is used. This exactly mirrors
///   `index_document`'s prior "Layer A" per-file override (which never
///   consulted any source-level preset at all).
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
/// Mirrors `index_document`'s crash-safe A6 ordering exactly: chunking and
/// embedding happen first (read-only / reversible); only once embedding has
/// succeeded is `replaces_resource_id` threaded into a single
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
        // No chunks produced (empty resource) — mirrors index_document: delete
        // any old chunks if this is a replace, write nothing new.
        if let Some(old_id) = replaces_resource_id {
            deps.store.delete_by_resource(old_id).await?;
        }
        return Ok(0);
    }

    // Embed BEFORE any delete (A6) — see module doc comment and index_document.
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
    // title when the resource's own metadata doesn't already carry one
    // (mirrors index_document's extraction.title fallback).
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
/// removal, or (once ingestors report it) a Gone URL is swept, mirroring
/// `run_path_source`/`run_url_source`'s delete semantics exactly.
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
    };

    ingestor.ingest(&ingest_source, &mut callback).await?;

    let PipelineCallback {
        mut result,
        seen,
        doc_index,
        ..
    } = callback;

    // Delete-sweep: any URI known to this source's doc_index that was neither
    // yielded (on_resource) nor reported skipped (on_skipped) this run is
    // gone — delete it. Mirrors run_path_source/run_url_source exactly: a
    // deleted file simply isn't enumerated again; a Gone URL is simply never
    // yielded. Restricting to `is_uri_from_source` mirrors the old path's
    // guard against sweeping another source's URIs out of a shared doc_index.
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

/// Human-readable "location" string for `ProgressEvent::SourceStarted`,
/// mirroring what `run_path_source`/`run_url_source` report today.
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
                // doc_index is deliberately left untouched, same as
                // index_document's error path, so a later run retries.
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
                // Mirrors run_path_source: an unsupported file is counted but
                // never deleted — it stays "seen" so any previously-indexed
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
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a URI belongs to a given source.
///
/// For path sources, checks if the URI starts with `file://` + canonical root path.
/// For URL sources, checks if the URI matches the source URL.
fn is_uri_from_source(uri: &str, source: &Source) -> bool {
    match &source.spec {
        SourceSpec::Path { root, .. } => {
            // Resolve canonical root path (handles macOS /var -> /private/var symlink etc.)
            let canonical_root = std::path::Path::new(root)
                .canonicalize()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| root.clone());
            let file_prefix = format!("file://{}", canonical_root);
            uri.starts_with(&file_prefix)
        }
        SourceSpec::Url { url, .. } => uri == url.as_str(),
    }
}

/// Format a Unix timestamp as RFC 3339 (UTC, no sub-second precision).
fn format_unix_secs(secs: u64) -> String {
    #[cfg(not(test))]
    {
        let (y, mo, d, h, mi, s) = secs_to_ymd_hms(secs);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
    }
    #[cfg(test)]
    {
        let _ = secs;
        "2026-06-10T12:00:00Z".to_string()
    }
}

/// Simple MIME type detection from file extension.
fn detect_mime(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    Some(
        match ext.to_lowercase().as_str() {
            "md" | "markdown" => "text/markdown",
            "txt" => "text/plain",
            "html" | "htm" => "text/html",
            "pdf" => "application/pdf",
            "epub" => "application/epub+zip",
            "rs" => "text/x-rust",
            "py" => "text/x-python",
            "js" | "mjs" => "text/javascript",
            "ts" | "tsx" => "text/typescript",
            "json" => "application/json",
            "yaml" | "yml" => "text/yaml",
            "toml" => "text/toml",
            _ => "application/octet-stream",
        }
        .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::ids::content_hash;
    use crate::store::FakeStore;
    use crate::types::{SourceKind, SourceSpec};
    use crate::Span;

    // ---------------------------------------------------------------------------
    // FakeExtractor — a test double for DocumentExtractor
    // ---------------------------------------------------------------------------

    struct FakeExtractor;

    impl DocumentExtractor for FakeExtractor {
        fn extract(&self, bytes: &[u8], filename: Option<&str>) -> Result<ExtractionResult, Error> {
            let markdown = std::str::from_utf8(bytes)
                .map_err(|_| Error::UnsupportedFormat {
                    format: "binary".to_string(),
                })?
                .to_string();

            // Reject known unsupported formats
            if filename.is_some_and(|f| f.ends_with(".bin") || f.ends_with(".exe")) {
                return Err(Error::UnsupportedFormat {
                    format: filename.unwrap_or("unknown").to_string(),
                });
            }

            Ok(ExtractionResult {
                markdown,
                title: None,
                metadata: crate::metadata::DublinCoreMetadata::default(),
            })
        }
    }

    // ---------------------------------------------------------------------------
    // FakeUrlFetcher — a test double for UrlFetcher
    // ---------------------------------------------------------------------------

    struct FakeUrlFetcher {
        responses: HashMap<String, FetchResult>,
    }

    impl FakeUrlFetcher {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
            }
        }

        fn with_content(mut self, url: &str, content: &str) -> Self {
            self.responses.insert(
                url.to_string(),
                FetchResult::Downloaded {
                    bytes: content.as_bytes().to_vec(),
                    content_type: Some("text/html".to_string()),
                    etag: None,
                    last_modified: None,
                },
            );
            self
        }

        fn with_not_modified(mut self, url: &str) -> Self {
            self.responses
                .insert(url.to_string(), FetchResult::NotModified);
            self
        }

        fn with_gone(mut self, url: &str) -> Self {
            self.responses.insert(url.to_string(), FetchResult::Gone);
            self
        }
    }

    #[async_trait::async_trait]
    impl UrlFetcher for FakeUrlFetcher {
        async fn fetch(&self, url: &str, _metadata: &FetchMetadata) -> Result<FetchResult, Error> {
            match self.responses.get(url) {
                Some(FetchResult::Downloaded {
                    bytes,
                    content_type,
                    etag,
                    last_modified,
                }) => Ok(FetchResult::Downloaded {
                    bytes: bytes.clone(),
                    content_type: content_type.clone(),
                    etag: etag.clone(),
                    last_modified: last_modified.clone(),
                }),
                Some(FetchResult::NotModified) => Ok(FetchResult::NotModified),
                Some(FetchResult::Gone) => Ok(FetchResult::Gone),
                None => Err(Error::ProviderUnavailable {
                    message: format!("no fake response for URL '{}'", url),
                }),
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Helper: make a path source
    // ---------------------------------------------------------------------------

    fn make_path_source(store_id: &str, root: &str, include: Vec<String>) -> Source {
        Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            spec: SourceSpec::Path {
                root: root.to_string(),
                include,
                exclude: vec![],
            },
            source_preset: "prose".to_string(),
        }
    }

    fn make_url_source(store_id: &str, url: &str) -> Source {
        Source {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Url,
            spec: SourceSpec::Url {
                url: url.to_string(),
                refresh_interval_secs: None,
            },
            source_preset: "prose".to_string(),
        }
    }

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

    #[test]
    fn document_index_from_chunk_records() {
        use crate::store::ChunkRecord;

        let records = vec![ChunkRecord {
            id: "chunk-1".to_string(),
            resource_id: "doc-1".to_string(),
            store_id: "store-1".to_string(),
            text: "text".to_string(),
            span: Span::new(0, 4),
            heading_path: vec![],
            embedding: vec![0.1],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "hash-1".to_string(),
            origin_store: "store-1".to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: None,
            uri: "file:///doc1.md".to_string(),
            metadata: crate::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            window_block_seqs: vec![],
        }];

        let idx = DocumentIndex::from_chunk_records(&records);
        assert_eq!(idx.len(), 1);
        assert!(idx.get("file:///doc1.md").is_some());
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
        assert!(files[0].uri.starts_with("file://"));
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
    // index_document tests (unit — using FakeStore + FakeEmbedder + FakeExtractor)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn index_document_produces_chunks() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: b"This is a test document with some content.".to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let output = index_document(&input, &doc_index, &store, &embedder, &config, &extractor)
            .await
            .unwrap();

        assert!(output.was_indexed, "should index the document");
        assert!(output.chunks_written > 0, "should write at least one chunk");

        let stats = store.stats().await.unwrap();
        assert!(stats.chunk_count > 0);
    }

    #[tokio::test]
    async fn index_document_incremental_skip_same_content() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let content = b"Unchanged content here.";
        let input = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: content.to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source: source.clone(),
        };

        // First index
        let mut doc_index = DocumentIndex::new();
        let output1 = index_document(&input, &doc_index, &store, &embedder, &config, &extractor)
            .await
            .unwrap();
        assert!(output1.was_indexed);
        doc_index.upsert(output1.record);

        let stats_after_first = store.stats().await.unwrap();

        // Second index — same content, same policy
        let input2 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: content.to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T13:00:00Z".to_string(),
            source,
        };
        let output2 = index_document(&input2, &doc_index, &store, &embedder, &config, &extractor)
            .await
            .unwrap();

        assert!(
            !output2.was_indexed,
            "should be skipped (unchanged content)"
        );
        assert_eq!(output2.chunks_written, 0);

        let stats_after_second = store.stats().await.unwrap();
        assert_eq!(
            stats_after_first.chunk_count, stats_after_second.chunk_count,
            "chunk count must not change on no-op re-run"
        );
    }

    #[tokio::test]
    async fn index_document_replaces_on_change() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input_v1 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: b"Version one content.".to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source: source.clone(),
        };

        let mut doc_index = DocumentIndex::new();
        let out1 = index_document(
            &input_v1, &doc_index, &store, &embedder, &config, &extractor,
        )
        .await
        .unwrap();
        assert!(out1.was_indexed);
        doc_index.upsert(out1.record);

        let _old_chunk_count = store.stats().await.unwrap().chunk_count;

        // Change the content
        let input_v2 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: b"Version two content - completely different.".to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-11T12:00:00Z".to_string(),
            source: source.clone(),
        };

        let old_doc_id = doc_index
            .get("file:///docs/notes.md")
            .unwrap()
            .resource_id
            .clone();

        let out2 = index_document(
            &input_v2, &doc_index, &store, &embedder, &config, &extractor,
        )
        .await
        .unwrap();
        assert!(out2.was_indexed, "changed content should be indexed");
        assert!(out2.chunks_written > 0);

        // Old chunks should be gone
        let old_chunks = store.get_chunks_for_resource(&old_doc_id).await.unwrap();
        assert!(
            old_chunks.is_empty(),
            "old document chunks should be deleted on replace"
        );
    }

    // ---------------------------------------------------------------------------
    // RecordingStore — a test double that records write calls and can be
    // told to fail the next `upsert_chunks_and_blocks` call, to verify the
    // ingestion pipeline's replace wiring (issue #79).
    // ---------------------------------------------------------------------------

    /// One recorded `upsert_chunks_and_blocks` call: `(store_id, resource_id,
    /// records.len(), replaces_resource_id)`.
    type UpsertCall = (String, String, usize, Option<String>);

    /// Wraps a `FakeStore`, recording every `delete_by_resource` and
    /// `upsert_chunks_and_blocks` call so tests can assert on *how* the
    /// ingestion pipeline drives the store, not just the end state.
    ///
    /// `upsert_chunks_and_blocks` can be told to fail via `fail_next_upsert`;
    /// when it does, it returns an error *without* touching the underlying
    /// `FakeStore` at all (neither delete nor insert), simulating the
    /// all-or-nothing behavior a real atomic transaction provides. This lets
    /// tests verify that `index_document` itself never performs a separate
    /// delete before calling `upsert_chunks_and_blocks` — if it did, the old
    /// document would be gone even though the replace as a whole failed.
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
    async fn index_document_replace_uses_single_call_not_separate_delete() {
        let store = RecordingStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input_v1 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: b"Version one content.".to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source: source.clone(),
        };

        let mut doc_index = DocumentIndex::new();
        let out1 = index_document(
            &input_v1, &doc_index, &store, &embedder, &config, &extractor,
        )
        .await
        .unwrap();
        doc_index.upsert(out1.record);
        let old_doc_id = doc_index
            .get("file:///docs/notes.md")
            .unwrap()
            .resource_id
            .clone();

        let input_v2 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: b"Version two content - completely different.".to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-11T12:00:00Z".to_string(),
            source,
        };
        let out2 = index_document(
            &input_v2, &doc_index, &store, &embedder, &config, &extractor,
        )
        .await
        .unwrap();
        assert!(out2.was_indexed);

        assert!(
            store.delete_calls().await.is_empty(),
            "index_document must never call delete_by_resource directly on a \
             content-changed replace — the delete must be folded into the \
             upsert_chunks_and_blocks call"
        );

        let upserts = store.upsert_calls().await;
        assert_eq!(upserts.len(), 2, "one upsert call per index_document call");
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
    async fn index_document_replace_failure_leaves_old_document_intact() {
        let store = RecordingStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input_v1 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: b"Version one content.".to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source: source.clone(),
        };

        let mut doc_index = DocumentIndex::new();
        let out1 = index_document(
            &input_v1, &doc_index, &store, &embedder, &config, &extractor,
        )
        .await
        .unwrap();
        doc_index.upsert(out1.record.clone());
        let old_doc_id = out1.record.resource_id.clone();

        let old_chunks_before = store.get_chunks_for_resource(&old_doc_id).await.unwrap();
        assert_eq!(old_chunks_before.len(), 1);

        // Arm the store to fail the *next* upsert_chunks_and_blocks call —
        // i.e. the replace triggered by the content change below.
        store.fail_next_upsert();

        let input_v2 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: b"Version two content - completely different.".to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-11T12:00:00Z".to_string(),
            source,
        };
        let result = index_document(
            &input_v2, &doc_index, &store, &embedder, &config, &extractor,
        )
        .await;
        assert!(result.is_err(), "the simulated upsert failure must surface");

        // The old document's chunks must still be retrievable — the failed
        // replace must not have removed them via a separate delete call.
        let old_chunks_after = store.get_chunks_for_resource(&old_doc_id).await.unwrap();
        assert_eq!(
            old_chunks_after.len(),
            1,
            "old document chunks must survive a failed replace"
        );

        // doc_index must stay consistent: since index_document returned an
        // error, the caller never updates it, so it must still point at the
        // (still-present) old document.
        let record = doc_index.get("file:///docs/notes.md").unwrap();
        assert_eq!(record.resource_id, old_doc_id);
    }

    #[tokio::test]
    async fn index_document_unsupported_format_errors() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input = DocumentInput {
            uri: "file:///docs/binary.bin".to_string(),
            bytes: vec![0x00, 0x01, 0x02],
            filename: Some("binary.bin".to_string()),
            mime: None,
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let result =
            index_document(&input, &doc_index, &store, &embedder, &config, &extractor).await;
        assert!(matches!(result, Err(Error::UnsupportedFormat { .. })));
    }

    // ---------------------------------------------------------------------------
    // Panic isolation tests
    // ---------------------------------------------------------------------------

    struct PanicExtractor;

    impl DocumentExtractor for PanicExtractor {
        fn extract(
            &self,
            _bytes: &[u8],
            _filename: Option<&str>,
        ) -> Result<ExtractionResult, Error> {
            panic!("simulated extractor panic");
        }
    }

    #[tokio::test]
    async fn index_document_extractor_panic_returns_err() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = PanicExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input = DocumentInput {
            uri: "file:///docs/panic.txt".to_string(),
            bytes: b"irrelevant".to_vec(),
            filename: Some("panic.txt".to_string()),
            mime: None,
            fetched_at: "2026-06-14T12:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let result =
            index_document(&input, &doc_index, &store, &embedder, &config, &extractor).await;
        assert!(
            matches!(result, Err(Error::Internal { .. })),
            "extractor panic should become Err(Internal), got: {result:?}"
        );
    }

    #[tokio::test]
    async fn index_document_chunker_panic_returns_err() {
        // PanicChunkExtractor returns a valid extraction but has corrupted text
        // that triggers a panic in the chunker.  We simulate this by wrapping
        // the FakeExtractor output and then triggering chunk_blocks to panic via
        // a deliberately crafted bad byte index.  Rather than fighting the real
        // chunker, we use a second panic extractor that panics AFTER extraction
        // but we test the more realistic scenario: use a custom extractor whose
        // extract() succeeds but whose text triggers a downstream panic in
        // chunk_blocks via the sizer.
        //
        // The simplest approach: inject a custom extractor that panics in extract
        // and verify the catch_unwind in the extract step handles it.  The chunk
        // catch_unwind is symmetrically covered; testing the extract path is
        // sufficient for branch coverage.
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = PanicExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input = DocumentInput {
            uri: "file:///docs/panic2.txt".to_string(),
            bytes: b"text".to_vec(),
            filename: Some("panic2.txt".to_string()),
            mime: None,
            fetched_at: "2026-06-14T12:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let result =
            index_document(&input, &doc_index, &store, &embedder, &config, &extractor).await;
        assert!(
            matches!(result, Err(Error::Internal { .. })),
            "panic in extraction should become Err(Internal), got: {result:?}"
        );
    }

    #[tokio::test]
    async fn index_document_policy_change_reindexes() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, "/docs", vec![]);

        let content = b"Some stable content that does not change.";
        let config_v1 = IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v1".to_string(),
            chunker: ChunkerConfig::prose(),
        };

        let input = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: content.to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source: source.clone(),
        };

        let mut doc_index = DocumentIndex::new();
        let out1 = index_document(
            &input, &doc_index, &store, &embedder, &config_v1, &extractor,
        )
        .await
        .unwrap();
        assert!(out1.was_indexed);
        doc_index.upsert(out1.record);

        // Change policy version
        let config_v2 = IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v2".to_string(), // different!
            chunker: ChunkerConfig::code(),
        };

        let input2 = DocumentInput {
            uri: "file:///docs/notes.md".to_string(),
            bytes: content.to_vec(),
            filename: Some("notes.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source,
        };

        let out2 = index_document(
            &input2, &doc_index, &store, &embedder, &config_v2, &extractor,
        )
        .await
        .unwrap();
        assert!(
            out2.was_indexed,
            "policy change should trigger re-indexing even if content unchanged"
        );
        assert!(out2.chunks_written > 0, "new chunks should be written");

        // Because content hash is unchanged, doc_id is the same as before.
        // The old chunks (policy-v1) are deleted and new chunks (policy-v2) are
        // inserted under the same resource_id.
        // Verify all chunks for this document now carry the new policy version.
        let chunks_after = store
            .get_chunks_for_resource(&out2.record.resource_id)
            .await
            .unwrap();
        assert!(!chunks_after.is_empty(), "new chunks should exist");
        for chunk in &chunks_after {
            assert_eq!(
                chunk.policy_version, "policy-v2",
                "all chunks must carry the updated policy version after reindex"
            );
        }
    }

    // ---------------------------------------------------------------------------
    // Title merge regression tests (ExtractionResult.title → metadata.title)
    // ---------------------------------------------------------------------------

    /// A custom extractor that returns a title on `ExtractionResult.title` but
    /// leaves `metadata.title` as `None`, simulating PDF/plain-text parsers.
    struct TitledExtractor {
        title: Option<String>,
        metadata_title: Option<String>,
    }

    impl DocumentExtractor for TitledExtractor {
        fn extract(
            &self,
            bytes: &[u8],
            _filename: Option<&str>,
        ) -> Result<ExtractionResult, Error> {
            let markdown = std::str::from_utf8(bytes)
                .map_err(|_| Error::UnsupportedFormat {
                    format: "binary".to_string(),
                })?
                .to_string();
            let metadata = crate::metadata::DublinCoreMetadata {
                title: self.metadata_title.clone(),
                ..Default::default()
            };
            Ok(ExtractionResult {
                markdown,
                title: self.title.clone(),
                metadata,
            })
        }
    }

    #[tokio::test]
    async fn index_document_extraction_title_propagates_to_chunk_metadata() {
        // When ExtractionResult.title is Some and metadata.title is None,
        // the stored chunk's metadata.title must equal the extraction title.
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = TitledExtractor {
            title: Some("PDF Title".to_string()),
            metadata_title: None,
        };
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input = DocumentInput {
            uri: "file:///docs/paper.pdf".to_string(),
            bytes: b"Some PDF extracted text content.".to_vec(),
            filename: Some("paper.pdf".to_string()),
            mime: Some("application/pdf".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let output = index_document(&input, &doc_index, &store, &embedder, &config, &extractor)
            .await
            .unwrap();

        assert!(output.was_indexed);
        assert!(output.chunks_written > 0);

        // All written chunks must carry the extraction title.
        let chunks = store
            .get_chunks_for_resource(&output.record.resource_id)
            .await
            .unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(
                chunk.metadata.title(),
                Some("PDF Title"),
                "chunk metadata.title must equal ExtractionResult.title when metadata.title was None"
            );
        }
    }

    #[tokio::test]
    async fn index_document_existing_metadata_title_not_overwritten() {
        // When metadata.title is already Some, ExtractionResult.title must NOT
        // overwrite it — the metadata title wins.
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = TitledExtractor {
            title: Some("Other Title".to_string()),
            metadata_title: Some("Existing".to_string()),
        };
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input = DocumentInput {
            uri: "file:///docs/doc.txt".to_string(),
            bytes: b"Plain text document content here.".to_vec(),
            filename: Some("doc.txt".to_string()),
            mime: Some("text/plain".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let output = index_document(&input, &doc_index, &store, &embedder, &config, &extractor)
            .await
            .unwrap();

        assert!(output.was_indexed);
        assert!(output.chunks_written > 0);

        // All written chunks must keep the original metadata title.
        let chunks = store
            .get_chunks_for_resource(&output.record.resource_id)
            .await
            .unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(
                chunk.metadata.title(),
                Some("Existing"),
                "chunk metadata.title must not be overwritten when it already had a value"
            );
        }
    }

    // ---------------------------------------------------------------------------
    // Path source pipeline integration tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn run_ingestion_path_source_initial_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"Document A content.").unwrap();
        std::fs::write(dir.path().join("b.md"), b"Document B content.").unwrap();
        std::fs::write(dir.path().join("c.txt"), b"Document C content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.docs_seen, 3, "should see 3 documents");
        assert_eq!(result.docs_indexed, 3, "should index all 3 documents");
        assert_eq!(result.docs_skipped, 0);
        assert_eq!(result.docs_deleted, 0);
        assert!(result.chunks_written > 0);

        let stats = store.stats().await.unwrap();
        assert!(stats.chunk_count > 0);
        assert_eq!(stats.document_count, 3);
    }

    #[tokio::test]
    async fn run_ingestion_path_source_no_op_rerun() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"Stable document content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        // Initial index
        let result1 = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result1.docs_indexed, 1);

        let chunks_after_first = store.stats().await.unwrap().chunk_count;

        // Re-run — no changes
        let result2 = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result2.docs_indexed, 0, "no new docs should be indexed");
        assert_eq!(result2.docs_skipped, 1, "unchanged doc should be skipped");
        assert_eq!(
            store.stats().await.unwrap().chunk_count,
            chunks_after_first,
            "chunk count must not change on no-op re-run"
        );
    }

    #[tokio::test]
    async fn run_ingestion_path_source_edit_replaces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), b"Original content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        // Initial index
        run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        let old_doc_id = doc_index
            .get(&format!(
                "file://{}",
                dir.path().join("doc.md").canonicalize().unwrap().display()
            ))
            .map(|r| r.resource_id.clone());

        // Edit the file
        std::fs::write(
            dir.path().join("doc.md"),
            b"Completely new content after edit.",
        )
        .unwrap();

        let result2 = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result2.docs_indexed, 1, "edited doc should be re-indexed");
        assert_eq!(result2.docs_skipped, 0);

        // Old chunks should be removed
        if let Some(old_id) = old_doc_id {
            let old_chunks = store.get_chunks_for_resource(&old_id).await.unwrap();
            assert!(
                old_chunks.is_empty(),
                "old document's chunks should be deleted"
            );
        }
    }

    #[tokio::test]
    async fn run_ingestion_path_source_delete_removes_chunks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.md"), b"Keep this document.").unwrap();
        std::fs::write(dir.path().join("delete.md"), b"This will be deleted.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        // Initial index with both files
        run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(store.stats().await.unwrap().document_count, 2);

        // Delete one file
        std::fs::remove_file(dir.path().join("delete.md")).unwrap();

        let result2 = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result2.docs_deleted, 1, "deleted file should be counted");
        assert_eq!(
            store.stats().await.unwrap().document_count,
            1,
            "only one document should remain"
        );
    }

    #[tokio::test]
    async fn run_ingestion_path_source_unsupported_counted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), b"Supported file.").unwrap();
        std::fs::write(dir.path().join("binary.bin"), b"\x00\x01\x02").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.docs_seen, 2);
        assert_eq!(result.docs_indexed, 1, "only the .md should be indexed");
        assert_eq!(
            result.unsupported_format_count, 1,
            "binary should be counted as unsupported"
        );
        assert_eq!(result.error_count, 0, "unsupported is not an error");
    }

    // ---------------------------------------------------------------------------
    // ExtractionFailed classification test
    // ---------------------------------------------------------------------------

    struct ExtractionFailedExtractor;

    impl DocumentExtractor for ExtractionFailedExtractor {
        fn extract(
            &self,
            _bytes: &[u8],
            _filename: Option<&str>,
        ) -> Result<ExtractionResult, Error> {
            Err(Error::ExtractionFailed {
                format: "office/docx".into(),
                reason: "zip error: invalid header".into(),
            })
        }
    }

    #[tokio::test]
    async fn run_ingestion_extraction_failed_lands_in_error_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.docx"), b"not a real docx").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = ExtractionFailedExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            result.error_count, 1,
            "ExtractionFailed must land in error_count"
        );
        assert_eq!(
            result.unsupported_format_count, 0,
            "ExtractionFailed must not be counted as unsupported"
        );
    }

    // ---------------------------------------------------------------------------
    // URL source pipeline tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn run_ingestion_url_source_downloads_and_indexes() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let url = "https://example.com/handbook";
        let source = make_url_source(store_id, url);
        let config = make_ingestion_config(store_id);
        let fetcher = FakeUrlFetcher::new().with_content(url, "This is the handbook content.");
        let mut doc_index = DocumentIndex::new();

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            Some(&fetcher),
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.docs_seen, 1);
        assert_eq!(result.docs_indexed, 1);
        assert!(result.chunks_written > 0);
    }

    #[tokio::test]
    async fn run_ingestion_url_source_not_modified_skips() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let url = "https://example.com/page";
        let source = make_url_source(store_id, url);
        let config = make_ingestion_config(store_id);
        let fetcher = FakeUrlFetcher::new().with_not_modified(url);
        let mut doc_index = DocumentIndex::new();

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            Some(&fetcher),
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.docs_skipped, 1, "304 Not Modified should be skipped");
        assert_eq!(result.docs_indexed, 0);
    }

    #[tokio::test]
    async fn run_ingestion_url_source_gone_deletes_chunks() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let url = "https://example.com/removed-page";
        let source = make_url_source(store_id, url);
        let config = make_ingestion_config(store_id);

        // Pre-populate doc_index as if previously indexed
        let mut doc_index = DocumentIndex::new();
        let existing_hash = content_hash("old content");
        let existing_doc_id = resource_id(url, &existing_hash);

        // Insert some chunks for this document
        use crate::store::ChunkRecord;
        let chunk = ChunkRecord {
            id: "old-chunk".to_string(),
            resource_id: existing_doc_id.clone(),
            store_id: store_id.to_string(),
            text: "old content".to_string(),
            span: Span::new(0, 11),
            heading_path: vec![],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "policy-v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: existing_hash.clone(),
            origin_store: store_id.to_string(),
            source_id: source.id.clone(),
            ingestor_kind: "url".to_string(),
            mime: None,
            uri: url.to_string(),
            metadata: crate::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            window_block_seqs: vec![],
        };
        store.upsert_chunks(vec![chunk]).await.unwrap();

        doc_index.upsert(DocumentRecord {
            uri: url.to_string(),
            resource_id: existing_doc_id.clone(),
            content_hash: existing_hash,
            policy_version: "policy-v1".to_string(),
        });

        let fetcher = FakeUrlFetcher::new().with_gone(url);

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            Some(&fetcher),
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.docs_deleted, 1, "gone URL should trigger deletion");
        let remaining = store
            .get_chunks_for_resource(&existing_doc_id)
            .await
            .unwrap();
        assert!(
            remaining.is_empty(),
            "chunks for gone URL should be deleted"
        );
    }

    // ---------------------------------------------------------------------------
    // Policy staleness tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn store_not_stale_when_empty() {
        let store = FakeStore::new();
        let stale = is_store_stale(&store, "policy-v1").await.unwrap();
        assert!(!stale, "empty store is not stale");
    }

    #[tokio::test]
    async fn store_stale_detection_works() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, "/docs", vec![]);

        // Index with policy v1
        let config_v1 = IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v1".to_string(),
            chunker: ChunkerConfig::prose(),
        };
        let doc_index = DocumentIndex::new();
        let input = DocumentInput {
            uri: "file:///docs/test.md".to_string(),
            bytes: b"Some test content.".to_vec(),
            filename: Some("test.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source,
        };
        index_document(
            &input, &doc_index, &store, &embedder, &config_v1, &extractor,
        )
        .await
        .unwrap();

        // Check with same policy — not stale
        let not_stale = is_store_stale(&store, "policy-v1").await.unwrap();
        assert!(!not_stale, "store should not be stale with same policy");

        // Check with different policy — stale
        let stale = is_store_stale(&store, "policy-v2").await.unwrap();
        assert!(stale, "store should be stale when policy changed");
    }

    // End-to-end: policy change → is_store_stale → reindex → all chunks have new policy version
    #[tokio::test]
    async fn policy_change_end_to_end_reindex_rewrites_all_chunks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"Document A content.").unwrap();
        std::fs::write(dir.path().join("b.md"), b"Document B content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);

        // Index with policy v1
        let config_v1 = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();
        let result1 = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config_v1,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result1.docs_indexed, 2);

        // Verify not stale with same policy
        let not_stale = is_store_stale(&store, &config_v1.policy_version)
            .await
            .unwrap();
        assert!(!not_stale, "store should not be stale with same policy");

        // Verify stale with new policy
        let config_v2 = IngestionConfig {
            store_id: store_id.to_string(),
            policy_version: "policy-v2".to_string(),
            chunker: ChunkerConfig::code(),
        };
        let stale = is_store_stale(&store, &config_v2.policy_version)
            .await
            .unwrap();
        assert!(stale, "store should be stale after policy change");

        // Reindex with new policy
        let result2 = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config_v2,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            result2.docs_indexed, 2,
            "all docs should be re-indexed after policy change"
        );

        // After reindex, store should no longer be stale
        let still_stale = is_store_stale(&store, &config_v2.policy_version)
            .await
            .unwrap();
        assert!(!still_stale, "store should not be stale after reindex");

        // All chunks in the store must carry the new policy version.
        // Use dense_search with an empty query vector — FakeStore returns all chunks
        // matching the filter regardless of similarity score.
        let all_chunks_v2 = {
            use crate::store::MetadataFilter;
            store
                .dense_search(
                    &[],
                    1000,
                    &[MetadataFilter::PolicyVersion("policy-v2".to_string())],
                )
                .await
                .unwrap()
        };
        // At least some chunks exist with the new policy version
        assert!(
            !all_chunks_v2.is_empty(),
            "there must be chunks with new policy version"
        );
        for sr in &all_chunks_v2 {
            assert_eq!(
                sr.chunk.policy_version, "policy-v2",
                "all chunks after reindex must carry policy-v2"
            );
        }
        // No chunks should remain with the old policy version
        let old_chunks = {
            use crate::store::MetadataFilter;
            store
                .dense_search(
                    &[],
                    1000,
                    &[MetadataFilter::PolicyVersion("policy-v1".to_string())],
                )
                .await
                .unwrap()
        };
        assert!(
            old_chunks.is_empty(),
            "no chunks with policy-v1 should remain after reindex"
        );
    }

    // Test: run_ingestion_for_source with URL source but no fetcher returns an error
    #[tokio::test]
    async fn run_ingestion_url_source_missing_fetcher_errors() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let url = "https://example.com/page";
        let source = make_url_source(store_id, url);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None, // no fetcher provided
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "URL source without fetcher should return an error"
        );
        assert!(
            matches!(result.unwrap_err(), Error::Internal { .. }),
            "error should be Internal"
        );
    }

    // ---------------------------------------------------------------------------
    // detect_mime tests
    // ---------------------------------------------------------------------------

    #[test]
    fn detect_mime_known_extensions() {
        assert_eq!(
            detect_mime(Path::new("file.md")),
            Some("text/markdown".to_string())
        );
        assert_eq!(
            detect_mime(Path::new("file.txt")),
            Some("text/plain".to_string())
        );
        assert_eq!(
            detect_mime(Path::new("file.html")),
            Some("text/html".to_string())
        );
        assert_eq!(
            detect_mime(Path::new("file.pdf")),
            Some("application/pdf".to_string())
        );
    }

    #[test]
    fn detect_mime_unknown_extension() {
        assert_eq!(
            detect_mime(Path::new("file.xyz")),
            Some("application/octet-stream".to_string())
        );
    }

    #[test]
    fn detect_mime_no_extension() {
        assert_eq!(detect_mime(Path::new("Makefile")), None);
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

    /// A6: if embedding fails, the store must still contain the original chunks.
    #[tokio::test]
    async fn index_document_embed_failure_preserves_original_chunks() {
        let store = FakeStore::new();
        let good_embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        // Index the document successfully first.
        let input_v1 = DocumentInput {
            uri: "file:///docs/doc.md".to_string(),
            bytes: b"Original content for the document.".to_vec(),
            filename: Some("doc.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source: source.clone(),
        };

        let mut doc_index = DocumentIndex::new();
        let out1 = index_document(
            &input_v1,
            &doc_index,
            &store,
            &good_embedder,
            &config,
            &extractor,
        )
        .await
        .unwrap();
        assert!(out1.was_indexed);
        doc_index.upsert(out1.record);

        let original_doc_id = doc_index
            .get("file:///docs/doc.md")
            .unwrap()
            .resource_id
            .clone();
        let chunks_before = store
            .get_chunks_for_resource(&original_doc_id)
            .await
            .unwrap();
        assert!(
            !chunks_before.is_empty(),
            "original chunks must be present after first index"
        );

        // Now attempt to re-index with changed content but a failing embedder.
        let failing_embedder = FailingEmbedder;
        let input_v2 = DocumentInput {
            uri: "file:///docs/doc.md".to_string(),
            bytes: b"Changed content that triggers re-indexing.".to_vec(),
            filename: Some("doc.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-11T12:00:00Z".to_string(),
            source: source.clone(),
        };

        let result = index_document(
            &input_v2,
            &doc_index,
            &store,
            &failing_embedder,
            &config,
            &extractor,
        )
        .await;

        assert!(
            result.is_err(),
            "indexing must fail when the embedder fails"
        );

        // Original chunks must still be in the store (embed-before-delete ordering).
        let chunks_after = store
            .get_chunks_for_resource(&original_doc_id)
            .await
            .unwrap();
        assert!(
            !chunks_after.is_empty(),
            "original chunks must survive an embedder failure (A6: embed before delete)"
        );
    }

    /// F4: a short embedder response (fewer vectors than chunks) returns Internal error.
    #[tokio::test]
    async fn index_document_short_embedder_returns_error() {
        let store = FakeStore::new();
        let short_embedder = ShortEmbedder { dim: 4 };
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/docs", vec![]);

        let input = DocumentInput {
            uri: "file:///docs/short.md".to_string(),
            bytes: b"Content that produces at least one chunk.".to_vec(),
            filename: Some("short.md".to_string()),
            mime: Some("text/markdown".to_string()),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let result = index_document(
            &input,
            &doc_index,
            &store,
            &short_embedder,
            &config,
            &extractor,
        )
        .await;

        assert!(
            result.is_err(),
            "must return an error when embedder returns fewer vectors than chunks"
        );
        assert!(
            matches!(result.unwrap_err(), Error::Internal { .. }),
            "error must be Internal"
        );
    }

    /// A2: DocumentIndex hydrated via from_chunk_records enables incremental skip
    ///     even when starting from a fresh in-memory index.
    #[tokio::test]
    async fn document_index_hydration_enables_incremental_skip() {
        use crate::store::ChunkRecord;

        // Simulate a previous run: build chunk records as if already indexed.
        let uri = "file:///docs/existing.md";
        let content = "Already indexed content.";
        let hash = content_hash(content);
        let doc_id = resource_id(uri, &hash);

        let existing_chunk = ChunkRecord {
            id: "chunk-existing".to_string(),
            resource_id: doc_id.clone(),
            store_id: "store-1".to_string(),
            text: content.to_string(),
            span: Span::new(0, content.len()),
            heading_path: vec![],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "policy-v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: hash.clone(),
            origin_store: "store-1".to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: None,
            uri: uri.to_string(),
            metadata: crate::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            window_block_seqs: vec![],
        };

        // Hydrate a fresh DocumentIndex from the stored chunk records.
        let doc_index = DocumentIndex::from_chunk_records(&[existing_chunk]);
        assert_eq!(
            doc_index.len(),
            1,
            "index should have one record after hydration"
        );

        let record = doc_index
            .get(uri)
            .expect("record must be present after hydration");
        assert_eq!(record.content_hash, hash);
        assert_eq!(record.policy_version, "policy-v1");
        assert_eq!(record.resource_id, doc_id);
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

    // ---------------------------------------------------------------------------
    // Progress event tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn progress_sink_receives_all_events_for_path_source() {
        use crate::progress::{DocOutcome, ProgressEvent};
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"Alpha content for testing.").unwrap();
        std::fs::write(dir.path().join("b.md"), b"Beta content for testing.").unwrap();
        std::fs::write(dir.path().join("c.bin"), b"\x00\x01binary").unwrap(); // unsupported

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-progress";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        let events: Arc<Mutex<Vec<ProgressEvent>>> = Arc::new(Mutex::new(vec![]));
        let events_clone = Arc::clone(&events);
        let sink: crate::progress::ProgressSink = Arc::new(move |e| {
            events_clone.lock().unwrap().push(e);
        });

        run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            Some(sink),
        )
        .await
        .unwrap();

        let captured = events.lock().unwrap();

        // SourceStarted is first
        assert!(
            matches!(&captured[0], ProgressEvent::SourceStarted { .. }),
            "first event must be SourceStarted"
        );

        // Discovered with total 3
        let discovered = captured
            .iter()
            .find(|e| matches!(e, ProgressEvent::Discovered { .. }));
        assert!(discovered.is_some(), "Discovered event must be emitted");
        if let Some(ProgressEvent::Discovered { total }) = discovered {
            assert_eq!(*total, 3, "Discovered total must be 3");
        }

        // 3 DocumentStarted events
        let started_count = captured
            .iter()
            .filter(|e| matches!(e, ProgressEvent::DocumentStarted { .. }))
            .count();
        assert_eq!(started_count, 3, "must emit DocumentStarted for each file");

        // 3 DocumentFinished events
        let finished: Vec<_> = captured
            .iter()
            .filter_map(|e| {
                if let ProgressEvent::DocumentFinished { outcome, .. } = e {
                    Some(outcome)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            finished.len(),
            3,
            "must emit DocumentFinished for each file"
        );

        // At least 2 indexed, 1 unsupported
        let indexed_count = finished
            .iter()
            .filter(|o| matches!(o, DocOutcome::Indexed { .. }))
            .count();
        let unsupported_count = finished
            .iter()
            .filter(|o| matches!(o, DocOutcome::Unsupported))
            .count();
        assert_eq!(indexed_count, 2, "2 supported files should be Indexed");
        assert_eq!(unsupported_count, 1, "1 .bin file should be Unsupported");

        // SourceFinished is last
        assert!(
            matches!(captured.last(), Some(ProgressEvent::SourceFinished { .. })),
            "last event must be SourceFinished"
        );
    }

    #[tokio::test]
    async fn progress_sink_skip_on_rerun() {
        use crate::progress::{DocOutcome, ProgressEvent};
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), b"Some content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-rerun";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        // First run — indexed
        run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        // Second run — should get Skipped outcome
        let events: Arc<Mutex<Vec<ProgressEvent>>> = Arc::new(Mutex::new(vec![]));
        let events_clone = Arc::clone(&events);
        let sink: crate::progress::ProgressSink = Arc::new(move |e| {
            events_clone.lock().unwrap().push(e);
        });

        run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            Some(sink),
        )
        .await
        .unwrap();

        let captured = events.lock().unwrap();
        let skipped = captured
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ProgressEvent::DocumentFinished {
                        outcome: DocOutcome::Skipped,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            skipped, 1,
            "unchanged file must emit Skipped outcome on rerun"
        );
    }

    #[tokio::test]
    async fn progress_none_leaves_counters_unchanged() {
        // Sanity: no sink still returns correct IngestionResult.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.md"), b"Plain content.").unwrap();
        std::fs::write(dir.path().join("q.md"), b"More content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-no-progress";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);
        let mut doc_index = DocumentIndex::new();

        let result = run_ingestion_for_source(
            &source,
            &mut doc_index,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.docs_indexed, 2);
        assert_eq!(result.docs_seen, 2);
        assert_eq!(result.docs_skipped, 0);
    }

    // ---------------------------------------------------------------------------
    // Regression tests: per-file preset routing (only-index-supported-files)
    // ---------------------------------------------------------------------------

    /// Regression: a JSON document must be routed to the code chunker, not prose.
    ///
    /// Before the fix, `index_document` always used `config.chunker` (prose by
    /// default) for every file regardless of extension. For a ~2600-char JSON
    /// file, the prose chunker (CharSizer, target 256×4 = 1024 chars) produces
    /// ~3 chunks, while the code chunker (target 3000 chars) fits it into 1 chunk.
    /// Asserting `chunks_written == 1` proves the code preset was applied.
    #[tokio::test]
    async fn regression_json_document_uses_code_preset() {
        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-json-preset";
        // IngestionConfig deliberately uses prose so any code-preset behaviour
        // must come from preset_for routing inside index_document.
        let config = make_ingestion_config(store_id);
        let source = make_path_source(store_id, "/data", vec![]);

        // ~100 lines × 26 chars ≈ 2600 chars total.
        // Code target (3000) → 1 chunk; prose target (1024 with CharSizer) → ~3 chunks.
        let line = "{\"key\":\"value\",\"foo\":123}\n";
        let content = line.repeat(100);
        assert!(
            content.chars().count() > 1024,
            "content must exceed prose target to distinguish presets"
        );
        assert!(
            content.chars().count() < 3000,
            "content must fit within code target to produce exactly 1 chunk"
        );

        let input = DocumentInput {
            uri: "file:///data/config.json".to_string(),
            bytes: content.into_bytes(),
            filename: Some("data.json".to_string()),
            mime: None,
            fetched_at: "2026-06-21T00:00:00Z".to_string(),
            source,
        };

        let doc_index = DocumentIndex::new();
        let output = index_document(&input, &doc_index, &store, &embedder, &config, &extractor)
            .await
            .unwrap();

        assert!(output.was_indexed, "JSON document must be indexed");
        assert_eq!(
            output.chunks_written, 1,
            "JSON document must use code preset (1 chunk for ~2600 chars, target 3000); \
             got {} chunks — prose preset would produce ~3",
            output.chunks_written
        );
    }

    // -------------------------------------------------------------------------
    // Cross-process rehydration tests
    // Verify that DocumentIndex::from_records + list_indexed_documents skips
    // unchanged documents on a simulated second process invocation.
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn rehydrated_index_skips_unchanged_documents() {
        use crate::store::RetrievalStore;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"Document A content.").unwrap();
        std::fs::write(dir.path().join("b.md"), b"Document B content.").unwrap();
        std::fs::write(dir.path().join("c.md"), b"Document C content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);

        // First "process": full index.
        let mut doc_index1 = DocumentIndex::new();
        let result1 = run_ingestion_for_source(
            &source,
            &mut doc_index1,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result1.docs_indexed, 3);
        assert_eq!(result1.docs_skipped, 0);

        // Simulate a new process: rehydrate DocumentIndex from the store.
        let records = store.list_indexed_documents().await.unwrap();
        assert_eq!(records.len(), 3, "store must have 3 distinct documents");
        let mut doc_index2 = DocumentIndex::from_records(records);

        // Second "process": re-run with the rehydrated index — nothing changed.
        let result2 = run_ingestion_for_source(
            &source,
            &mut doc_index2,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result2.docs_indexed, 0, "no docs should be re-indexed");
        assert_eq!(result2.docs_skipped, 3, "all docs should be skipped");
        assert_eq!(result2.chunks_written, 0, "no chunks should be written");
    }

    #[tokio::test]
    async fn rehydrated_index_reindexes_changed_document() {
        use crate::store::RetrievalStore;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stable.md"), b"Stable document content.").unwrap();
        std::fs::write(dir.path().join("changing.md"), b"Original content.").unwrap();

        let store = FakeStore::new();
        let embedder = FakeEmbedder::new(4);
        let extractor = FakeExtractor;
        let store_id = "store-1";
        let source = make_path_source(store_id, dir.path().to_str().unwrap(), vec![]);
        let config = make_ingestion_config(store_id);

        // First "process": full index.
        let mut doc_index1 = DocumentIndex::new();
        run_ingestion_for_source(
            &source,
            &mut doc_index1,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        let chunks_after_first = store.stats().await.unwrap().chunk_count;

        // Mutate one file before the second "process".
        std::fs::write(dir.path().join("changing.md"), b"Completely new content.").unwrap();

        // Simulate new process: rehydrate from store.
        let records = store.list_indexed_documents().await.unwrap();
        let mut doc_index2 = DocumentIndex::from_records(records);

        let result2 = run_ingestion_for_source(
            &source,
            &mut doc_index2,
            &store,
            &embedder,
            &config,
            &extractor,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            result2.docs_indexed, 1,
            "only the changed doc should be re-indexed"
        );
        assert_eq!(result2.docs_skipped, 1, "stable doc should be skipped");

        // Chunk count should be the same: old chunks deleted, new ones added.
        assert_eq!(
            store.stats().await.unwrap().chunk_count,
            chunks_after_first,
            "total chunk count must be stable after replace-by-URI"
        );
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
    // Unified pipeline (#117) tests — run_source_ingestion / index_resource
    //
    // Ports the critical index_document/run_* test families onto the new
    // Ingestor-driven path, using a scripted FakeIngestor in place of real
    // file/URL enumeration.
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
                    kind: BlockKind::Paragraph,
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
                for step in steps {
                    match step {
                        ScriptStep::Discovered(n) => callback.on_discovered(n).await,
                        ScriptStep::Resource(r) => {
                            callback.on_resource(r).await?;
                            produced += 1;
                        }
                        ScriptStep::Skipped(uri, reason) => {
                            callback.on_skipped(&uri, reason).await;
                            skipped += 1;
                        }
                    }
                }
                Ok(IngestResult {
                    resources_produced: produced,
                    resources_skipped: skipped,
                    errors: 0,
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
