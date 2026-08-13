//! Perplexity contextualized embedding provider.
//!
//! Uses Perplexity's `/v1/contextualizedembeddings` endpoint, which accepts a document
//! context and a list of chunks, returning contextualized embeddings for each chunk.
//!
//! The API key is read from the environment variable specified in config (`api_key_env`).
//!
//! # API shape (as documented by Perplexity)
//!
//! Request:
//! ```json
//! {
//!   "model": "pplx-embed-context-v1",
//!   "document": "full document text",
//!   "chunks": ["chunk 1", "chunk 2", ...]
//! }
//! ```
//!
//! Response:
//! ```json
//! {
//!   "data": [
//!     {"embedding": [0.1, ...], "index": 0},
//!     ...
//!   ]
//! }
//! ```
//!
//! Everything but the constants and the request body below is shared with the
//! other contextualized provider — see [`crate::contextual`].
//!
//! See specs/04-search-pipeline.md §4, specs/03-config.md §6.

use serde::Serialize;

use crate::contextual::{ContextualEmbedder, ContextualProviderSpec};

/// Request body for Perplexity `/v1/contextualizedembeddings`.
#[derive(Debug, Serialize)]
struct PerplexityEmbedRequest<'a> {
    model: &'a str,
    document: &'a str,
    chunks: &'a [String],
}

/// Marker type carrying Perplexity's half of [`ContextualProviderSpec`].
pub struct Perplexity;

impl ContextualProviderSpec for Perplexity {
    const DEFAULT_BASE_URL: &'static str = "https://api.perplexity.ai";
    const DEFAULT_MODEL: &'static str = "pplx-embed-context-v1";
    const DEFAULT_DIM: usize = 768;
    const PATH: &'static str = "/v1/contextualizedembeddings";
    const NAME: &'static str = "perplexity";

    fn request<'a>(
        model: &'a str,
        document: &'a str,
        chunks: &'a [String],
    ) -> impl Serialize + Send + 'a {
        PerplexityEmbedRequest {
            model,
            document,
            chunks,
        }
    }
}

/// Perplexity contextualized embedding provider.
///
/// Uses document context for each document's chunks. The document context
/// (full text or summary) is passed alongside the chunk list, giving the model
/// the broader context needed for late/contextualized chunking.
pub type PerplexityEmbedder = ContextualEmbedder<Perplexity>;

// `"chunks"` is Perplexity's chunk-list field, and the only wire-level
// difference from Voyage — swapping the two request structs above would leave
// every other case in the suite green, so it is asserted here explicitly.
#[cfg(test)]
crate::contextual::contextual_provider_tests!(Perplexity, "chunks");
