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

use crate::chunker::{chunk_document, ChunkerConfig};
use crate::embedder::{DocumentChunks, Embedder};
use crate::error::Error;
use crate::ids::{content_hash, document_id, new_ulid};
use crate::store::{ChunkRecord, RetrievalStore};
use crate::types::{
    Block, Chunk, IndexJob, IndexJobScope, IndexJobState, IndexJobStats, Provenance, Source,
    SourceKind, SourceRef, SourceSpec,
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
    pub document_id: String,
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
    /// Each unique (uri, document_id, content_hash, policy_version) combination
    /// in the store becomes one record.
    pub fn from_chunk_records(chunks: &[ChunkRecord]) -> Self {
        let mut records = HashMap::new();
        for chunk in chunks {
            records.entry(chunk.uri.clone()).or_insert(DocumentRecord {
                uri: chunk.uri.clone(),
                document_id: chunk.document_id.clone(),
                content_hash: chunk.content_hash.clone(),
                policy_version: chunk.policy_version.clone(),
            });
        }
        Self { records }
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
/// Returns `true` if any existing chunk in the store was indexed with a
/// different policy version. Callers should trigger a full reindex when this is true.
pub async fn is_store_stale(
    store: &dyn RetrievalStore,
    current_policy_version: &str,
) -> Result<bool, Error> {
    let stats = store.stats().await?;
    if stats.chunk_count == 0 {
        return Ok(false);
    }

    // Get a sample chunk to check its policy version
    // We use dense_search with an empty query to get any chunk
    let results = store.dense_search(&[], 1, &[]).await?;
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

    let mut found = Vec::new();
    enumerate_dir(root_path, root_path, include, exclude, &mut found)?;
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

/// Recursively enumerate a directory.
fn enumerate_dir(
    root: &Path,
    dir: &Path,
    include: &[String],
    exclude: &[String],
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

        // Apply exclude globs first
        if !exclude.is_empty() && matches_any_glob(&relative_str, exclude) {
            continue;
        }

        if path.is_dir() {
            enumerate_dir(root, &path, include, exclude, found)?;
        } else if path.is_file() {
            // Apply include globs: if any are specified, file must match one
            if !include.is_empty() && !matches_any_glob(&relative_str, include) {
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

/// Check if a path matches any of the given glob patterns.
fn matches_any_glob(path: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| glob_match(g, path))
}

/// Simple glob matching supporting `*`, `**`, and `?`.
///
/// Uses a recursive descent implementation.
fn glob_match(pattern: &str, path: &str) -> bool {
    glob_match_parts(pattern, path)
}

fn glob_match_parts(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }

    // Handle ** — matches any path segment(s) including path separators
    if let Some(rest) = pattern.strip_prefix("**/") {
        // Try matching with zero or more path components consumed
        if glob_match_parts(rest, path) {
            return true;
        }
        // Try consuming one path component
        if let Some(pos) = path.find('/') {
            return glob_match_parts(pattern, &path[pos + 1..]);
        }
        return false;
    }

    if pattern == "**" {
        return true; // matches everything remaining
    }

    // Handle * — matches anything except /
    if let Some(rest) = pattern.strip_prefix('*') {
        // Try at each position
        for i in 0..=path.len() {
            if i > 0 && path.as_bytes()[i - 1] == b'/' {
                break; // * doesn't cross directory boundaries
            }
            if glob_match_parts(rest, &path[i..]) {
                return true;
            }
        }
        return false;
    }

    // Handle ? — matches any single character except /
    if let Some(rest) = pattern.strip_prefix('?') {
        if path.is_empty() || path.starts_with('/') {
            return false;
        }
        let first_char_len = path.chars().next().map(|c| c.len_utf8()).unwrap_or(0);
        return glob_match_parts(rest, &path[first_char_len..]);
    }

    // Literal character
    let p = pattern.chars().next().unwrap();
    let c = path.chars().next();
    if c == Some(p) {
        let pat_next = &pattern[p.len_utf8()..];
        let path_next = &path[c.unwrap().len_utf8()..];
        return glob_match_parts(pat_next, path_next);
    }

    false
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
pub struct DocumentIndexOutput {
    /// Number of chunks written.
    pub chunks_written: usize,
    /// Whether the document was actually indexed (vs skipped as unchanged).
    pub was_indexed: bool,
    /// The new document record (for updating the index).
    pub record: DocumentRecord,
}

/// Index a single document: extract → chunk → embed → upsert.
///
/// If the document's content hash matches an existing record, returns early
/// (incremental skip). If changed, deletes old chunks before inserting new ones.
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
    // Extract text and blocks
    let extraction = extractor.extract(&input.bytes, input.filename.as_deref())?;

    // Compute content hash
    let hash = content_hash(&extraction.text);

    // Content-addressed document ID
    let doc_id = document_id(&input.uri, &hash);

    // Check for incremental skip: same hash, same policy → skip
    if let Some(existing) = doc_index.get(&input.uri) {
        if existing.content_hash == hash && existing.policy_version == config.policy_version {
            // Unchanged — nothing to do
            return Ok(DocumentIndexOutput {
                chunks_written: 0,
                was_indexed: false,
                record: existing.clone(),
            });
        }

        // Content or policy changed: delete old chunks
        store.delete_by_document(&existing.document_id).await?;
    }

    // Set document_id on blocks
    let mut blocks: Vec<Block> = extraction.blocks.clone();
    for block in &mut blocks {
        block.document_id = doc_id.clone();
    }

    // Chunk the document
    let chunker_cfg = config.chunker.clone();
    let chunk_outputs = chunk_document(&doc_id, &extraction.text, &blocks, &chunker_cfg)?;

    if chunk_outputs.is_empty() {
        // No chunks produced (empty doc) — still record it as indexed
        let record = DocumentRecord {
            uri: input.uri.clone(),
            document_id: doc_id,
            content_hash: hash,
            policy_version: config.policy_version.clone(),
        };
        return Ok(DocumentIndexOutput {
            chunks_written: 0,
            was_indexed: true,
            record,
        });
    }

    // Embed the chunks (document-aware interface)
    let doc_chunks = DocumentChunks {
        document_context: extraction.text.clone(),
        chunks: chunk_outputs.iter().map(|c| c.text.clone()).collect(),
    };

    let embedded = embedder.embed_documents(vec![doc_chunks]).await?;
    let embeddings = &embedded[0];

    // Build Chunk and ChunkRecord structures
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

    let mut records = Vec::new();
    for (chunk_out, embedding) in chunk_outputs.iter().zip(embeddings.iter()) {
        let chunk = Chunk {
            id: chunk_out.id.clone(),
            document_id: doc_id.clone(),
            store_id: config.store_id.clone(),
            text: chunk_out.text.clone(),
            span: chunk_out.span.clone(),
            heading_path: chunk_out.heading_path.clone(),
            policy_version: config.policy_version.clone(),
            provenance: provenance.clone(),
        };

        let record = ChunkRecord::from_chunk(
            &chunk,
            embedding.clone(),
            input.uri.clone(),
            extraction.title.clone(),
            input.mime.clone(),
        );
        records.push(record);
    }

    let written = records.len();
    store.upsert_chunks(records).await?;

    let record = DocumentRecord {
        uri: input.uri.clone(),
        document_id: doc_id,
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
    /// Extract normalized text + blocks from raw bytes.
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
    /// Normalized document text.
    pub text: String,
    /// Structural blocks.
    pub blocks: Vec<Block>,
    /// Optional document title.
    pub title: Option<String>,
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
            SourceKind::Path => "path".to_string(),
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
}

/// Run the ingestion pipeline for a single source.
///
/// Handles path sources (file enumeration) and URL sources (HTTP fetch).
/// Builds an IndexJob, processes all documents, and returns the final job state.
///
/// # One-shot semantics
/// This function runs synchronously in embedded mode. The daemon (T11) wraps
/// it in a job queue and adds file watching.
pub async fn run_ingestion_for_source(
    source: &Source,
    doc_index: &mut DocumentIndex,
    store: &dyn RetrievalStore,
    embedder: &dyn Embedder,
    config: &IngestionConfig,
    extractor: &dyn DocumentExtractor,
    url_fetcher: Option<&dyn UrlFetcher>,
) -> Result<IngestionResult, Error> {
    let mut result = IngestionResult::default();
    let mut ctx = PipelineCtx {
        source,
        doc_index,
        store,
        embedder,
        config,
        extractor,
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
    let files = enumerate_path_source(root, include, exclude)?;
    let seen_uris: std::collections::HashSet<String> =
        files.iter().map(|f| f.uri.clone()).collect();

    // Process each file
    for file in &files {
        result.docs_seen += 1;

        let bytes = match std::fs::read(&file.path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("cannot read file '{}': {}", file.path.display(), e);
                result.error_count += 1;
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
                } else {
                    result.docs_skipped += 1;
                }
                ctx.doc_index.upsert(output.record);
            }
            Err(Error::UnsupportedFormat { .. }) => {
                result.unsupported_format_count += 1;
            }
            Err(e) => {
                tracing::warn!("error indexing '{}': {}", file.uri, e);
                result.error_count += 1;
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
                    .delete_by_document(&old_record.document_id)
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
                uri,
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
                    } else {
                        result.docs_skipped += 1;
                    }
                    ctx.doc_index.upsert(output.record);
                }
                Err(Error::UnsupportedFormat { .. }) => {
                    result.unsupported_format_count += 1;
                }
                Err(e) => {
                    tracing::warn!("error indexing URL '{}': {}", url, e);
                    result.error_count += 1;
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
                    .delete_by_document(&old_record.document_id)
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
    use crate::store::FakeStore;
    use crate::types::{BlockKind, SourceKind, SourceSpec};
    use crate::Span;

    // ---------------------------------------------------------------------------
    // FakeExtractor — a test double for DocumentExtractor
    // ---------------------------------------------------------------------------

    struct FakeExtractor;

    impl DocumentExtractor for FakeExtractor {
        fn extract(&self, bytes: &[u8], filename: Option<&str>) -> Result<ExtractionResult, Error> {
            let text = std::str::from_utf8(bytes)
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

            let blocks = if text.is_empty() {
                vec![]
            } else {
                vec![Block {
                    document_id: String::new(),
                    ordinal: 0,
                    kind: BlockKind::Paragraph,
                    text: text.clone(),
                    span: Span::new(0, text.len()),
                    heading_path: vec![],
                }]
            };

            Ok(ExtractionResult {
                text,
                blocks,
                title: None,
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
            source_kind_preset: "prose".to_string(),
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
            source_kind_preset: "prose".to_string(),
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
            document_id: "doc-id-1".to_string(),
            content_hash: "hash-1".to_string(),
            policy_version: "v1".to_string(),
        };
        idx.upsert(rec.clone());
        let found = idx.get("file:///test.md").unwrap();
        assert_eq!(found.document_id, "doc-id-1");
    }

    #[test]
    fn document_index_remove() {
        let mut idx = DocumentIndex::new();
        let rec = DocumentRecord {
            uri: "file:///test.md".to_string(),
            document_id: "doc-id-1".to_string(),
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
        use std::collections::HashMap;

        let records = vec![ChunkRecord {
            id: "chunk-1".to_string(),
            document_id: "doc-1".to_string(),
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
            source_kind: "path".to_string(),
            mime: None,
            uri: "file:///doc1.md".to_string(),
            title: None,
            meta: HashMap::new(),
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
    fn enumerate_path_source_uris_are_file_uris() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.md"), b"content").unwrap();

        let root = dir.path().to_str().unwrap();
        let files = enumerate_path_source(root, &[], &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].uri.starts_with("file://"));
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
            .document_id
            .clone();

        let out2 = index_document(
            &input_v2, &doc_index, &store, &embedder, &config, &extractor,
        )
        .await
        .unwrap();
        assert!(out2.was_indexed, "changed content should be indexed");
        assert!(out2.chunks_written > 0);

        // Old chunks should be gone
        let old_chunks = store.get_chunks_for_document(&old_doc_id).await.unwrap();
        assert!(
            old_chunks.is_empty(),
            "old document chunks should be deleted on replace"
        );
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
        )
        .await
        .unwrap();

        let old_doc_id = doc_index
            .get(&format!(
                "file://{}",
                dir.path().join("doc.md").canonicalize().unwrap().display()
            ))
            .map(|r| r.document_id.clone());

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
        )
        .await
        .unwrap();

        assert_eq!(result2.docs_indexed, 1, "edited doc should be re-indexed");
        assert_eq!(result2.docs_skipped, 0);

        // Old chunks should be removed
        if let Some(old_id) = old_doc_id {
            let old_chunks = store.get_chunks_for_document(&old_id).await.unwrap();
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
        let existing_doc_id = document_id(url, &existing_hash);

        // Insert some chunks for this document
        use crate::store::ChunkRecord;
        use std::collections::HashMap;
        let chunk = ChunkRecord {
            id: "old-chunk".to_string(),
            document_id: existing_doc_id.clone(),
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
            source_kind: "url".to_string(),
            mime: None,
            uri: url.to_string(),
            title: None,
            meta: HashMap::new(),
        };
        store.upsert_chunks(vec![chunk]).await.unwrap();

        doc_index.upsert(DocumentRecord {
            uri: url.to_string(),
            document_id: existing_doc_id.clone(),
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
        )
        .await
        .unwrap();

        assert_eq!(result.docs_deleted, 1, "gone URL should trigger deletion");
        let remaining = store
            .get_chunks_for_document(&existing_doc_id)
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
}
