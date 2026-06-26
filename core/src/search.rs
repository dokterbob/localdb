//! Hybrid search & citations — T08.
//!
//! Implements query orchestration: BM25 leg + dense leg (query embedding via Embedder),
//! RRF fusion (k=60, K=50 per leg), multi-store fan-out with global fusion,
//! metadata/store filters, and result shaping to Citation objects with per-leg scores.
//!
//! A no-op rerank seam is left between fuse and shape.
//!
//! See specs/04-search-pipeline.md §5 and specs/02-domain-model.md §6.

use std::collections::HashMap;
use std::sync::Arc;

use crate::citation::{Citation, CitationProvenance, CitationStore, Score};
use crate::embedder::{DocumentChunks, Embedder};
use crate::error::Error;
use crate::store::{ChunkRecord, MetadataFilter, RetrievalStore, SearchResult};
use crate::types::Span;

// ---------------------------------------------------------------------------
// RRF constants
// ---------------------------------------------------------------------------

/// RRF smoothing parameter (k = 60, per spec).
pub const RRF_K: f64 = 60.0;

/// Default number of results per leg (K = 50, per spec).
pub const DEFAULT_LEG_K: usize = 50;

/// Default number of final results to return (N = 10, per spec).
pub const DEFAULT_TOP_N: usize = 10;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A named store handle for fan-out search.
///
/// Bundles a `RetrievalStore` implementation with human-readable metadata
/// for citation construction.
pub struct StoreHandle {
    /// Store ID (ULID string).
    pub id: String,
    /// Store name.
    pub name: String,
    /// The underlying store.
    pub store: Arc<dyn RetrievalStore>,
}

/// Query request for the search orchestrator.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    /// The query text (used for BM25 and to embed for dense search).
    pub query: String,
    /// Number of results per leg. Defaults to [`DEFAULT_LEG_K`].
    pub leg_k: Option<usize>,
    /// Number of final results to return. Defaults to [`DEFAULT_TOP_N`].
    pub top_n: Option<usize>,
    /// Optional metadata filters pushed down to each backend.
    pub filters: Vec<MetadataFilter>,
}

/// Query response with ranked citations.
#[derive(Debug, Clone)]
pub struct QueryResponse {
    /// Ranked citation results.
    pub citations: Vec<Citation>,
    /// Total number of unique chunks considered (before truncation).
    pub total_candidates: usize,
}

// ---------------------------------------------------------------------------
// RRF fusion logic (pure, no I/O — critical function, ≥80% coverage required)
// ---------------------------------------------------------------------------

/// Compute the RRF score contribution for rank `i` (0-indexed) with smoothing `k`.
///
/// Formula: `1 / (k + rank + 1)` where rank is 1-indexed.
#[inline]
pub fn rrf_score(rank_0indexed: usize, k: f64) -> f64 {
    1.0 / (k + (rank_0indexed as f64) + 1.0)
}

/// Intermediate fused entry for a single chunk.
#[derive(Debug, Clone)]
pub struct FusedChunkEntry {
    /// The chunk.
    pub chunk: ChunkRecord,
    /// Cumulative RRF score.
    pub fused_score: f64,
    /// Dense leg raw score (if present).
    pub dense_score: Option<f64>,
    /// BM25 leg raw score (if present).
    pub bm25_score: Option<f64>,
}

/// Fuse two ranked lists using Reciprocal Rank Fusion.
///
/// - `dense_results`: ranked results from the dense leg (most similar first).
/// - `bm25_results`: ranked results from the BM25 leg (highest score first).
/// - `k`: RRF smoothing parameter (default `RRF_K = 60`).
///
/// Returns fused entries sorted by descending fused score, with deterministic
/// tie-breaking by chunk_id (ascending).
///
/// # Algorithm
///
/// For each result in each leg at 0-indexed rank `r`, add `1 / (k + r + 1)` to the
/// chunk's fused score. Chunks appearing in only one leg still get a score.
pub fn rrf_fuse(
    dense_results: &[SearchResult],
    bm25_results: &[SearchResult],
    k: f64,
) -> Vec<FusedChunkEntry> {
    let mut entries: HashMap<String, FusedChunkEntry> = HashMap::new();

    for (rank, result) in dense_results.iter().enumerate() {
        let contribution = rrf_score(rank, k);
        let entry = entries
            .entry(result.chunk.id.clone())
            .or_insert_with(|| FusedChunkEntry {
                chunk: result.chunk.clone(),
                fused_score: 0.0,
                dense_score: None,
                bm25_score: None,
            });
        entry.fused_score += contribution;
        entry.dense_score = Some(result.score as f64);
    }

    for (rank, result) in bm25_results.iter().enumerate() {
        let contribution = rrf_score(rank, k);
        let entry = entries
            .entry(result.chunk.id.clone())
            .or_insert_with(|| FusedChunkEntry {
                chunk: result.chunk.clone(),
                fused_score: 0.0,
                dense_score: None,
                bm25_score: None,
            });
        entry.fused_score += contribution;
        entry.bm25_score = Some(result.score as f64);
    }

    let mut sorted: Vec<FusedChunkEntry> = entries.into_values().collect();
    sorted.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.chunk.id.cmp(&b.chunk.id))
    });
    sorted
}

// ---------------------------------------------------------------------------
// Rerank seam (no-op in MVP)
// ---------------------------------------------------------------------------

/// No-op rerank stage — left as a seam for future reranking models.
///
/// Per spec: "explicitly post-MVP". The pipeline calls this between fuse and shape.
pub fn rerank_noop(results: Vec<FusedChunkEntry>) -> Vec<FusedChunkEntry> {
    results
}

// ---------------------------------------------------------------------------
// Citation shaping
// ---------------------------------------------------------------------------

/// Shape a fused result into a `Citation`.
pub fn shape_citation(fused: FusedChunkEntry, store_id: String, store_name: String) -> Citation {
    Citation {
        chunk_id: fused.chunk.id.clone(),
        document_id: fused.chunk.document_id.clone(),
        store: CitationStore {
            id: store_id,
            name: store_name,
        },
        uri: fused.chunk.uri.clone(),
        title: fused.chunk.metadata.title.clone(),
        heading_path: fused.chunk.heading_path.clone(),
        span: Span {
            start: fused.chunk.span.start,
            end: fused.chunk.span.end,
        },
        snippet: fused.chunk.text.clone(),
        score: Score {
            fused: fused.fused_score,
            dense: fused.dense_score,
            bm25: fused.bm25_score,
        },
        provenance: CitationProvenance {
            fetched_at: fused.chunk.fetched_at.clone(),
            content_hash: fused.chunk.content_hash.clone(),
        },
        metadata: fused.chunk.metadata.clone(),
    }
}

// ---------------------------------------------------------------------------
// SearchOrchestrator — the main entry point
// ---------------------------------------------------------------------------

/// Query orchestrator for hybrid search.
///
/// Performs:
/// 1. Embed the query text via the provided `Embedder`.
/// 2. Fan out BM25 + dense queries to each `StoreHandle` sequentially.
/// 3. Apply per-store RRF fusion.
/// 4. Merge results from all stores, sort globally by fused score.
/// 5. Apply the no-op rerank seam.
/// 6. Shape the top-N results into `Citation` objects.
///
/// See specs/04-search-pipeline.md §5.
pub struct SearchOrchestrator;

impl SearchOrchestrator {
    /// Execute a hybrid search query across one or more stores.
    ///
    /// `stores`: the store handles to fan out to. Each is queried independently,
    ///           then results are merged globally.
    /// `embedder`: used to embed the query text for the dense leg.
    /// `request`: query parameters.
    pub async fn query(
        stores: &[StoreHandle],
        embedder: &dyn Embedder,
        request: &QueryRequest,
    ) -> Result<QueryResponse, Error> {
        if stores.is_empty() {
            return Ok(QueryResponse {
                citations: vec![],
                total_candidates: 0,
            });
        }

        let leg_k = request.leg_k.unwrap_or(DEFAULT_LEG_K);
        let top_n = request.top_n.unwrap_or(DEFAULT_TOP_N);

        // 1. Embed the query text for the dense leg.
        let query_embedding = Self::embed_query(embedder, &request.query).await?;

        // 2. Fan out to each store, collect per-store fused entries with store metadata.
        let mut all_entries: Vec<(FusedChunkEntry, String, String)> = Vec::new();

        for handle in stores {
            let (dense_results, bm25_results) = Self::search_store(
                handle,
                &query_embedding,
                &request.query,
                leg_k,
                &request.filters,
            )
            .await?;

            let fused = rrf_fuse(&dense_results, &bm25_results, RRF_K);

            for entry in fused {
                all_entries.push((entry, handle.id.clone(), handle.name.clone()));
            }
        }

        let total_candidates = all_entries.len();

        if total_candidates == 0 {
            return Ok(QueryResponse {
                citations: vec![],
                total_candidates: 0,
            });
        }

        // 3. Global sort: merge all per-store entries and sort by fused score descending,
        //    with chunk_id as tiebreaker for determinism.
        all_entries.sort_by(|a, b| {
            b.0.fused_score
                .partial_cmp(&a.0.fused_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.chunk.id.cmp(&b.0.chunk.id))
        });

        // 4. Rerank seam (no-op): extract entries, pass through, then re-attach store info.
        let entries_only: Vec<FusedChunkEntry> =
            all_entries.iter().map(|(e, _, _)| e.clone()).collect();
        let reranked = rerank_noop(entries_only);
        // Re-attach store metadata by index (rerank is a no-op so order is unchanged).
        let final_entries: Vec<(FusedChunkEntry, String, String)> = reranked
            .into_iter()
            .enumerate()
            .map(|(i, entry)| {
                let (_, sid, sname) = &all_entries[i];
                (entry, sid.clone(), sname.clone())
            })
            .collect();

        // 5. Take top_n and shape into Citations.
        let citations: Vec<Citation> = final_entries
            .into_iter()
            .take(top_n)
            .map(|(entry, sid, sname)| shape_citation(entry, sid, sname))
            .collect();

        Ok(QueryResponse {
            citations,
            total_candidates,
        })
    }

    // ---------------------------------------------------------------------------
    // Private helpers
    // ---------------------------------------------------------------------------

    /// Embed a query string using the embedder.
    ///
    /// The query is treated as a single-chunk document (degenerate case).
    async fn embed_query(embedder: &dyn Embedder, query: &str) -> Result<Vec<f32>, Error> {
        let docs = vec![DocumentChunks {
            document_context: query.to_string(),
            chunks: vec![query.to_string()],
        }];
        let embedded = embedder.embed_documents(docs).await?;
        Ok(embedded
            .into_iter()
            .next()
            .and_then(|d| d.into_iter().next())
            .unwrap_or_default())
    }

    /// Run both search legs against a single store sequentially.
    async fn search_store(
        handle: &StoreHandle,
        query_vector: &[f32],
        query_text: &str,
        leg_k: usize,
        filters: &[MetadataFilter],
    ) -> Result<(Vec<SearchResult>, Vec<SearchResult>), Error> {
        let dense = handle
            .store
            .dense_search(query_vector, leg_k, filters)
            .await?;
        let bm25 = handle.store.bm25_search(query_text, leg_k, filters).await?;
        Ok((dense, bm25))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{DocumentChunks, FakeEmbedder};
    use crate::store::{ChunkRecord, FakeStore, SearchResult};
    use crate::types::Span;

    // -----------------------------------------------------------------------
    // Helper: make a ChunkRecord for tests
    // -----------------------------------------------------------------------

    fn make_chunk(
        id: &str,
        doc_id: &str,
        store_id: &str,
        text: &str,
        heading_path: Vec<String>,
        uri: &str,
        embedding: Vec<f32>,
    ) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            document_id: doc_id.to_string(),
            store_id: store_id.to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path,
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            source_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: uri.to_string(),
            metadata: crate::parser::DocumentMetadata::default(),
        }
    }

    fn make_search_result(chunk: ChunkRecord, score: f32) -> SearchResult {
        SearchResult { chunk, score }
    }

    /// Embed a text using FakeEmbedder (async version for use in async tests).
    async fn embed_text(embedder: &FakeEmbedder, text: &str) -> Vec<f32> {
        let docs = vec![DocumentChunks {
            document_context: text.to_string(),
            chunks: vec![text.to_string()],
        }];
        let result = embedder.embed_documents(docs).await.unwrap();
        result
            .into_iter()
            .next()
            .and_then(|d| d.into_iter().next())
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // RRF unit tests — hand-computed fixtures
    // -----------------------------------------------------------------------

    /// Basic RRF score formula verification.
    #[test]
    fn rrf_score_formula_correct() {
        // rank 0 (1st place), k=60: 1 / (60 + 0 + 1) = 1/61
        let expected = 1.0 / 61.0;
        assert!((rrf_score(0, 60.0) - expected).abs() < 1e-10);

        // rank 1 (2nd place), k=60: 1 / (60 + 1 + 1) = 1/62
        let expected = 1.0 / 62.0;
        assert!((rrf_score(1, 60.0) - expected).abs() < 1e-10);

        // rank 49 (50th place), k=60: 1 / (60 + 49 + 1) = 1/110
        let expected = 1.0 / 110.0;
        assert!((rrf_score(49, 60.0) - expected).abs() < 1e-10);
    }

    /// RRF score decreases monotonically with rank.
    #[test]
    fn rrf_score_monotonically_decreasing() {
        for rank in 0..49 {
            assert!(
                rrf_score(rank, 60.0) > rrf_score(rank + 1, 60.0),
                "score at rank {} should be greater than rank {}",
                rank,
                rank + 1
            );
        }
    }

    /// Hand-computed RRF fusion: two results each in both legs.
    ///
    /// chunk-A: rank 0 in dense, rank 0 in BM25 → 1/61 + 1/61 = 2/61
    /// chunk-B: rank 1 in dense, rank 1 in BM25 → 1/62 + 1/62 = 2/62
    /// chunk-C: rank 2 in dense only → 1/63
    /// chunk-D: rank 2 in BM25 only → 1/63
    #[test]
    fn rrf_fuse_hand_computed_scores() {
        let chunk_a = make_chunk(
            "A",
            "doc-1",
            "s1",
            "text A",
            vec![],
            "file:///a.md",
            vec![1.0, 0.0],
        );
        let chunk_b = make_chunk(
            "B",
            "doc-2",
            "s1",
            "text B",
            vec![],
            "file:///b.md",
            vec![0.9, 0.1],
        );
        let chunk_c = make_chunk(
            "C",
            "doc-3",
            "s1",
            "text C",
            vec![],
            "file:///c.md",
            vec![0.8, 0.2],
        );
        let chunk_d = make_chunk(
            "D",
            "doc-4",
            "s1",
            "text D",
            vec![],
            "file:///d.md",
            vec![0.7, 0.3],
        );

        let dense = vec![
            make_search_result(chunk_a.clone(), 0.99),
            make_search_result(chunk_b.clone(), 0.88),
            make_search_result(chunk_c.clone(), 0.75),
        ];
        let bm25 = vec![
            make_search_result(chunk_a.clone(), 10.0),
            make_search_result(chunk_b.clone(), 8.0),
            make_search_result(chunk_d.clone(), 5.0),
        ];

        let fused = rrf_fuse(&dense, &bm25, 60.0);

        // chunk-A should be rank 1: 2/61 ≈ 0.03279
        assert_eq!(fused[0].chunk.id, "A", "A should be rank 1");
        // chunk-B should be rank 2: 2/62 ≈ 0.03226
        assert_eq!(fused[1].chunk.id, "B", "B should be rank 2");
        // C and D tie at 1/63 — alphabetical tiebreak: C < D
        assert!(
            fused[2].chunk.id == "C" || fused[2].chunk.id == "D",
            "C or D should be rank 3"
        );
        assert!(
            fused[3].chunk.id == "C" || fused[3].chunk.id == "D",
            "C or D should be rank 4"
        );

        // Verify exact scores
        let expected_a = 1.0 / 61.0 + 1.0 / 61.0;
        assert!(
            (fused[0].fused_score - expected_a).abs() < 1e-10,
            "A's fused score should be 2/61, got {}",
            fused[0].fused_score
        );

        let expected_b = 1.0 / 62.0 + 1.0 / 62.0;
        assert!(
            (fused[1].fused_score - expected_b).abs() < 1e-10,
            "B's fused score should be 2/62, got {}",
            fused[1].fused_score
        );

        // Verify per-leg scores are retained (f32 → f64 conversion is approximate)
        let dense_score = fused[0]
            .dense_score
            .expect("A's dense score should be present");
        assert!(
            (dense_score - 0.99f64).abs() < 1e-4,
            "A's dense score should be ~0.99, got {dense_score}"
        );
        let bm25_score = fused[0]
            .bm25_score
            .expect("A's BM25 score should be present");
        assert!(
            (bm25_score - 10.0f64).abs() < 1e-4,
            "A's BM25 score should be ~10.0, got {bm25_score}"
        );

        // C only appeared in dense
        let c = fused.iter().find(|e| e.chunk.id == "C").unwrap();
        assert!(c.dense_score.is_some(), "C should have a dense score");
        assert!(c.bm25_score.is_none(), "C should have no BM25 score");

        // D only appeared in BM25
        let d = fused.iter().find(|e| e.chunk.id == "D").unwrap();
        assert!(d.dense_score.is_none(), "D should have no dense score");
        assert!(d.bm25_score.is_some(), "D should have a BM25 score");
    }

    /// Tie test: two chunks with identical RRF scores are ordered by chunk_id.
    #[test]
    fn rrf_fuse_tie_ordering_is_deterministic() {
        // chunk-A in BM25 rank 0 only, chunk-Z in dense rank 0 only → both score 1/61
        let chunk_a = make_chunk(
            "A",
            "doc-1",
            "s1",
            "text A",
            vec![],
            "file:///a.md",
            vec![1.0],
        );
        let chunk_z = make_chunk(
            "Z",
            "doc-2",
            "s1",
            "text Z",
            vec![],
            "file:///z.md",
            vec![0.5],
        );

        let dense = vec![make_search_result(chunk_z.clone(), 0.9)];
        let bm25 = vec![make_search_result(chunk_a.clone(), 5.0)];

        let fused = rrf_fuse(&dense, &bm25, 60.0);
        assert_eq!(fused.len(), 2);
        // Same score; alphabetical tiebreak: A < Z
        assert_eq!(fused[0].chunk.id, "A");
        assert_eq!(fused[1].chunk.id, "Z");
    }

    /// Single-leg test: if only BM25 has results, they still appear in fused output.
    #[test]
    fn rrf_fuse_single_leg_only_bm25() {
        let chunk = make_chunk(
            "X",
            "doc-1",
            "s1",
            "text X",
            vec![],
            "file:///x.md",
            vec![1.0],
        );
        let bm25 = vec![make_search_result(chunk.clone(), 7.5)];
        let fused = rrf_fuse(&[], &bm25, 60.0);

        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].chunk.id, "X");
        assert!(fused[0].dense_score.is_none());
        assert!(fused[0].bm25_score.is_some());
        let expected = 1.0 / 61.0;
        assert!((fused[0].fused_score - expected).abs() < 1e-10);
    }

    /// Single-leg test: if only dense has results, they still appear in fused output.
    #[test]
    fn rrf_fuse_single_leg_only_dense() {
        let chunk = make_chunk(
            "Y",
            "doc-1",
            "s1",
            "text Y",
            vec![],
            "file:///y.md",
            vec![1.0],
        );
        let dense = vec![make_search_result(chunk.clone(), 0.85)];
        let fused = rrf_fuse(&dense, &[], 60.0);

        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].chunk.id, "Y");
        assert!(fused[0].dense_score.is_some());
        assert!(fused[0].bm25_score.is_none());
    }

    /// Empty inputs → empty output.
    #[test]
    fn rrf_fuse_empty_inputs() {
        let fused = rrf_fuse(&[], &[], 60.0);
        assert!(fused.is_empty());
    }

    /// Single result in each leg (same chunk) → fused score = 2/61.
    #[test]
    fn rrf_fuse_single_chunk_both_legs() {
        let chunk = make_chunk(
            "X",
            "doc-1",
            "s1",
            "text",
            vec![],
            "file:///x.md",
            vec![1.0],
        );
        let dense = vec![make_search_result(chunk.clone(), 0.95)];
        let bm25 = vec![make_search_result(chunk.clone(), 9.0)];
        let fused = rrf_fuse(&dense, &bm25, 60.0);

        assert_eq!(fused.len(), 1);
        let expected = 2.0 / 61.0;
        assert!((fused[0].fused_score - expected).abs() < 1e-10);
    }

    /// Test many results in each leg with known rankings.
    ///
    /// dense input order: [chunk-4, chunk-3, chunk-2, chunk-1, chunk-0]
    /// So chunk-4 is at rank 0 (score 1/61), chunk-3 at rank 1 (1/62), etc.
    /// After RRF fusion the output order must match the input rank order.
    #[test]
    fn rrf_fuse_multiple_results_ordering() {
        // chunks 0..4 created in ascending ID order
        let chunks: Vec<ChunkRecord> = (0..5)
            .map(|i| {
                make_chunk(
                    &format!("{i}"),
                    "doc-1",
                    "s1",
                    &format!("text {i}"),
                    vec![],
                    &format!("file:///{i}.md"),
                    vec![1.0],
                )
            })
            .collect();

        // Provide chunks in reverse order so chunk-4 is at dense rank 0, chunk-0 at rank 4.
        let dense: Vec<SearchResult> = chunks
            .iter()
            .rev()
            .cloned()
            .map(|chunk| SearchResult { chunk, score: 1.0 })
            .collect();

        let fused = rrf_fuse(&dense, &[], 60.0);
        assert_eq!(fused.len(), 5);

        // Fused scores must be strictly decreasing (each chunk is at a unique rank).
        for i in 0..fused.len() - 1 {
            assert!(
                fused[i].fused_score > fused[i + 1].fused_score,
                "scores must be strictly decreasing: rank {} ({}) vs rank {} ({})",
                i,
                fused[i].fused_score,
                i + 1,
                fused[i + 1].fused_score,
            );
        }

        // The chunk at rank 0 in the dense list (chunk-4) must be first in fused output.
        assert_eq!(
            fused[0].chunk.id, "4",
            "chunk-4 (dense rank 0) should be first in fused output"
        );
        // The chunk at rank 4 in the dense list (chunk-0) must be last.
        assert_eq!(
            fused[4].chunk.id, "0",
            "chunk-0 (dense rank 4) should be last in fused output"
        );
    }

    // -----------------------------------------------------------------------
    // Citation shaping tests
    // -----------------------------------------------------------------------

    #[test]
    fn shape_citation_carries_correct_fields() {
        let chunk = make_chunk(
            "chunk-1",
            "doc-1",
            "store-A",
            "The quick brown fox",
            vec!["Overview".to_string(), "Details".to_string()],
            "file:///docs/guide.md",
            vec![0.5, 0.5],
        );
        let entry = FusedChunkEntry {
            chunk,
            fused_score: 0.0327,
            dense_score: Some(0.92),
            bm25_score: Some(8.5),
        };

        let citation = shape_citation(entry, "store-A".to_string(), "my-store".to_string());

        assert_eq!(citation.chunk_id, "chunk-1");
        assert_eq!(citation.document_id, "doc-1");
        assert_eq!(citation.store.id, "store-A");
        assert_eq!(citation.store.name, "my-store");
        assert_eq!(citation.uri, "file:///docs/guide.md");
        assert_eq!(
            citation.heading_path,
            vec!["Overview".to_string(), "Details".to_string()]
        );
        assert_eq!(citation.span.start, 0);
        assert_eq!(citation.span.end, "The quick brown fox".len());
        assert_eq!(citation.snippet, "The quick brown fox");
        assert!((citation.score.fused - 0.0327).abs() < 1e-10);
        assert_eq!(citation.score.dense, Some(0.92));
        assert_eq!(citation.score.bm25, Some(8.5));
        assert_eq!(citation.provenance.fetched_at, "2026-06-10T12:00:00Z");
        assert_eq!(citation.provenance.content_hash, "abc123");
    }

    #[test]
    fn shape_citation_single_leg_scores_preserved() {
        let chunk = make_chunk("c1", "d1", "s1", "text", vec![], "file:///a.md", vec![1.0]);
        let entry = FusedChunkEntry {
            chunk,
            fused_score: 1.0 / 61.0,
            dense_score: Some(0.88),
            bm25_score: None, // only dense leg
        };

        let citation = shape_citation(entry, "s1".to_string(), "store-one".to_string());
        assert_eq!(citation.score.dense, Some(0.88));
        assert_eq!(citation.score.bm25, None);
    }

    #[test]
    fn shape_citation_serializes_to_canonical_json() {
        let chunk = make_chunk(
            "cid",
            "did",
            "sid",
            "snippet text",
            vec!["H1".to_string()],
            "file:///x.md",
            vec![1.0],
        );
        let entry = FusedChunkEntry {
            chunk,
            fused_score: 0.05,
            dense_score: Some(0.9),
            bm25_score: Some(3.5),
        };
        let citation = shape_citation(entry, "sid".to_string(), "my-store".to_string());

        let v: serde_json::Value = serde_json::to_value(&citation).unwrap();
        // Verify canonical shape from specs/02-domain-model.md §6
        assert!(v.get("chunk_id").is_some());
        assert!(v.get("document_id").is_some());
        assert!(v.get("store").is_some());
        assert!(v.get("uri").is_some());
        assert!(v.get("heading_path").is_some());
        assert!(v.get("span").is_some());
        assert!(v.get("snippet").is_some());
        assert!(v.get("score").is_some());
        assert!(v.get("provenance").is_some());
        assert!(v["score"].get("fused").is_some());
        assert!(v["score"].get("dense").is_some());
        assert!(v["score"].get("bm25").is_some());
        assert!(v["span"].get("start").is_some());
        assert!(v["span"].get("end").is_some());
    }

    #[test]
    fn shape_citation_carries_metadata() {
        let mut chunk = make_chunk("c1", "d1", "s1", "text", vec![], "file:///a.md", vec![1.0]);
        chunk.metadata = crate::parser::DocumentMetadata {
            title: Some("My Title".to_string()),
            creator: vec!["Bob".to_string()],
            date: Some("2026-03-01".to_string()),
            ..Default::default()
        };
        let entry = FusedChunkEntry {
            chunk,
            fused_score: 0.5,
            dense_score: None,
            bm25_score: Some(4.0),
        };
        let citation = shape_citation(entry, "s1".to_string(), "store-one".to_string());
        assert_eq!(citation.metadata.title.as_deref(), Some("My Title"));
        assert_eq!(citation.metadata.creator, vec!["Bob".to_string()]);
        assert_eq!(citation.metadata.date.as_deref(), Some("2026-03-01"));
    }

    // -----------------------------------------------------------------------
    // Multi-store fan-out tests (via SearchOrchestrator::query)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn query_empty_stores_returns_empty() {
        let embedder = FakeEmbedder::new(4);
        let request = QueryRequest {
            query: "test query".to_string(),
            leg_k: None,
            top_n: None,
            filters: vec![],
        };
        let result = SearchOrchestrator::query(&[], &embedder, &request)
            .await
            .unwrap();
        assert!(result.citations.is_empty());
        assert_eq!(result.total_candidates, 0);
    }

    #[tokio::test]
    async fn query_single_store_returns_citations() {
        let embedder = FakeEmbedder::new(4);
        let store = FakeStore::new();

        let text = "The quick brown fox jumps over the lazy dog";
        let chunk = make_chunk(
            "chunk-1",
            "doc-1",
            "store-A",
            text,
            vec!["Animals".to_string()],
            "file:///docs/animals.md",
            embed_text(&embedder, text).await,
        );
        store.upsert_chunks(vec![chunk]).await.unwrap();

        let handle = StoreHandle {
            id: "store-A".to_string(),
            name: "Animals Store".to_string(),
            store: Arc::new(store),
        };

        let request = QueryRequest {
            query: "quick fox".to_string(),
            leg_k: Some(10),
            top_n: Some(5),
            filters: vec![],
        };

        let result = SearchOrchestrator::query(&[handle], &embedder, &request)
            .await
            .unwrap();

        assert!(
            !result.citations.is_empty(),
            "should return at least one citation"
        );
        let c = &result.citations[0];
        assert_eq!(c.store.id, "store-A");
        assert_eq!(c.store.name, "Animals Store");
        assert_eq!(c.uri, "file:///docs/animals.md");
        assert_eq!(c.heading_path, vec!["Animals"]);
        assert!(c.score.fused > 0.0);
    }

    #[tokio::test]
    async fn query_multi_store_global_ordering() {
        // Prove that multi-store fan-out produces a globally consistent ordering.
        let embedder = FakeEmbedder::new(4);

        let text_a1 = "rust programming language performance";
        let store_a = FakeStore::new();
        let chunk_a1 = make_chunk(
            "a1",
            "doc-a1",
            "store-A",
            text_a1,
            vec![],
            "file:///a1.md",
            embed_text(&embedder, text_a1).await,
        );
        store_a.upsert_chunks(vec![chunk_a1]).await.unwrap();

        let text_b1 = "python web framework django";
        let text_b2 = "rust memory safety ownership";
        let store_b = FakeStore::new();
        let chunk_b1 = make_chunk(
            "b1",
            "doc-b1",
            "store-B",
            text_b1,
            vec![],
            "file:///b1.md",
            embed_text(&embedder, text_b1).await,
        );
        let chunk_b2 = make_chunk(
            "b2",
            "doc-b2",
            "store-B",
            text_b2,
            vec![],
            "file:///b2.md",
            embed_text(&embedder, text_b2).await,
        );
        store_b
            .upsert_chunks(vec![chunk_b1, chunk_b2])
            .await
            .unwrap();

        let handles = vec![
            StoreHandle {
                id: "store-A".to_string(),
                name: "Store A".to_string(),
                store: Arc::new(store_a),
            },
            StoreHandle {
                id: "store-B".to_string(),
                name: "Store B".to_string(),
                store: Arc::new(store_b),
            },
        ];

        let request = QueryRequest {
            query: "rust programming".to_string(),
            leg_k: Some(10),
            top_n: Some(5),
            filters: vec![],
        };

        let result = SearchOrchestrator::query(&handles, &embedder, &request)
            .await
            .unwrap();

        // We should get results from both stores
        assert!(result.total_candidates > 0);
        assert!(!result.citations.is_empty());

        // Results should be ordered by fused score descending
        for i in 0..result.citations.len().saturating_sub(1) {
            assert!(
                result.citations[i].score.fused >= result.citations[i + 1].score.fused,
                "citations should be ordered by fused score descending"
            );
        }
    }

    #[tokio::test]
    async fn query_top_n_respected() {
        let embedder = FakeEmbedder::new(4);
        let store = FakeStore::new();

        let mut chunks: Vec<ChunkRecord> = Vec::new();
        for i in 0..20usize {
            let text = format!("search term content chunk number {i}");
            let emb = embed_text(&embedder, &text).await;
            chunks.push(make_chunk(
                &format!("chunk-{i}"),
                &format!("doc-{i}"),
                "store-A",
                &text,
                vec![],
                &format!("file:///doc{i}.md"),
                emb,
            ));
        }
        store.upsert_chunks(chunks).await.unwrap();

        let handle = StoreHandle {
            id: "store-A".to_string(),
            name: "Store A".to_string(),
            store: Arc::new(store),
        };

        let request = QueryRequest {
            query: "search term".to_string(),
            leg_k: Some(50),
            top_n: Some(5),
            filters: vec![],
        };

        let result = SearchOrchestrator::query(&[handle], &embedder, &request)
            .await
            .unwrap();

        assert!(
            result.citations.len() <= 5,
            "top_n=5 should limit results to at most 5, got {}",
            result.citations.len()
        );
    }

    #[tokio::test]
    async fn query_with_metadata_filter() {
        let embedder = FakeEmbedder::new(4);
        let store = FakeStore::new();

        let md_text = "markdown documentation content";
        let mut chunk_md = make_chunk(
            "md-chunk",
            "doc-md",
            "store-A",
            md_text,
            vec![],
            "file:///docs/guide.md",
            embed_text(&embedder, md_text).await,
        );
        chunk_md.mime = Some("text/markdown".to_string());

        let py_text = "python documentation content";
        let mut chunk_py = make_chunk(
            "py-chunk",
            "doc-py",
            "store-A",
            py_text,
            vec![],
            "file:///docs/guide.py",
            embed_text(&embedder, py_text).await,
        );
        chunk_py.mime = Some("text/x-python".to_string());

        store.upsert_chunks(vec![chunk_md, chunk_py]).await.unwrap();

        let handle = StoreHandle {
            id: "store-A".to_string(),
            name: "Store A".to_string(),
            store: Arc::new(store),
        };

        let request = QueryRequest {
            query: "documentation".to_string(),
            leg_k: Some(10),
            top_n: Some(10),
            filters: vec![MetadataFilter::Mime("text/markdown".to_string())],
        };

        let result = SearchOrchestrator::query(&[handle], &embedder, &request)
            .await
            .unwrap();

        // Only the markdown chunk should be returned
        for citation in &result.citations {
            assert_eq!(
                citation.chunk_id, "md-chunk",
                "filter should exclude non-markdown chunks"
            );
        }
    }

    #[tokio::test]
    async fn query_citations_have_correct_span_and_heading_path() {
        let embedder = FakeEmbedder::new(4);
        let store = FakeStore::new();

        let text = "Important content here";
        let mut chunk = make_chunk(
            "span-chunk",
            "doc-1",
            "store-A",
            text,
            vec!["Chapter 1".to_string(), "Section 2".to_string()],
            "file:///book.md",
            embed_text(&embedder, text).await,
        );
        chunk.span = Span::new(42, 64);

        store.upsert_chunks(vec![chunk]).await.unwrap();

        let handle = StoreHandle {
            id: "store-A".to_string(),
            name: "Store A".to_string(),
            store: Arc::new(store),
        };

        let request = QueryRequest {
            query: "Important content".to_string(),
            leg_k: Some(10),
            top_n: Some(5),
            filters: vec![],
        };

        let result = SearchOrchestrator::query(&[handle], &embedder, &request)
            .await
            .unwrap();

        let c = result
            .citations
            .iter()
            .find(|c| c.chunk_id == "span-chunk")
            .expect("span-chunk should be in results");

        assert_eq!(c.span.start, 42, "span.start should be preserved");
        assert_eq!(c.span.end, 64, "span.end should be preserved");
        assert_eq!(
            c.heading_path,
            vec!["Chapter 1".to_string(), "Section 2".to_string()],
            "heading_path should be preserved"
        );
        assert_eq!(c.uri, "file:///book.md");
    }

    /// Relevance smoke test: known query finds known doc in top 3.
    #[tokio::test]
    async fn relevance_smoke_test_known_query_in_top_3() {
        let embedder = FakeEmbedder::new(16);
        let store = FakeStore::new();

        let relevant_text = "Rust ownership and borrowing rules for memory safety";
        let irrelevant1 = "Python asyncio event loop and coroutines tutorial";
        let irrelevant2 = "JavaScript promises and async await patterns";
        let irrelevant3 = "SQL database normalization third normal form";
        let irrelevant4 = "CSS flexbox layout and grid systems";

        let chunks = vec![
            make_chunk(
                "irrelevant-1",
                "d1",
                "s",
                irrelevant1,
                vec![],
                "file:///1.md",
                embed_text(&embedder, irrelevant1).await,
            ),
            make_chunk(
                "irrelevant-2",
                "d2",
                "s",
                irrelevant2,
                vec![],
                "file:///2.md",
                embed_text(&embedder, irrelevant2).await,
            ),
            make_chunk(
                "relevant",
                "d3",
                "s",
                relevant_text,
                vec![],
                "file:///relevant.md",
                embed_text(&embedder, relevant_text).await,
            ),
            make_chunk(
                "irrelevant-3",
                "d4",
                "s",
                irrelevant3,
                vec![],
                "file:///3.md",
                embed_text(&embedder, irrelevant3).await,
            ),
            make_chunk(
                "irrelevant-4",
                "d5",
                "s",
                irrelevant4,
                vec![],
                "file:///4.md",
                embed_text(&embedder, irrelevant4).await,
            ),
        ];
        store.upsert_chunks(chunks).await.unwrap();

        let handle = StoreHandle {
            id: "s".to_string(),
            name: "Test Store".to_string(),
            store: Arc::new(store),
        };

        let request = QueryRequest {
            query: "Rust memory safety ownership".to_string(),
            leg_k: Some(10),
            top_n: Some(5),
            filters: vec![],
        };

        let result = SearchOrchestrator::query(&[handle], &embedder, &request)
            .await
            .unwrap();

        let top_3_ids: Vec<&str> = result
            .citations
            .iter()
            .take(3)
            .map(|c| c.chunk_id.as_str())
            .collect();
        assert!(
            top_3_ids.contains(&"relevant"),
            "known query 'Rust memory safety ownership' should find 'relevant' chunk in top 3, got {:?}",
            top_3_ids
        );
    }

    // -----------------------------------------------------------------------
    // Rerank seam test
    // -----------------------------------------------------------------------

    #[test]
    fn rerank_noop_preserves_order() {
        let chunk = make_chunk("c1", "d1", "s1", "text", vec![], "file:///a.md", vec![1.0]);
        let entries = vec![
            FusedChunkEntry {
                chunk: chunk.clone(),
                fused_score: 0.9,
                dense_score: Some(0.9),
                bm25_score: None,
            },
            FusedChunkEntry {
                chunk: {
                    let mut c = chunk.clone();
                    c.id = "c2".to_string();
                    c
                },
                fused_score: 0.5,
                dense_score: None,
                bm25_score: Some(5.0),
            },
        ];
        let reranked = rerank_noop(entries.clone());
        assert_eq!(reranked.len(), entries.len());
        assert_eq!(reranked[0].chunk.id, "c1");
        assert_eq!(reranked[1].chunk.id, "c2");
    }

    #[test]
    fn rerank_noop_empty() {
        let reranked = rerank_noop(vec![]);
        assert!(reranked.is_empty());
    }

    // -----------------------------------------------------------------------
    // Default constants tests
    // -----------------------------------------------------------------------

    #[test]
    fn default_constants_match_spec() {
        assert_eq!(RRF_K, 60.0, "RRF_K should be 60 per spec");
        assert_eq!(DEFAULT_LEG_K, 50, "DEFAULT_LEG_K should be 50 per spec");
        assert_eq!(DEFAULT_TOP_N, 10, "DEFAULT_TOP_N should be 10 per spec");
    }
}
