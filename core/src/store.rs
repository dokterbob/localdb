//! The `RetrievalStore` trait and related types.
//!
//! This is the abstraction layer between `core` domain logic and the physical
//! storage backend. The default implementation is in `store-libsql`.
//!
//! Fusion (RRF) happens **above** this trait in `core` — the trait exposes raw
//! BM25 and dense search legs separately.
//!
//! See specs/01-architecture.md §4 and specs/04-search-pipeline.md §5.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;

use crate::ids::{ContentId, UlidId};
use crate::ingestion::DocumentRecord;
use crate::parser::DocumentMetadata;
use crate::types::{Chunk, Span};
use crate::Error;

// ---------------------------------------------------------------------------
// ChunkRecord — the unit stored in a backend
// ---------------------------------------------------------------------------

/// A chunk record as stored in the retrieval backend.
///
/// This contains all fields needed for BM25, dense search, and citation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// Content-addressed chunk ID.
    pub id: ContentId,

    /// Parent document ID.
    pub document_id: ContentId,

    /// Owning store ID.
    pub store_id: UlidId,

    /// Chunk text (feeds BM25).
    pub text: String,

    /// Range in the normalized document text.
    pub span: Span,

    /// Heading path inherited from blocks.
    #[serde(default)]
    pub heading_path: Vec<String>,

    /// Dense embedding vector.
    pub embedding: Vec<f32>,

    /// Hash of the indexing policy that produced this chunk.
    pub policy_version: String,

    /// Acquisition time (RFC 3339 string). Used for metadata filters.
    pub fetched_at: String,

    /// blake3 content hash of normalized text (hex string).
    pub content_hash: String,

    /// Origin store ID (for federation provenance).
    pub origin_store: UlidId,

    /// Source ID.
    pub source_id: UlidId,

    /// Source kind (e.g. "path", "url").
    pub source_kind: String,

    /// MIME type for metadata filtering.
    #[serde(default)]
    pub mime: Option<String>,

    /// Document URI (e.g. `file:///path/to/file` or URL).
    pub uri: String,

    /// Document metadata extracted from the document.
    ///
    /// Persisted as a JSON-encoded column. Read defensively: stores created
    /// before this schema migration return `DocumentMetadata::default()` on read.
    #[serde(default)]
    pub metadata: DocumentMetadata,
}

impl ChunkRecord {
    /// Construct a `ChunkRecord` from a `Chunk` plus supplementary fields.
    pub fn from_chunk(
        chunk: &Chunk,
        embedding: Vec<f32>,
        uri: String,
        mime: Option<String>,
        metadata: DocumentMetadata,
    ) -> Self {
        Self {
            id: chunk.id.clone(),
            document_id: chunk.document_id.clone(),
            store_id: chunk.store_id.clone(),
            text: chunk.text.clone(),
            span: chunk.span.clone(),
            heading_path: chunk.heading_path.clone(),
            embedding,
            policy_version: chunk.policy_version.clone(),
            fetched_at: chunk.provenance.fetched_at.clone(),
            content_hash: chunk.provenance.content_hash.clone(),
            origin_store: chunk.provenance.origin_store.clone(),
            source_id: chunk.provenance.source_ref.id.clone(),
            source_kind: chunk.provenance.source_ref.kind.clone(),
            mime,
            uri,
            metadata,
        }
    }
}

// ---------------------------------------------------------------------------
// SearchResult
// ---------------------------------------------------------------------------

/// A single search result from one leg (dense or BM25).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    /// The matching chunk record.
    pub chunk: ChunkRecord,

    /// The score for this result within its leg.
    /// Dense: cosine/dot-product similarity.
    /// BM25: BM25 score.
    pub score: f32,
}

// ---------------------------------------------------------------------------
// MetadataFilter — pushed down to the backend
// ---------------------------------------------------------------------------

/// A single metadata filter condition.
///
/// See specs/04-search-pipeline.md §5 (filter pushdown expectations).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MetadataFilter {
    /// Filter by MIME type.
    Mime(String),
    /// Filter by URI prefix.
    UriPrefix(String),
    /// Filter: fetched_at >= value (RFC 3339 string).
    FetchedAfter(String),
    /// Filter: fetched_at <= value (RFC 3339 string).
    FetchedBefore(String),
    /// Filter by source ID.
    SourceId(UlidId),
    /// Filter by document ID.
    DocumentId(ContentId),
    /// Filter by policy version.
    PolicyVersion(String),
}

// ---------------------------------------------------------------------------
// StoreStats
// ---------------------------------------------------------------------------

/// Statistics for a retrieval store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StoreStats {
    /// Number of chunks indexed.
    pub chunk_count: u64,
    /// Number of distinct documents with at least one chunk.
    pub document_count: u64,
}

// ---------------------------------------------------------------------------
// RetrievalStore trait
// ---------------------------------------------------------------------------

/// The storage abstraction for a single knowledge base.
///
/// Production storage is implemented by `store-libsql`.
///
/// This trait is object-safe and `Send + Sync` so it can be boxed and shared across async tasks.
///
/// **Design invariant**: fusion (RRF) is done **above** this trait in `core`, not in the
/// implementations. Each implementation exposes raw ranked lists from each leg.
///
/// See specs/01-architecture.md §4 and specs/04-search-pipeline.md §5.
#[async_trait]
pub trait RetrievalStore: Send + Sync + 'static {
    // ------------------------------------------------------------------
    // Writes (≥90% coverage required)
    // ------------------------------------------------------------------

    /// Upsert a batch of chunk records.
    ///
    /// If a record with the same `id` already exists, it is replaced.
    /// Returns the number of records written (implementations may return the total
    /// count passed in, or only net-new records — callers must not depend on the
    /// exact value for replaced records).
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error>;

    /// Delete all chunks belonging to a given document.
    ///
    /// Returns the number of chunks deleted.
    async fn delete_by_document(&self, document_id: &str) -> Result<usize, Error>;

    /// Delete all chunks belonging to a given store.
    ///
    /// Used when a store is removed or fully re-indexed.
    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error>;

    // ------------------------------------------------------------------
    // Reads
    // ------------------------------------------------------------------

    /// Dense vector search.
    ///
    /// Returns up to `limit` results ordered by descending similarity to `query_vector`.
    /// Optional metadata filters are pushed down to the backend.
    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error>;

    /// BM25 full-text search.
    ///
    /// Returns up to `limit` results ordered by descending BM25 score.
    /// Optional metadata filters are pushed down to the backend.
    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error>;

    /// Store-level statistics: chunk count, document count.
    async fn stats(&self) -> Result<StoreStats, Error>;

    /// Retrieve a specific chunk by ID. Returns `None` if not found.
    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error>;

    /// Retrieve all chunks for a given document.
    async fn get_chunks_for_document(&self, document_id: &str) -> Result<Vec<ChunkRecord>, Error>;

    /// Enumerate per-document indexing identity for every distinct document in the
    /// store. Used to rehydrate the incremental-skip index across process runs.
    ///
    /// One record per distinct URI (first chunk wins). Implementations must NOT
    /// return the embedding column to avoid loading vectors for the entire store.
    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error>;
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

/// An in-memory `RetrievalStore` for use in tests.
///
/// No persistence, no actual vector index — linear scan for both legs.
/// Dense search uses cosine similarity; BM25 uses simple term frequency scoring.
#[cfg(any(test, feature = "test-support"))]
pub struct FakeStore {
    chunks: tokio::sync::RwLock<Vec<ChunkRecord>>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeStore {
    /// Create a new empty fake store.
    pub fn new() -> Self {
        Self {
            chunks: tokio::sync::RwLock::new(Vec::new()),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for FakeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute cosine similarity between two vectors.
#[cfg(any(test, feature = "test-support"))]
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Simple term-frequency BM25 approximation for tests.
///
/// Not a real BM25 implementation — just counts term matches for test purposes.
#[cfg(any(test, feature = "test-support"))]
fn simple_bm25_score(query: &str, text: &str) -> f32 {
    let query_terms: Vec<&str> = query.split_whitespace().collect();
    if query_terms.is_empty() {
        return 0.0;
    }
    let text_lower = text.to_lowercase();
    let matched: usize = query_terms
        .iter()
        .filter(|t| text_lower.contains(&t.to_lowercase()))
        .count();
    matched as f32 / query_terms.len() as f32
}

/// Apply metadata filters to a chunk record. Returns `true` if the record passes.
#[cfg(any(test, feature = "test-support"))]
fn passes_filters(record: &ChunkRecord, filters: &[MetadataFilter]) -> bool {
    for filter in filters {
        match filter {
            MetadataFilter::Mime(mime) => {
                if record.mime.as_deref() != Some(mime.as_str()) {
                    return false;
                }
            }
            MetadataFilter::UriPrefix(prefix) => {
                if !record.uri.starts_with(prefix.as_str()) {
                    return false;
                }
            }
            MetadataFilter::FetchedAfter(ts) => {
                if record.fetched_at.as_str() < ts.as_str() {
                    return false;
                }
            }
            MetadataFilter::FetchedBefore(ts) => {
                if record.fetched_at.as_str() > ts.as_str() {
                    return false;
                }
            }
            MetadataFilter::SourceId(id) => {
                if &record.source_id != id {
                    return false;
                }
            }
            MetadataFilter::DocumentId(id) => {
                if &record.document_id != id {
                    return false;
                }
            }
            MetadataFilter::PolicyVersion(v) => {
                if &record.policy_version != v {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl RetrievalStore for FakeStore {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        let mut chunks = self.chunks.write().await;
        let mut count = 0;
        for record in records {
            if let Some(pos) = chunks.iter().position(|c| c.id == record.id) {
                chunks[pos] = record;
            } else {
                chunks.push(record);
                count += 1;
            }
        }
        Ok(count)
    }

    async fn delete_by_document(&self, document_id: &str) -> Result<usize, Error> {
        let mut chunks = self.chunks.write().await;
        let before = chunks.len();
        chunks.retain(|c| c.document_id != document_id);
        Ok(before - chunks.len())
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        let mut chunks = self.chunks.write().await;
        let before = chunks.len();
        chunks.retain(|c| c.store_id != store_id);
        Ok(before - chunks.len())
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let chunks = self.chunks.read().await;
        let mut results: Vec<SearchResult> = chunks
            .iter()
            .filter(|c| passes_filters(c, filters))
            .map(|c| {
                let score = cosine_similarity(query_vector, &c.embedding);
                SearchResult {
                    chunk: c.clone(),
                    score,
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let chunks = self.chunks.read().await;
        let mut results: Vec<SearchResult> = chunks
            .iter()
            .filter(|c| passes_filters(c, filters))
            .filter_map(|c| {
                let score = simple_bm25_score(query_text, &c.text);
                if score > 0.0 {
                    Some(SearchResult {
                        chunk: c.clone(),
                        score,
                    })
                } else {
                    None
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        let chunks = self.chunks.read().await;
        let chunk_count = chunks.len() as u64;
        let doc_ids: std::collections::HashSet<&str> =
            chunks.iter().map(|c| c.document_id.as_str()).collect();
        Ok(StoreStats {
            chunk_count,
            document_count: doc_ids.len() as u64,
        })
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        let chunks = self.chunks.read().await;
        Ok(chunks.iter().find(|c| c.id == chunk_id).cloned())
    }

    async fn get_chunks_for_document(&self, document_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        let chunks = self.chunks.read().await;
        Ok(chunks
            .iter()
            .filter(|c| c.document_id == document_id)
            .cloned()
            .collect())
    }

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        let chunks = self.chunks.read().await;
        let mut seen: HashMap<String, DocumentRecord> = HashMap::new();
        for chunk in chunks.iter() {
            seen.entry(chunk.uri.clone()).or_insert(DocumentRecord {
                uri: chunk.uri.clone(),
                document_id: chunk.document_id.clone(),
                content_hash: chunk.content_hash.clone(),
                policy_version: chunk.policy_version.clone(),
            });
        }
        Ok(seen.into_values().collect())
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

/// A shared test suite exercising the `RetrievalStore` contract.
///
/// Call this with any concrete implementation. Integration tests in `store-libsql`
/// run this same suite against the real libsql backend.
pub mod conformance {
    use super::*;

    fn make_record(
        id: &str,
        document_id: &str,
        store_id: &str,
        text: &str,
        embedding: Vec<f32>,
    ) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            document_id: document_id.to_string(),
            store_id: store_id.to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            source_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            metadata: crate::parser::DocumentMetadata::default(),
        }
    }

    /// Test: upsert then stats reflect correct counts.
    pub async fn test_upsert_and_stats(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "Hello world", vec![1.0, 0.0]),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "Another chunk",
                vec![0.0, 1.0],
            ),
            make_record(
                "chunk-3",
                "doc-2",
                "store-1",
                "Different document",
                vec![0.5, 0.5],
            ),
        ];
        let n = store.upsert_chunks(records).await.unwrap();
        assert_eq!(n, 3, "should upsert 3 new chunks");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 3, "chunk_count should be 3");
        assert_eq!(stats.document_count, 2, "document_count should be 2");
    }

    /// Test: upsert replaces existing chunks with the same ID.
    pub async fn test_upsert_replaces_existing(store: &dyn RetrievalStore) {
        let record = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "Original text",
            vec![1.0, 0.0],
        );
        store.upsert_chunks(vec![record]).await.unwrap();

        let updated = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "Updated text",
            vec![0.5, 0.5],
        );
        let n = store.upsert_chunks(vec![updated]).await.unwrap();
        // Replacement: count may be 0 (no net new chunks)
        let _ = n;

        let chunk = store.get_chunk("chunk-1").await.unwrap();
        assert!(chunk.is_some());
        assert_eq!(chunk.unwrap().text, "Updated text");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1, "should still have exactly 1 chunk");
    }

    /// Test: delete_by_document removes all chunks for that document.
    pub async fn test_delete_by_document(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "Doc1 chunk1", vec![1.0, 0.0]),
            make_record("chunk-2", "doc-1", "store-1", "Doc1 chunk2", vec![0.9, 0.1]),
            make_record("chunk-3", "doc-2", "store-1", "Doc2 chunk1", vec![0.0, 1.0]),
        ];
        store.upsert_chunks(records).await.unwrap();

        let deleted = store.delete_by_document("doc-1").await.unwrap();
        assert_eq!(deleted, 2, "should delete 2 chunks from doc-1");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1, "only doc-2 chunk remains");
        assert_eq!(stats.document_count, 1, "only doc-2 remains");

        // Verify the remaining chunk is from doc-2
        let remaining = store.get_chunks_for_document("doc-2").await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].document_id, "doc-2");
    }

    /// Test: delete_by_document on non-existent document returns 0.
    pub async fn test_delete_nonexistent_document(store: &dyn RetrievalStore) {
        let deleted = store.delete_by_document("nonexistent-doc").await.unwrap();
        assert_eq!(deleted, 0, "deleting nonexistent doc should return 0");
    }

    /// Test: dense search returns results ordered by similarity.
    pub async fn test_dense_search_round_trip(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "Close match", vec![1.0, 0.0]),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "Medium match",
                vec![0.707, 0.707],
            ),
            make_record("chunk-3", "doc-2", "store-1", "Far match", vec![0.0, 1.0]),
        ];
        store.upsert_chunks(records).await.unwrap();

        // Query close to chunk-1
        let results = store.dense_search(&[1.0, 0.0], 3, &[]).await.unwrap();
        assert!(!results.is_empty(), "should return results");
        assert_eq!(
            results[0].chunk.id, "chunk-1",
            "closest chunk should be first"
        );
        assert!(
            results[0].score >= results[1].score,
            "results should be sorted descending by score"
        );
    }

    /// Test: BM25 search returns results containing the query terms.
    pub async fn test_bm25_search_round_trip(store: &dyn RetrievalStore) {
        let records = vec![
            make_record(
                "chunk-1",
                "doc-1",
                "store-1",
                "The quick brown fox jumps",
                vec![1.0, 0.0],
            ),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "A lazy dog slept",
                vec![0.0, 1.0],
            ),
            make_record(
                "chunk-3",
                "doc-2",
                "store-1",
                "The fox was quick indeed",
                vec![0.5, 0.5],
            ),
        ];
        store.upsert_chunks(records).await.unwrap();

        let results = store.bm25_search("fox quick", 3, &[]).await.unwrap();
        assert!(!results.is_empty(), "BM25 search should find results");
        // Both chunk-1 and chunk-3 contain "fox" and "quick"
        let ids: Vec<&str> = results.iter().map(|r| r.chunk.id.as_str()).collect();
        assert!(
            ids.contains(&"chunk-1") || ids.contains(&"chunk-3"),
            "should find chunks with 'fox' and/or 'quick'"
        );
        // chunk-2 should not appear (no matching terms)
        assert!(
            !ids.contains(&"chunk-2"),
            "lazy dog chunk should not match 'fox quick'"
        );
    }

    /// Test: metadata filter by MIME type.
    pub async fn test_metadata_filter_mime(store: &dyn RetrievalStore) {
        let mut r1 = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "markdown doc",
            vec![1.0, 0.0],
        );
        r1.mime = Some("text/markdown".to_string());
        let mut r2 = make_record("chunk-2", "doc-2", "store-1", "html doc", vec![0.5, 0.5]);
        r2.mime = Some("text/html".to_string());

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::Mime("text/markdown".to_string())];
        let dense_results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(dense_results.len(), 1, "should only return markdown chunk");
        assert_eq!(dense_results[0].chunk.id, "chunk-1");

        let bm25_results = store.bm25_search("doc", 10, &filter).await.unwrap();
        assert_eq!(bm25_results.len(), 1, "BM25 should also filter by mime");
        assert_eq!(bm25_results[0].chunk.id, "chunk-1");
    }

    /// Test: metadata filter by URI prefix.
    pub async fn test_metadata_filter_uri_prefix(store: &dyn RetrievalStore) {
        let mut r1 = make_record("chunk-1", "doc-1", "store-1", "notes file", vec![1.0, 0.0]);
        r1.uri = "file:///home/user/notes/foo.md".to_string();
        let mut r2 = make_record("chunk-2", "doc-2", "store-1", "docs file", vec![0.5, 0.5]);
        r2.uri = "file:///home/user/docs/bar.md".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::UriPrefix(
            "file:///home/user/notes/".to_string(),
        )];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    /// Test: get_chunk by ID.
    pub async fn test_get_chunk(store: &dyn RetrievalStore) {
        let record = make_record("chunk-1", "doc-1", "store-1", "Hello", vec![1.0, 0.0]);
        store.upsert_chunks(vec![record.clone()]).await.unwrap();

        let found = store.get_chunk("chunk-1").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "chunk-1");

        let not_found = store.get_chunk("nonexistent").await.unwrap();
        assert!(not_found.is_none());
    }

    /// Test: get_chunks_for_document returns all chunks for a document.
    pub async fn test_get_chunks_for_document(store: &dyn RetrievalStore) {
        let records = vec![
            make_record("chunk-1", "doc-1", "store-1", "First chunk", vec![1.0, 0.0]),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "Second chunk",
                vec![0.9, 0.1],
            ),
            make_record("chunk-3", "doc-2", "store-1", "Other doc", vec![0.0, 1.0]),
        ];
        store.upsert_chunks(records).await.unwrap();

        let doc1_chunks = store.get_chunks_for_document("doc-1").await.unwrap();
        assert_eq!(doc1_chunks.len(), 2);

        let doc2_chunks = store.get_chunks_for_document("doc-2").await.unwrap();
        assert_eq!(doc2_chunks.len(), 1);

        let missing = store.get_chunks_for_document("nonexistent").await.unwrap();
        assert!(missing.is_empty());
    }

    /// Test: delete_by_store removes all chunks in a store.
    pub async fn test_delete_by_store(store: &dyn RetrievalStore) {
        let records = vec![
            make_record(
                "chunk-1",
                "doc-1",
                "store-A",
                "Store A chunk",
                vec![1.0, 0.0],
            ),
            make_record(
                "chunk-2",
                "doc-2",
                "store-A",
                "Another A chunk",
                vec![0.5, 0.5],
            ),
            make_record(
                "chunk-3",
                "doc-3",
                "store-B",
                "Store B chunk",
                vec![0.0, 1.0],
            ),
        ];
        store.upsert_chunks(records).await.unwrap();

        let deleted = store.delete_by_store("store-A").await.unwrap();
        assert_eq!(deleted, 2, "should delete 2 chunks from store-A");

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1);
    }

    /// Test: dense search with limit is respected.
    pub async fn test_dense_search_limit(store: &dyn RetrievalStore) {
        let records: Vec<ChunkRecord> = (0..5)
            .map(|i| {
                make_record(
                    &format!("chunk-{i}"),
                    "doc-1",
                    "store-1",
                    &format!("chunk text {i}"),
                    vec![i as f32 * 0.1, 1.0 - i as f32 * 0.1],
                )
            })
            .collect();
        store.upsert_chunks(records).await.unwrap();

        let results = store.dense_search(&[1.0, 0.0], 2, &[]).await.unwrap();
        assert_eq!(results.len(), 2, "limit should be respected");
    }

    /// Test: BM25 search with limit is respected.
    pub async fn test_bm25_search_limit(store: &dyn RetrievalStore) {
        let records: Vec<ChunkRecord> = (0..5)
            .map(|i| {
                make_record(
                    &format!("chunk-{i}"),
                    "doc-1",
                    "store-1",
                    &format!("search term chunk {i}"),
                    vec![0.5, 0.5],
                )
            })
            .collect();
        store.upsert_chunks(records).await.unwrap();

        let results = store.bm25_search("search term", 2, &[]).await.unwrap();
        assert_eq!(results.len(), 2, "BM25 limit should be respected");
    }

    /// Run a subset of the conformance suite that does not require a pre-built FTS index.
    ///
    /// The store must be freshly created (empty) when this is called.
    /// Note: because each conformance function leaves data in the store, this helper
    /// is only useful for backends that can provide a fresh store per call.  For
    /// fine-grained control call each `test_*` function directly (as the per-backend
    /// test modules do).
    ///
    /// Usage: in an async test, create a store, then call `run_non_fts(store).await`.
    pub async fn run_non_fts(store: &dyn RetrievalStore) {
        test_upsert_and_stats(store).await;
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::conformance::*;
    use super::*;

    fn make_test_record(id: &str, doc_id: &str, text: &str, embedding: Vec<f32>) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            document_id: doc_id.to_string(),
            store_id: "test-store".to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            origin_store: "test-store".to_string(),
            source_id: "src-1".to_string(),
            source_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            metadata: crate::parser::DocumentMetadata::default(),
        }
    }

    #[tokio::test]
    async fn fake_store_upsert_and_stats() {
        let store = FakeStore::new();
        test_upsert_and_stats(&store).await;
    }

    #[tokio::test]
    async fn fake_store_upsert_replaces_existing() {
        let store = FakeStore::new();
        test_upsert_replaces_existing(&store).await;
    }

    #[tokio::test]
    async fn fake_store_delete_by_document() {
        let store = FakeStore::new();
        test_delete_by_document(&store).await;
    }

    #[tokio::test]
    async fn fake_store_delete_nonexistent_document() {
        let store = FakeStore::new();
        test_delete_nonexistent_document(&store).await;
    }

    #[tokio::test]
    async fn fake_store_dense_search_round_trip() {
        let store = FakeStore::new();
        test_dense_search_round_trip(&store).await;
    }

    #[tokio::test]
    async fn fake_store_bm25_search_round_trip() {
        let store = FakeStore::new();
        test_bm25_search_round_trip(&store).await;
    }

    #[tokio::test]
    async fn fake_store_metadata_filter_mime() {
        let store = FakeStore::new();
        test_metadata_filter_mime(&store).await;
    }

    #[tokio::test]
    async fn fake_store_metadata_filter_uri_prefix() {
        let store = FakeStore::new();
        test_metadata_filter_uri_prefix(&store).await;
    }

    #[tokio::test]
    async fn fake_store_get_chunk() {
        let store = FakeStore::new();
        test_get_chunk(&store).await;
    }

    #[tokio::test]
    async fn fake_store_get_chunks_for_document() {
        let store = FakeStore::new();
        test_get_chunks_for_document(&store).await;
    }

    #[tokio::test]
    async fn fake_store_delete_by_store() {
        let store = FakeStore::new();
        test_delete_by_store(&store).await;
    }

    #[tokio::test]
    async fn fake_store_dense_search_limit() {
        let store = FakeStore::new();
        test_dense_search_limit(&store).await;
    }

    #[tokio::test]
    async fn fake_store_bm25_search_limit() {
        let store = FakeStore::new();
        test_bm25_search_limit(&store).await;
    }

    #[tokio::test]
    async fn fake_store_empty_stats() {
        let store = FakeStore::new();
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 0);
        assert_eq!(stats.document_count, 0);
    }

    #[tokio::test]
    async fn fake_store_dense_search_empty() {
        let store = FakeStore::new();
        let results = store.dense_search(&[1.0, 0.0], 10, &[]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn fake_store_bm25_search_empty() {
        let store = FakeStore::new();
        let results = store.bm25_search("test", 10, &[]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn fake_store_dense_search_sorted_descending() {
        let store = FakeStore::new();
        let records = vec![
            make_test_record("a", "doc-1", "text a", vec![0.0, 1.0]),
            make_test_record("b", "doc-1", "text b", vec![1.0, 0.0]),
            make_test_record("c", "doc-1", "text c", vec![0.707, 0.707]),
        ];
        store.upsert_chunks(records).await.unwrap();

        let results = store.dense_search(&[1.0, 0.0], 3, &[]).await.unwrap();
        assert_eq!(results.len(), 3);
        // Scores should be descending
        assert!(results[0].score >= results[1].score);
        assert!(results[1].score >= results[2].score);
        // chunk b should be first (closest to [1.0, 0.0])
        assert_eq!(results[0].chunk.id, "b");
    }

    #[tokio::test]
    async fn chunk_record_from_chunk_helper() {
        use crate::types::{Chunk, Provenance, SourceRef};

        let chunk = Chunk {
            id: "chunk-id".to_string(),
            document_id: "doc-id".to_string(),
            store_id: "store-id".to_string(),
            text: "Some text".to_string(),
            span: Span::new(0, 9),
            heading_path: vec!["Heading".to_string()],
            policy_version: "policy-v1".to_string(),
            provenance: Provenance {
                origin_store: "store-id".to_string(),
                source_ref: SourceRef {
                    id: "source-id".to_string(),
                    kind: "path".to_string(),
                },
                fetched_at: "2026-06-10T12:00:00Z".to_string(),
                content_hash: "abc123".to_string(),
                share_path: vec![],
            },
        };

        let record = ChunkRecord::from_chunk(
            &chunk,
            vec![0.1, 0.2, 0.3],
            "file:///test.md".to_string(),
            Some("text/markdown".to_string()),
            crate::parser::DocumentMetadata::default(),
        );

        assert_eq!(record.id, "chunk-id");
        assert_eq!(record.document_id, "doc-id");
        assert_eq!(record.store_id, "store-id");
        assert_eq!(record.text, "Some text");
        assert_eq!(record.embedding, vec![0.1, 0.2, 0.3]);
        assert_eq!(record.uri, "file:///test.md");
        assert_eq!(record.mime, Some("text/markdown".to_string()));
        assert_eq!(record.source_id, "source-id");
        assert_eq!(record.source_kind, "path");
    }

    #[tokio::test]
    async fn cosine_similarity_known_values() {
        // Identical vectors → 1.0
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        // Orthogonal vectors → 0.0
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-6);
        // Zero vector → 0.0
        assert!((cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]) - 0.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn metadata_filter_fetched_after() {
        let store = FakeStore::new();
        let mut r1 = make_test_record("old", "doc-1", "old text", vec![1.0, 0.0]);
        r1.fetched_at = "2026-01-01T00:00:00Z".to_string();
        let mut r2 = make_test_record("new", "doc-2", "new text", vec![0.5, 0.5]);
        r2.fetched_at = "2026-06-10T00:00:00Z".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::FetchedAfter(
            "2026-03-01T00:00:00Z".to_string(),
        )];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "new");
    }

    #[tokio::test]
    async fn metadata_filter_source_id() {
        let store = FakeStore::new();
        let mut r1 = make_test_record("chunk-1", "doc-1", "source A text", vec![1.0, 0.0]);
        r1.source_id = "source-A".to_string();
        let mut r2 = make_test_record("chunk-2", "doc-2", "source B text", vec![0.5, 0.5]);
        r2.source_id = "source-B".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::SourceId("source-A".to_string())];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    #[tokio::test]
    async fn metadata_filter_policy_version() {
        let store = FakeStore::new();
        let mut r1 = make_test_record("chunk-1", "doc-1", "v1 text", vec![1.0, 0.0]);
        r1.policy_version = "policy-v1".to_string();
        let mut r2 = make_test_record("chunk-2", "doc-2", "v2 text", vec![0.5, 0.5]);
        r2.policy_version = "policy-v2".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::PolicyVersion("policy-v1".to_string())];
        let results = store.dense_search(&[1.0, 0.0], 10, &filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");
    }
}
