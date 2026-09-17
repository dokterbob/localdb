//! Hybrid search & citations — T08.
//!
//! Implements query orchestration: BM25 leg + dense leg (query embedding via Embedder),
//! RRF fusion (k=60, K=50 per leg), multi-store fan-out, metadata/store filters, and
//! result shaping to Citation objects with per-leg scores.
//!
//! Multi-store fusion topology: each leg's per-store results are
//! pooled into one globally rank-ordered list (`pool_leg_results`), and a
//! *single* RRF pass (`rrf_fuse_global`) runs over the two pooled legs, keyed
//! on the composite `(store_id, chunk_id)`. Fusing per-store and merging
//! already-fused scores would be wrong: RRF scores are rank-based and
//! scale-free, so every store's local rank-0 chunk would tie at the same
//! score regardless of how weak it actually is relative to other stores'
//! candidates.
//!
//! A no-op rerank seam is left between fusion and exact passage grouping.
//!
//! See specs/04-search-pipeline.md §5 and specs/02-domain-model.md §6.

use std::collections::{HashMap, HashSet};

mod grouping;
use grouping::group_candidates;
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

/// Exact passage grouping mode shared by search surfaces.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchDedup {
    /// Return independently ranked occurrences.
    Off,
    /// Group equal stored text.
    Text,
    /// Group equal stored text or eligible equal stored vectors.
    #[default]
    TextAndVector,
}

impl std::str::FromStr for SearchDedup {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "text" => Ok(Self::Text),
            "text_and_vector" => Ok(Self::TextAndVector),
            _ => Err("dedup must be off, text, or text_and_vector".to_string()),
        }
    }
}

impl std::fmt::Display for SearchDedup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Text => "text",
            Self::TextAndVector => "text_and_vector",
        })
    }
}

/// Query request for the search orchestrator.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    /// Exact passage grouping mode.
    pub dedup: SearchDedup,
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
    /// Distinct fused `(store, chunk)` occurrences in the retrieved pool,
    /// before reranking, grouping, or truncation.
    pub total_candidates: usize,
    /// Number of representatives before truncation.
    pub total_results: usize,
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
    /// Internal exact stored-vector identity, if eligible.
    pub embedding_identity: Option<crate::store::StoredEmbeddingIdentity>,
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
    conflicts: &mut HashSet<FusionKey>,
    k: f64,
    leg: Leg,
) {
    for (rank, result) in results.iter().enumerate() {
        let contribution = rrf_score(rank, k);
        let key = fusion_key(&result.chunk);
        let entry = entries
            .entry(key.clone())
            .or_insert_with(|| FusedChunkEntry {
                embedding_identity: None,
                chunk: result.chunk.clone(),
                fused_score: 0.0,
                dense_score: None,
                bm25_score: None,
            });

        if !conflicts.contains(&key) {
            match (&entry.embedding_identity, &result.embedding_identity) {
                (Some(existing), Some(incoming)) if existing != incoming => {
                    entry.embedding_identity = None;
                    conflicts.insert(key);
                }
                (None, Some(incoming)) => entry.embedding_identity = Some(incoming.clone()),
                _ => {}
            }
        }
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

    let mut conflicts = HashSet::new();
    add_leg(&mut entries, dense_results, &mut conflicts, k, Leg::Dense);
    add_leg(&mut entries, bm25_results, &mut conflicts, k, Leg::Bm25);

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
/// Per spec: "explicitly post-MVP". The pipeline calls this between fusion and grouping.
pub fn rerank_noop(results: Vec<FusedChunkEntry>) -> Vec<FusedChunkEntry> {
    results
}

// ---------------------------------------------------------------------------
// Citation shaping
// ---------------------------------------------------------------------------

/// Shape a fused result into a `Citation`.
pub fn shape_citation(fused: FusedChunkEntry, store_id: String, store_name: String) -> Citation {
    Citation {
        duplicates: vec![],
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
/// 5. Group exact passages, take top-N representatives, and shape compact citations.
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
                total_results: 0,
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
                total_results: 0,
            });
        }

        // 4. Rerank seam (no-op) — operates directly on Vec<FusedChunkEntry>;
        //    store attribution lives in entry.chunk.store_id, so this seam
        //    survives a future reranker that reorders or drops entries.
        let reranked = rerank_noop(fused);

        // 5. Group the complete retrieved pool before limiting representatives.
        let groups = group_candidates(reranked, request.dedup);
        let total_results = groups.len();
        let shape = |entry: FusedChunkEntry| {
            let store_id = entry.chunk.store_id.clone();
            let store_name = store_names.get(&store_id).cloned().unwrap_or_default();
            shape_citation(entry, store_id, store_name)
        };
        let citations = groups
            .into_iter()
            .take(top_n)
            .map(|group| {
                let representative = shape(group.representative);
                representative.with_duplicates(
                    group
                        .duplicates
                        .into_iter()
                        .map(|(entry, reasons)| (shape(entry), reasons)),
                )
            })
            .collect();

        Ok(QueryResponse {
            citations,
            total_candidates,
            total_results,
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
mod tests;
