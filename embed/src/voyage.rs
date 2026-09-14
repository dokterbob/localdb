//! Voyage contextualized embedding provider.
//!
//! Uses Voyage AI's `voyage-context-3` model endpoint, which supports contextualized
//! embeddings by accepting a document alongside the chunks.
//!
//! # API shape (Voyage AI contextual embeddings)
//!
//! Request to `https://api.voyageai.com/v1/contextual_embeddings`:
//! ```json
//! {
//!   "model": "voyage-context-3",
//!   "document": "full document text",
//!   "input": ["chunk 1", "chunk 2", ...]
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

/// Request body for Voyage `/v1/contextual_embeddings`.
#[derive(Debug, Serialize)]
struct VoyageEmbedRequest<'a> {
    model: &'a str,
    document: &'a str,
    input: &'a [String],
}

/// Marker type carrying Voyage's half of [`ContextualProviderSpec`].
pub struct Voyage;

impl ContextualProviderSpec for Voyage {
    const DEFAULT_BASE_URL: &'static str = "https://api.voyageai.com";
    const DEFAULT_MODEL: &'static str = "voyage-context-3";
    const DEFAULT_DIM: usize = 1024;
    const PATH: &'static str = "/v1/contextual_embeddings";
    const NAME: &'static str = "voyage";

    fn request<'a>(
        model: &'a str,
        document: &'a str,
        chunks: &'a [String],
    ) -> impl Serialize + Send + 'a {
        VoyageEmbedRequest {
            model,
            document,
            input: chunks,
        }
    }
}

/// Voyage contextualized embedding provider.
///
/// Uses the `voyage-context-3` model with document-level context.
pub type VoyageEmbedder = ContextualEmbedder<Voyage>;

// `"input"` is Voyage's chunk-list field, and the only wire-level difference
// from Perplexity — swapping the two request structs above would leave every
// other case in the suite green, so it is asserted here explicitly.
#[cfg(test)]
crate::contextual::contextual_provider_tests!(Voyage, "input");
