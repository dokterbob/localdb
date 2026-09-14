//! Hybrid search & citations — T08.
//!
//! Implements query orchestration: BM25 leg + dense leg (query embedding via Embedder),
//! RRF fusion (k=60, K=50 per leg), multi-store fan-out, metadata/store filters, and
//! result shaping to Citation objects with per-leg scores.
//!
//! Multi-store fusion topology (issue #162): each leg's per-store results are
//! pooled into one globally rank-ordered list (`pool_leg_results`), and a
//! *single* RRF pass (`rrf_fuse_global`) runs over the two pooled legs, keyed
//! on the composite `(store_id, chunk_id)`. Fusing per-store and merging
//! already-fused scores would be wrong: RRF scores are rank-based and
//! scale-free, so every store's local rank-0 chunk would tie at the same
//! score regardless of how weak it actually is relative to other stores'
//! candidates.
//!
//! A no-op rerank seam is left between fuse and shape.
//!
//! See specs/04-search-pipeline.md §5 and specs/02-domain-model.md §6.

use std::collections::HashMap;
use std::sync::Arc;

use crate::citation::{
    ChunkPosition, Citation, CitationBlock, CitationLocation, CitationProvenance, CitationStore,
    Score,
};
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
// Search limit clamp
// ---------------------------------------------------------------------------

/// Maximum `limit`/`top_n` a client may request from any search-serving
/// surface. Requests above this are silently clamped, not rejected. All
/// three surfaces that accept a client-supplied result count clamp to this
/// single constant, so `localdb search --limit <huge>` behaves identically
/// whether it runs embedded or against the daemon:
/// - HTTP `POST /v1/search` (`server::search_service::clamp_search_limit`)
/// - the MCP `search` tool (`mcp::tools::resolve_search_limit`)
/// - the CLI's embedded `search` command
///   (`cli::cmds::search::SearchCmd::run_embedded`)
pub const SEARCH_MAX_LIMIT: usize = 100;

/// Clamp a client-supplied `limit` to [`SEARCH_MAX_LIMIT`], silently — no
/// error. Shared by the HTTP and CLI-embedded search surfaces, whose `limit`
/// is a plain `usize`. The MCP tool clamps separately
/// (`mcp::tools::resolve_search_limit`) because its `limit` is an
/// `Option<i64>` with its own absent/negative-handling semantics that don't
/// fit this signature.
#[inline]
pub fn clamp_search_limit(limit: usize) -> usize {
    limit.min(SEARCH_MAX_LIMIT)
}

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
    /// Total number of unique `(store, chunk)` candidates considered globally
    /// (before truncation to `top_n`). Store IDs partition the pooled legs,
    /// so this is numerically identical to the old per-store distinct-chunk-id
    /// sum — it just now counts the composite key directly.
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

#[derive(Debug, Clone, Copy)]
enum Leg {
    Dense,
    Bm25,
}

/// Fusion identity: the composite `(store_id, chunk_id)`.
///
/// See [`rrf_fuse_global`] for why `chunk_id` alone is not enough.
type FusionKey = (String, String);

fn fusion_key(chunk: &ChunkRecord) -> FusionKey {
    (chunk.store_id.clone(), chunk.id.clone())
}

/// Accumulate one leg's RRF contributions into `entries`, keyed on
/// [`FusionKey`].
///
/// For each result at 0-indexed rank `r`, add `1 / (k + r + 1)` to that
/// chunk's fused score. A chunk appearing in only one leg still gets a score.
fn add_leg(
    entries: &mut HashMap<FusionKey, FusedChunkEntry>,
    results: &[SearchResult],
    k: f64,
    leg: Leg,
) {
    for (rank, result) in results.iter().enumerate() {
        let contribution = rrf_score(rank, k);
        let entry = entries
            .entry(fusion_key(&result.chunk))
            .or_insert_with(|| FusedChunkEntry {
                chunk: result.chunk.clone(),
                fused_score: 0.0,
                dense_score: None,
                bm25_score: None,
            });

        entry.fused_score += contribution;

        match leg {
            Leg::Dense => entry.dense_score = Some(result.score as f64),
            Leg::Bm25 => entry.bm25_score = Some(result.score as f64),
        }
    }
}

/// Fuse two globally-pooled ranked lists using Reciprocal Rank Fusion, with
/// fusion identity keyed on the composite `(store_id, chunk_id)` rather than
/// `chunk_id` alone.
///
/// # Why the composite key
///
/// Chunk IDs are content-addressed (`core/src/ids.rs`), and the chunks table
/// is `UNIQUE (store_id, id)` — **not** `UNIQUE (id)` (see
/// `store-libsql/src/schema.rs`). The same document indexed into two
/// different stores therefore yields the *same* `chunk_id` in both stores.
/// Deduping fusion identity on `chunk_id` alone would silently merge two
/// stores' distinct hits into one entry and mis-attribute the survivor to
/// whichever store happened to win the `HashMap` insertion race. Keying on
/// `(store_id, chunk_id)` keeps every store's hit distinct even when the
/// underlying content — and thus the chunk_id — is identical.
///
/// For a single-store query the composite key degenerates to plain `chunk_id`
/// fusion: `store_id` is constant, so it can neither split nor merge entries,
/// and the `store_id` tiebreak below is a no-op. Single-store search therefore
/// behaves exactly as it did before global fusion existed.
///
/// # Precondition
///
/// `dense_results` and `bm25_results` must already be globally rank-ordered
/// across all stores (see [`pool_leg_results`]) — this function does not
/// re-derive cross-store ranking itself, it only fuses two already-pooled
/// per-leg rankings using each entry's position in the slice as its rank.
///
/// - `dense_results`: pooled, globally-ranked dense leg results (most similar first).
/// - `bm25_results`: pooled, globally-ranked BM25 leg results (highest score first).
/// - `k`: RRF smoothing parameter (default `RRF_K = 60`).
///
/// Returns fused entries sorted by descending fused score, with deterministic
/// tie-breaking by `store_id` ascending, then `chunk_id` ascending.
pub fn rrf_fuse_global(
    dense_results: &[SearchResult],
    bm25_results: &[SearchResult],
    k: f64,
) -> Vec<FusedChunkEntry> {
    let mut entries: HashMap<FusionKey, FusedChunkEntry> = HashMap::new();

    add_leg(&mut entries, dense_results, k, Leg::Dense);
    add_leg(&mut entries, bm25_results, k, Leg::Bm25);

    let mut sorted: Vec<FusedChunkEntry> = entries.into_values().collect();
    sorted.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.chunk.store_id.cmp(&b.chunk.store_id))
            .then_with(|| a.chunk.id.cmp(&b.chunk.id))
    });
    sorted
}

/// Sort one leg's concatenated per-store results into a single global ranking.
///
/// Order: `score` descending, then `store_id` ascending, then `chunk_id`
/// ascending.
///
/// # Why `store_id` is load-bearing in the tiebreak
///
/// A `chunk_id`-only tiebreak would suffice if all inputs came from one store.
/// Here they are pooled across stores, so two genuinely different chunks from
/// different stores can legitimately score identically — and because chunk IDs
/// are content-addressed, two stores holding the same content produce results
/// with an equal score *and* an equal `chunk_id`. Without `store_id` in the
/// sort key, that case would order nondeterministically depending on `Vec`
/// concatenation order.
///
/// This produces the rank ordering that [`rrf_fuse_global`] consumes as its
/// precondition.
fn pool_leg_results(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut pooled = results;
    pooled.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.chunk.store_id.cmp(&b.chunk.store_id))
            .then_with(|| a.chunk.id.cmp(&b.chunk.id))
    });
    pooled
}

/// Drop any result a store returned that is not stamped with that store's own
/// `store_id`, preserving the relative order of the rest.
///
/// Global fusion identity is the composite `(store_id, chunk_id)` and each
/// citation's store attribution is resolved from `chunk.store_id`, so a
/// mis-stamped chunk would be fused under the wrong key and attributed to the
/// wrong store — or, if its `store_id` matches no queried handle, surface with
/// an empty store name. No current `RetrievalStore` implementation can produce
/// one (the libsql read path filters `WHERE c.store_id = ?`), and a `debug_assert!`
/// in the fan-out loop fails loudly in dev builds if that ever changes. This is
/// the release-build backstop: `debug_assert!` compiles out, so without it a
/// mis-stamped chunk would pass through silently.
///
/// Dropping rather than relabelling is deliberate. Rewriting `store_id` to the
/// querying handle's id would make the invariant true by construction, but it
/// would also disguise a genuine cross-tenant leak as a correctly-attributed
/// result. Mirrors the same check in `mcp`'s `find_document_chunks`.
fn retain_own_chunks(results: Vec<SearchResult>, handle: &StoreHandle) -> Vec<SearchResult> {
    results
        .into_iter()
        .filter(|r| r.chunk.store_id == handle.id)
        .collect()
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
        resource_id: fused.chunk.resource_id.clone(),
        store: CitationStore {
            id: store_id,
            name: store_name,
        },
        uri: fused.chunk.uri.clone(),
        title: fused.chunk.metadata.title().map(|s| s.to_string()),
        heading_path: fused.chunk.heading_path.clone(),
        block: CitationBlock {
            seq: fused.chunk.block_seq,
            kind: fused.chunk.block_kind.clone(),
            page: fused.chunk.page,
        },
        chunk_position: ChunkPosition {
            seq_in_block: fused.chunk.seq_in_block,
        },
        location: CitationLocation {
            span: Span {
                start: fused.chunk.span.start,
                end: fused.chunk.span.end,
            },
            window_block_seqs: fused.chunk.window_block_seqs.clone(),
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
/// 3. Pool each leg's per-store results into one globally rank-ordered list,
///    then run a single global RRF pass keyed on `(store_id, chunk_id)`.
/// 4. Apply the no-op rerank seam.
/// 5. Shape the top-N results into `Citation` objects.
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

        // 2. Fan out to each store sequentially, accumulating each leg's raw
        //    per-store results into pools (no per-store fusion — see module doc).
        let mut dense_pool: Vec<SearchResult> = Vec::new();
        let mut bm25_pool: Vec<SearchResult> = Vec::new();
        let mut store_names: HashMap<String, String> = HashMap::new();

        for handle in stores {
            store_names.insert(handle.id.clone(), handle.name.clone());

            let (dense_results, bm25_results) = Self::search_store(
                handle,
                &query_embedding,
                &request.query,
                leg_k,
                &request.filters,
            )
            .await?;

            debug_assert!(
                dense_results.iter().all(|r| r.chunk.store_id == handle.id)
                    && bm25_results.iter().all(|r| r.chunk.store_id == handle.id),
                "store {} returned a chunk whose store_id does not match the handle it was \
                 fetched from — global fusion identity is keyed on (store_id, chunk_id), so \
                 every store must stamp its own store_id on the chunks it returns",
                handle.id
            );

            dense_pool.extend(retain_own_chunks(dense_results, handle));
            bm25_pool.extend(retain_own_chunks(bm25_results, handle));
        }

        // 3. Pool each leg into one globally rank-ordered list, then run a
        //    single global RRF pass over the two pooled legs.
        let pooled_dense = pool_leg_results(dense_pool);
        let pooled_bm25 = pool_leg_results(bm25_pool);
        let fused = rrf_fuse_global(&pooled_dense, &pooled_bm25, RRF_K);

        let total_candidates = fused.len();

        if total_candidates == 0 {
            return Ok(QueryResponse {
                citations: vec![],
                total_candidates: 0,
            });
        }

        // 4. Rerank seam (no-op) — operates directly on Vec<FusedChunkEntry>;
        //    store attribution lives in entry.chunk.store_id, so this seam
        //    survives a future reranker that reorders or drops entries.
        let reranked = rerank_noop(fused);

        // 5. Take top_n and shape into Citations, resolving each entry's
        //    store name from its own chunk.store_id.
        let citations: Vec<Citation> = reranked
            .into_iter()
            .take(top_n)
            .map(|entry| {
                let store_id = entry.chunk.store_id.clone();
                let store_name = store_names.get(&store_id).cloned().unwrap_or_default();
                shape_citation(entry, store_id, store_name)
            })
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
            resource_id: doc_id.to_string(),
            store_id: store_id.to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path,
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
            content_hash: "abc123".to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
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
    // Search limit clamp tests
    // -----------------------------------------------------------------------

    #[test]
    fn clamp_search_limit_passes_through_values_at_or_below_the_max() {
        assert_eq!(clamp_search_limit(1), 1);
        assert_eq!(clamp_search_limit(SEARCH_MAX_LIMIT), SEARCH_MAX_LIMIT);
    }

    #[test]
    fn clamp_search_limit_caps_values_above_the_max() {
        assert_eq!(clamp_search_limit(SEARCH_MAX_LIMIT + 1), SEARCH_MAX_LIMIT);
        assert_eq!(clamp_search_limit(100_000), SEARCH_MAX_LIMIT);
        assert_eq!(clamp_search_limit(usize::MAX), SEARCH_MAX_LIMIT);
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
    fn rrf_fuse_global_hand_computed_scores_single_store() {
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

        let fused = rrf_fuse_global(&dense, &bm25, 60.0);

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
    fn rrf_fuse_global_tie_ordering_is_deterministic() {
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

        let fused = rrf_fuse_global(&dense, &bm25, 60.0);
        assert_eq!(fused.len(), 2);
        // Same score; alphabetical tiebreak: A < Z
        assert_eq!(fused[0].chunk.id, "A");
        assert_eq!(fused[1].chunk.id, "Z");
    }

    /// Single-leg test: if only BM25 has results, they still appear in fused output.
    #[test]
    fn rrf_fuse_global_single_leg_only_bm25() {
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
        let fused = rrf_fuse_global(&[], &bm25, 60.0);

        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].chunk.id, "X");
        assert!(fused[0].dense_score.is_none());
        assert!(fused[0].bm25_score.is_some());
        let expected = 1.0 / 61.0;
        assert!((fused[0].fused_score - expected).abs() < 1e-10);
    }

    /// Single-leg test: if only dense has results, they still appear in fused output.
    #[test]
    fn rrf_fuse_global_single_leg_only_dense() {
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
        let fused = rrf_fuse_global(&dense, &[], 60.0);

        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].chunk.id, "Y");
        assert!(fused[0].dense_score.is_some());
        assert!(fused[0].bm25_score.is_none());
    }

    /// Empty inputs → empty output.
    #[test]
    fn rrf_fuse_global_empty_inputs() {
        let fused = rrf_fuse_global(&[], &[], 60.0);
        assert!(fused.is_empty());
    }

    /// Single result in each leg (same chunk) → fused score = 2/61.
    #[test]
    fn rrf_fuse_global_single_chunk_both_legs() {
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
        let fused = rrf_fuse_global(&dense, &bm25, 60.0);

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
    fn rrf_fuse_global_multiple_results_ordering() {
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

        let fused = rrf_fuse_global(&dense, &[], 60.0);
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
    // Global RRF fusion tests (issue #162)
    //
    // The historical bug: `SearchOrchestrator::query` fused each store's two
    // legs on its own and merged the already-fused entries — since RRF scores
    // are rank-based and scale-free, every store's local rank-0 chunk ties at
    // 2/61 regardless of actual quality. The fix is `rrf_fuse_global`: pool
    // each leg across all stores first (`pool_leg_results`), then fuse once
    // over the pooled, globally rank-ordered lists. Because chunk IDs are
    // content-addressed and the chunks table is `UNIQUE (store_id, id)` (not
    // `UNIQUE (id)`), the same document indexed into two stores yields the
    // *same* chunk_id in both stores — so the fusion identity must be the
    // composite `(store_id, chunk_id)`, never `chunk_id` alone.
    // -----------------------------------------------------------------------

    /// The SAME chunk_id under two different store_ids must yield two
    /// distinct fused entries, each carrying its own store's per-leg scores.
    /// This is the test that falsifies the naive "dedupe on chunk_id"
    /// approach to global fusion.
    #[test]
    fn rrf_fuse_global_same_chunk_id_in_two_stores_stay_distinct() {
        let chunk_in_store_1 = make_chunk(
            "shared-id",
            "doc-1",
            "store-1",
            "text in store 1",
            vec![],
            "file:///store1/a.md",
            vec![1.0],
        );
        let chunk_in_store_2 = make_chunk(
            "shared-id",
            "doc-2",
            "store-2",
            "text in store 2",
            vec![],
            "file:///store2/a.md",
            vec![1.0],
        );

        let dense = vec![make_search_result(chunk_in_store_1.clone(), 0.9)];
        let bm25 = vec![make_search_result(chunk_in_store_2.clone(), 5.0)];

        let fused = rrf_fuse_global(&dense, &bm25, 60.0);

        assert_eq!(
            fused.len(),
            2,
            "same chunk_id under two different store_ids must yield two distinct entries, got {}",
            fused.len()
        );

        let entry_1 = fused
            .iter()
            .find(|e| e.chunk.store_id == "store-1")
            .expect("store-1's entry should be present");
        assert_eq!(entry_1.chunk.id, "shared-id");
        assert!(
            entry_1.dense_score.is_some(),
            "store-1's entry should carry the dense score"
        );
        assert!(
            entry_1.bm25_score.is_none(),
            "store-1's entry should not carry store-2's bm25 score"
        );

        let entry_2 = fused
            .iter()
            .find(|e| e.chunk.store_id == "store-2")
            .expect("store-2's entry should be present");
        assert_eq!(entry_2.chunk.id, "shared-id");
        assert!(
            entry_2.bm25_score.is_some(),
            "store-2's entry should carry the bm25 score"
        );
        assert!(
            entry_2.dense_score.is_none(),
            "store-2's entry should not carry store-1's dense score"
        );
    }

    /// Mirrors `rrf_fuse_global_hand_computed_scores_single_store`, but
    /// chunk-A/chunk-C live in store `s1` and chunk-B/chunk-D live in store
    /// `s2`. None of these
    /// chunk_ids collide across stores, so the hand-computed arithmetic must
    /// be identical to the single-store case.
    #[test]
    fn rrf_fuse_global_hand_computed_scores_across_stores() {
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
            "s2",
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
            "s2",
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

        let fused = rrf_fuse_global(&dense, &bm25, 60.0);

        // chunk-A should be rank 1: 2/61 ≈ 0.03279
        assert_eq!(fused[0].chunk.id, "A", "A should be rank 1");
        assert_eq!(fused[0].chunk.store_id, "s1");
        // chunk-B should be rank 2: 2/62 ≈ 0.03226
        assert_eq!(fused[1].chunk.id, "B", "B should be rank 2");
        assert_eq!(fused[1].chunk.store_id, "s2");

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

        // C (store s1) and D (store s2) tie at 1/63 — store_id tiebreak: s1 < s2.
        assert_eq!(fused[2].chunk.id, "C");
        assert_eq!(fused[2].chunk.store_id, "s1");
        assert_eq!(fused[3].chunk.id, "D");
        assert_eq!(fused[3].chunk.store_id, "s2");

        let expected_cd = 1.0 / 63.0;
        assert!(
            (fused[2].fused_score - expected_cd).abs() < 1e-10,
            "C's fused score should be 1/63, got {}",
            fused[2].fused_score
        );
        assert!(
            (fused[3].fused_score - expected_cd).abs() < 1e-10,
            "D's fused score should be 1/63, got {}",
            fused[3].fused_score
        );
    }

    /// Equal `fused_score` entries must be ordered by `store_id` ascending
    /// first, then `chunk_id` ascending — never by `chunk_id` alone (see
    /// `rrf_fuse_global_same_chunk_id_in_two_stores_stay_distinct` for why
    /// chunk_id can't be the sole fusion/tiebreak key).
    #[test]
    fn rrf_fuse_global_tiebreak_orders_by_store_id_then_chunk_id() {
        let chunk_store2_a = make_chunk(
            "A",
            "doc-1",
            "store-2",
            "text",
            vec![],
            "file:///1.md",
            vec![1.0],
        );
        let chunk_store1_b = make_chunk(
            "B",
            "doc-2",
            "store-1",
            "text",
            vec![],
            "file:///2.md",
            vec![1.0],
        );
        let chunk_store1_d = make_chunk(
            "D",
            "doc-3",
            "store-1",
            "text",
            vec![],
            "file:///3.md",
            vec![1.0],
        );
        let chunk_store1_c = make_chunk(
            "C",
            "doc-4",
            "store-1",
            "text",
            vec![],
            "file:///4.md",
            vec![1.0],
        );

        // dense rank0 = store-2/A (1/61), dense rank1 = store-1/D (1/62)
        let dense = vec![
            make_search_result(chunk_store2_a.clone(), 0.9),
            make_search_result(chunk_store1_d.clone(), 0.5),
        ];
        // bm25 rank0 = store-1/B (1/61), bm25 rank1 = store-1/C (1/62)
        let bm25 = vec![
            make_search_result(chunk_store1_b.clone(), 9.0),
            make_search_result(chunk_store1_c.clone(), 4.0),
        ];

        let fused = rrf_fuse_global(&dense, &bm25, 60.0);
        assert_eq!(fused.len(), 4);

        // 1/61 group: store-1/B before store-2/A — store_id tiebreak wins
        // even though chunk_id "A" < "B" alphabetically.
        assert_eq!(
            (fused[0].chunk.store_id.as_str(), fused[0].chunk.id.as_str()),
            ("store-1", "B")
        );
        assert_eq!(
            (fused[1].chunk.store_id.as_str(), fused[1].chunk.id.as_str()),
            ("store-2", "A")
        );

        // 1/62 group: same store_id, so chunk_id decides — C before D.
        assert_eq!(
            (fused[2].chunk.store_id.as_str(), fused[2].chunk.id.as_str()),
            ("store-1", "C")
        );
        assert_eq!(
            (fused[3].chunk.store_id.as_str(), fused[3].chunk.id.as_str()),
            ("store-1", "D")
        );

        let expected_group1 = 1.0 / 61.0;
        assert!((fused[0].fused_score - expected_group1).abs() < 1e-10);
        assert!((fused[1].fused_score - expected_group1).abs() < 1e-10);

        let expected_group2 = 1.0 / 62.0;
        assert!((fused[2].fused_score - expected_group2).abs() < 1e-10);
        assert!((fused[3].fused_score - expected_group2).abs() < 1e-10);
    }

    /// Sort order within a single pooled leg: `score` desc, then `store_id`
    /// asc, then `chunk_id` asc. The equal-score/equal-chunk_id-across-stores
    /// case proves `store_id` is load-bearing in the sort key — without it,
    /// two results with identical score AND chunk_id (the same content
    /// indexed into two stores) would sort nondeterministically.
    #[test]
    fn pool_leg_results_orders_by_score_desc_then_store_id_then_chunk_id() {
        let high = make_search_result(
            make_chunk(
                "m",
                "doc-1",
                "store-1",
                "text",
                vec![],
                "file:///m.md",
                vec![1.0],
            ),
            0.9,
        );
        // Equal score (0.5), different store_id AND different chunk_id.
        let tie_a_store2 = make_search_result(
            make_chunk(
                "b",
                "doc-2",
                "store-2",
                "text",
                vec![],
                "file:///b.md",
                vec![1.0],
            ),
            0.5,
        );
        let tie_a_store1 = make_search_result(
            make_chunk(
                "c",
                "doc-3",
                "store-1",
                "text",
                vec![],
                "file:///c.md",
                vec![1.0],
            ),
            0.5,
        );
        // Equal score (0.3) AND equal chunk_id, different store_id.
        let tie_b_store2 = make_search_result(
            make_chunk(
                "same-id",
                "doc-4",
                "store-2",
                "text",
                vec![],
                "file:///s2.md",
                vec![1.0],
            ),
            0.3,
        );
        let tie_b_store1 = make_search_result(
            make_chunk(
                "same-id",
                "doc-5",
                "store-1",
                "text",
                vec![],
                "file:///s1.md",
                vec![1.0],
            ),
            0.3,
        );

        let input = vec![
            tie_a_store2.clone(),
            high.clone(),
            tie_b_store1.clone(),
            tie_a_store1.clone(),
            tie_b_store2.clone(),
        ];

        let pooled = pool_leg_results(input);

        let actual: Vec<(&str, &str)> = pooled
            .iter()
            .map(|r| (r.chunk.store_id.as_str(), r.chunk.id.as_str()))
            .collect();

        assert_eq!(
            actual,
            vec![
                ("store-1", "m"),
                ("store-1", "c"),
                ("store-2", "b"),
                ("store-1", "same-id"),
                ("store-2", "same-id"),
            ]
        );
    }

    /// A store that returns a chunk stamped with someone else's `store_id` has
    /// its mis-stamped results dropped, not relabelled — otherwise the chunk
    /// would fuse under the wrong composite key and be attributed to the wrong
    /// store (or surface with an empty store name).
    ///
    /// This is exercised on `retain_own_chunks` directly rather than through
    /// `SearchOrchestrator::query`, because the fan-out loop's `debug_assert!`
    /// deliberately panics on this input in dev builds (tests included). The
    /// filter is the *release*-build backstop for when that assert compiles
    /// out, so the helper is the only place the drop behavior is observable
    /// under `cargo test`.
    #[test]
    fn retain_own_chunks_drops_results_stamped_with_another_store_id() {
        let handle = StoreHandle {
            id: "store-A".to_string(),
            name: "Store A".to_string(),
            store: Arc::new(FakeStore::new()),
        };

        let mine_first = make_search_result(
            make_chunk("a", "d1", "store-A", "t", vec![], "file:///a.md", vec![1.0]),
            0.9,
        );
        let foreign = make_search_result(
            make_chunk("x", "d2", "store-B", "t", vec![], "file:///x.md", vec![1.0]),
            0.8,
        );
        let mine_last = make_search_result(
            make_chunk("b", "d3", "store-A", "t", vec![], "file:///b.md", vec![1.0]),
            0.7,
        );

        let kept = retain_own_chunks(vec![mine_first, foreign, mine_last], &handle);

        // The foreign chunk is gone; surviving results keep their relative order.
        assert_eq!(
            kept.iter().map(|r| r.chunk.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(kept.iter().all(|r| r.chunk.store_id == "store-A"));
    }

    /// An all-foreign result set collapses to empty rather than leaking.
    #[test]
    fn retain_own_chunks_drops_every_foreign_result() {
        let handle = StoreHandle {
            id: "store-A".to_string(),
            name: "Store A".to_string(),
            store: Arc::new(FakeStore::new()),
        };
        let foreign = make_search_result(
            make_chunk("x", "d1", "store-B", "t", vec![], "file:///x.md", vec![1.0]),
            0.8,
        );

        assert!(retain_own_chunks(vec![foreign], &handle).is_empty());
    }

    /// Headline regression test for issue #162.
    ///
    /// OLD topology: fusion ran once per store, then the already-fused results
    /// were merged. Because RRF is rank-based and scale-free, each store's
    /// local rank-0 chunk gets the *same* score (2/61) regardless of how
    /// strong that chunk actually is relative to chunks in other stores — a
    /// mediocre chunk that happens to be alone in its store ties the
    /// genuinely-best chunk from a store with many strong candidates.
    ///
    /// That half is reproduced below by calling `rrf_fuse_global` once per
    /// store, on that store's own results only — which is exactly what the old
    /// code did. Handed a single store's results the composite key degenerates
    /// to plain `chunk_id` fusion, so this is a faithful reconstruction of the
    /// pre-fix behavior and not merely an approximation of it.
    ///
    /// NEW topology: each leg is pooled across all stores into one globally
    /// rank-ordered list (`pool_leg_results`), then fused once
    /// (`rrf_fuse_global`). Because ranks are now assigned over the pooled
    /// list, the weak store's only chunk is correctly ranked behind all
    /// three stronger chunks from the `rel` store. Both assertions coexist
    /// in this test so a future reader can see the bug and the fix
    /// side by side.
    #[test]
    fn query_multi_store_true_global_rrf_demotes_weak_stores_rank0_chunk() {
        // `rel`: three strong chunks, dense + BM25 scores strictly decreasing.
        let r0 = make_chunk(
            "r0",
            "doc-r0",
            "rel",
            "text r0",
            vec![],
            "file:///r0.md",
            vec![1.0],
        );
        let r1 = make_chunk(
            "r1",
            "doc-r1",
            "rel",
            "text r1",
            vec![],
            "file:///r1.md",
            vec![1.0],
        );
        let r2 = make_chunk(
            "r2",
            "doc-r2",
            "rel",
            "text r2",
            vec![],
            "file:///r2.md",
            vec![1.0],
        );
        // `weak`: one mediocre chunk, alone in its store — local rank-0 by
        // default, purely because it has no competition within its own store.
        let w0 = make_chunk(
            "w0",
            "doc-w0",
            "weak",
            "text w0",
            vec![],
            "file:///w0.md",
            vec![1.0],
        );

        // -------------------------------------------------------------
        // OLD topology: fuse per store, then merge (documents the bug).
        // -------------------------------------------------------------
        let rel_dense = vec![
            make_search_result(r0.clone(), 0.99),
            make_search_result(r1.clone(), 0.90),
            make_search_result(r2.clone(), 0.80),
        ];
        let rel_bm25 = vec![
            make_search_result(r0.clone(), 10.0),
            make_search_result(r1.clone(), 8.0),
            make_search_result(r2.clone(), 6.0),
        ];
        let weak_dense = vec![make_search_result(w0.clone(), 0.50)];
        let weak_bm25 = vec![make_search_result(w0.clone(), 3.0)];

        let rel_fused_old = rrf_fuse_global(&rel_dense, &rel_bm25, 60.0);
        let weak_fused_old = rrf_fuse_global(&weak_dense, &weak_bm25, 60.0);

        let rel_rank0_old = rel_fused_old.iter().find(|e| e.chunk.id == "r0").unwrap();
        let weak_rank0_old = weak_fused_old.iter().find(|e| e.chunk.id == "w0").unwrap();

        let expected_rank0_score = 2.0 / 61.0;
        assert!(
            (rel_rank0_old.fused_score - expected_rank0_score).abs() < 1e-10,
            "rel's rank-0 chunk should score exactly 2/61 under the old per-store topology, got {}",
            rel_rank0_old.fused_score
        );
        assert!(
            (weak_rank0_old.fused_score - expected_rank0_score).abs() < 1e-10,
            "weak's rank-0 chunk should score exactly 2/61 under the old per-store topology \
             (THIS IS THE BUG: a mediocre chunk alone in its store ties the genuinely-best \
             chunk from a store with real competition), got {}",
            weak_rank0_old.fused_score
        );

        // -------------------------------------------------------------
        // NEW topology: pool each leg globally, then a single rrf_fuse_global.
        // -------------------------------------------------------------
        let pooled_dense = pool_leg_results(vec![
            make_search_result(r0.clone(), 0.99),
            make_search_result(r1.clone(), 0.90),
            make_search_result(r2.clone(), 0.80),
            make_search_result(w0.clone(), 0.50),
        ]);
        let pooled_bm25 = pool_leg_results(vec![
            make_search_result(r0.clone(), 10.0),
            make_search_result(r1.clone(), 8.0),
            make_search_result(r2.clone(), 6.0),
            make_search_result(w0.clone(), 3.0),
        ]);

        let fused_new = rrf_fuse_global(&pooled_dense, &pooled_bm25, 60.0);
        assert_eq!(fused_new.len(), 4);

        // Weak's chunk must now be ranked strictly last.
        assert_eq!(
            fused_new[3].chunk.id,
            "w0",
            "weak's chunk must be demoted to last place under true global RRF, got order {:?}",
            fused_new
                .iter()
                .map(|e| e.chunk.id.as_str())
                .collect::<Vec<_>>()
        );

        let r0_entry = fused_new.iter().find(|e| e.chunk.id == "r0").unwrap();
        let r1_entry = fused_new.iter().find(|e| e.chunk.id == "r1").unwrap();
        let r2_entry = fused_new.iter().find(|e| e.chunk.id == "r2").unwrap();
        let w0_entry = fused_new.iter().find(|e| e.chunk.id == "w0").unwrap();

        assert!(
            (r0_entry.fused_score - 2.0 / 61.0).abs() < 1e-10,
            "r0 should be 2/61, got {}",
            r0_entry.fused_score
        );
        assert!(
            (r1_entry.fused_score - 2.0 / 62.0).abs() < 1e-10,
            "r1 should be 2/62, got {}",
            r1_entry.fused_score
        );
        assert!(
            (r2_entry.fused_score - 2.0 / 63.0).abs() < 1e-10,
            "r2 should be 2/63, got {}",
            r2_entry.fused_score
        );
        assert!(
            (w0_entry.fused_score - 2.0 / 64.0).abs() < 1e-10,
            "w0 should be 2/64 — correctly demoted behind all three rel chunks, got {}",
            w0_entry.fused_score
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
        assert_eq!(citation.resource_id, "doc-1");
        assert_eq!(citation.store.id, "store-A");
        assert_eq!(citation.store.name, "my-store");
        assert_eq!(citation.uri, "file:///docs/guide.md");
        assert_eq!(
            citation.heading_path,
            vec!["Overview".to_string(), "Details".to_string()]
        );
        assert_eq!(citation.location.span.start, 0);
        assert_eq!(citation.location.span.end, "The quick brown fox".len());
        assert!(
            citation.location.window_block_seqs.is_empty(),
            "non-window chunk should have empty window_block_seqs"
        );
        assert_eq!(citation.block.seq, 0);
        assert_eq!(citation.chunk_position.seq_in_block, 0);
        assert_eq!(citation.snippet, "The quick brown fox");
        assert!((citation.score.fused - 0.0327).abs() < 1e-10);
        assert_eq!(citation.score.dense, Some(0.92));
        assert_eq!(citation.score.bm25, Some(8.5));
        assert_eq!(citation.provenance.fetched_at, "2026-06-10T12:00:00Z");
        assert_eq!(citation.provenance.content_hash, "abc123");
    }

    /// `shape_citation` must thread `block_seq`/`seq_in_block`/`block_kind`/
    /// `window_block_seqs` from `ChunkRecord` into the nested
    /// `block`/`chunk_position`/`location` citation fields (specs/02
    /// §6): a message-window chunk with non-default values everywhere.
    #[test]
    fn shape_citation_carries_block_and_window_fields() {
        let mut chunk = make_chunk(
            "chunk-2",
            "doc-1",
            "store-A",
            "window chunk text",
            vec![],
            "file:///thread.md",
            vec![0.1, 0.2],
        );
        chunk.block_seq = 5;
        chunk.seq_in_block = 2;
        chunk.block_kind = Some("message".to_string());
        chunk.window_block_seqs = vec![3, 4, 5];

        let entry = FusedChunkEntry {
            chunk,
            fused_score: 0.01,
            dense_score: None,
            bm25_score: Some(1.0),
        };

        let citation = shape_citation(entry, "store-A".to_string(), "my-store".to_string());

        assert_eq!(citation.block.seq, 5);
        assert_eq!(citation.block.kind, Some("message".to_string()));
        assert_eq!(citation.chunk_position.seq_in_block, 2);
        assert_eq!(citation.location.window_block_seqs, vec![3, 4, 5]);
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
        assert!(v.get("resource_id").is_some());
        assert!(v.get("store").is_some());
        assert!(v.get("uri").is_some());
        assert!(v.get("heading_path").is_some());
        assert!(v.get("block").is_some());
        assert!(v.get("chunk_position").is_some());
        assert!(v.get("location").is_some());
        assert!(v.get("snippet").is_some());
        assert!(v.get("score").is_some());
        assert!(v.get("provenance").is_some());
        assert!(v["score"].get("fused").is_some());
        assert!(v["score"].get("dense").is_some());
        assert!(v["score"].get("bm25").is_some());
        assert!(v["block"].get("seq").is_some());
        assert!(v["chunk_position"].get("seq_in_block").is_some());
        assert!(v["location"]["span"].get("start").is_some());
        assert!(v["location"]["span"].get("end").is_some());
    }

    #[test]
    fn shape_citation_carries_metadata() {
        let mut chunk = make_chunk("c1", "d1", "s1", "text", vec![], "file:///a.md", vec![1.0]);
        chunk.metadata = crate::metadata::Metadata::Document(crate::metadata::DocumentMetadata {
            dublin_core: crate::metadata::DublinCoreMetadata {
                title: Some("My Title".to_string()),
                creator: vec!["Bob".to_string()],
                date: Some("2026-03-01".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
        let entry = FusedChunkEntry {
            chunk,
            fused_score: 0.5,
            dense_score: None,
            bm25_score: Some(4.0),
        };
        let citation = shape_citation(entry, "s1".to_string(), "store-one".to_string());
        assert_eq!(citation.metadata.title(), Some("My Title"));
        assert_eq!(
            citation.metadata.dublin_core().creator,
            vec!["Bob".to_string()]
        );
        assert_eq!(
            citation.metadata.dublin_core().date.as_deref(),
            Some("2026-03-01")
        );
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
        //
        // Discriminating fixture (issue #162): this fixture is deliberately
        // asymmetric in BM25-leg presence so it can prove a cross-store score
        // inequality without depending on `FakeEmbedder`'s hash internals (see
        // `core/src/embedder.rs`). Query "rust programming" is 2 terms; per
        // `simple_bm25_score` (`core/src/store.rs`), chunk a1's text contains
        // both terms, so it always scores 2/2 = 1.0 — the uniquely highest BM25
        // score among these three chunks, i.e. always BM25 rank 0 (contribution
        // exactly 1/61). Chunk b1's text contains neither term, so its BM25
        // score is 0.0 and `FakeStore::bm25_search` filters it out of the BM25
        // leg entirely (contribution 0). Both chunks are always present in the
        // dense leg (cosine similarity, no score filter), at *some* rank among
        // the 3 pooled candidates — worst case rank 2 (1/63), best case rank 0
        // (1/61). That gives a1 a fused-score floor of 1/61 + 1/63 and b1 a
        // fused-score ceiling of 1/61; since 1/61 + 1/63 > 1/61, a1 must
        // strictly outscore b1 regardless of how the dense leg (hash-based)
        // happens to rank them. Exact RRF arithmetic coverage lives in
        // `query_multi_store_true_global_rrf_demotes_weak_stores_rank0_chunk`;
        // this test only needs the inequality direction plus the ordering and
        // candidate-count invariants below.
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

        // All three chunks (a1, b1, b2) are distinct (store_id, chunk_id) keys,
        // and every one of them appears in at least one leg (a1 and b2 in both
        // legs, b1 in the dense leg only) — global fusion must therefore
        // produce exactly 3 candidates.
        assert_eq!(
            result.total_candidates, 3,
            "expected exactly 3 globally-fused candidates (a1, b1, b2)"
        );

        // Results should be ordered by fused score descending
        for i in 0..result.citations.len().saturating_sub(1) {
            assert!(
                result.citations[i].score.fused >= result.citations[i + 1].score.fused,
                "citations should be ordered by fused score descending"
            );
        }

        // Cross-store score inequality (see the fixture comment above for the
        // bound derivation): a1's guaranteed BM25-rank-0 contribution plus its
        // worst-case dense contribution must exceed b1's best-case dense-only
        // contribution, regardless of dense-leg (hash-based) ranking.
        let a1 = result
            .citations
            .iter()
            .find(|c| c.chunk_id == "a1")
            .expect("a1 should be present (dense leg always includes it)");
        let b1 = result
            .citations
            .iter()
            .find(|c| c.chunk_id == "b1")
            .expect("b1 should be present (dense leg always includes it)");
        assert!(
            a1.score.fused > b1.score.fused,
            "a1 (2/2 BM25 term match, always BM25 rank 0) must strictly outscore b1 \
             (0/2 BM25 term match, absent from the BM25 leg entirely): a1={}, b1={}",
            a1.score.fused,
            b1.score.fused
        );
    }

    /// The same chunk_id present in two different stores (e.g. the same
    /// document indexed into both) must survive as two distinct citations,
    /// each correctly attributed to its own store. Each chunk's `store_id`
    /// equals the `StoreHandle.id` of the store holding it — the
    /// implementation must resolve each citation's store name from the
    /// chunk's `store_id`, not from positional pairing with the fan-out loop
    /// (which pooling across stores makes unreliable).
    #[tokio::test]
    async fn query_same_chunk_id_present_in_two_stores_both_survive_with_correct_attribution() {
        let embedder = FakeEmbedder::new(4);

        let text = "shared content across stores";
        let embedding = embed_text(&embedder, text).await;

        let store_a = FakeStore::new();
        let chunk_a = make_chunk(
            "shared-chunk-id",
            "doc-a",
            "store-A",
            text,
            vec![],
            "file:///a.md",
            embedding.clone(),
        );
        store_a.upsert_chunks(vec![chunk_a]).await.unwrap();

        let store_b = FakeStore::new();
        let chunk_b = make_chunk(
            "shared-chunk-id",
            "doc-b",
            "store-B",
            text,
            vec![],
            "file:///b.md",
            embedding,
        );
        store_b.upsert_chunks(vec![chunk_b]).await.unwrap();

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
            query: "shared content".to_string(),
            leg_k: Some(10),
            top_n: Some(10),
            filters: vec![],
        };

        let result = SearchOrchestrator::query(&handles, &embedder, &request)
            .await
            .unwrap();

        assert_eq!(
            result.total_candidates, 2,
            "both stores' chunks should be counted as distinct candidates, not deduped away"
        );
        assert_eq!(
            result.citations.len(),
            2,
            "both citations should survive — same chunk_id in different stores must not collide"
        );

        let store_ids: std::collections::HashSet<&str> = result
            .citations
            .iter()
            .map(|c| c.store.id.as_str())
            .collect();
        assert_eq!(
            store_ids,
            std::collections::HashSet::from(["store-A", "store-B"]),
            "citations must carry the two distinct store ids, one each"
        );

        for c in &result.citations {
            assert_eq!(c.chunk_id, "shared-chunk-id");
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

        assert_eq!(c.location.span.start, 42, "span.start should be preserved");
        assert_eq!(c.location.span.end, 64, "span.end should be preserved");
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
