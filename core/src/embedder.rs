//! The `Embedder` trait — document-aware embedding interface.
//!
//! The trait receives chunks **grouped by document**, with the document context
//! (nested chunks-per-document). This is the day-one document-aware shape
//! required by contextualized/late-chunking models.
//!
//! Classic per-chunk embedding is the degenerate case (context ignored,
//! one chunk per call batch).
//!
//! See specs/04-search-pipeline.md §4.

use std::sync::Arc;

use crate::error::Error;

/// Describes how embedding vectors should be stored in the backend.
///
/// The embedder signals the encoding; the store binarizes at index time for `Binary`.
/// `FakeEmbedder` always returns `Float32`; pplx local-ONNX models return `Binary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VectorEncoding {
    /// Raw 32-bit float vectors. Default for all embedders.
    #[default]
    Float32,
    /// Binary-quantized: `(x ≥ 0.0)` → 1, packed MSB-first into bytes.
    /// A 1024-dim vector becomes 128 bytes/vector.
    Binary,
}

/// A token-counting closure: maps a text to its token count.
///
/// Returned by [`Embedder::token_counter`] when the embedder has a local
/// tokenizer, and consumed by the chunker as a `TokenSizer`.
pub type TokenCounter = Arc<dyn Fn(&str) -> usize + Send + Sync>;

/// One document's worth of chunks for embedding.
///
/// The `document_context` string is the full document text (or a summary/title),
/// used by contextualized models to embed each chunk in context.
/// Classic embedders may ignore it.
#[derive(Debug, Clone)]
pub struct DocumentChunks {
    /// Full document text (or representative context) for contextualized embedding.
    /// May be ignored by classic per-chunk embedders.
    pub document_context: String,
    /// The chunk texts to embed. Order is preserved in the output.
    pub chunks: Vec<String>,
}

/// Result of embedding one document's chunks.
///
/// One inner `Vec<f32>` per chunk, in the same order as the input.
pub type EmbeddedDocument = Vec<Vec<f32>>;

/// The document-aware embedding trait.
///
/// Receives chunks grouped by document with document context.
/// Returns one embedding vector per chunk, grouped by document.
///
/// ```text
/// embed_documents(docs: [{document_context, chunks: [chunk_text, ...]}, ...])
///     -> [[vector, ...], ...]
/// ```
///
/// Implementations:
/// - Local ONNX: `embed` crate, uses fastembed-class models
/// - OpenAI-compatible: `embed` crate, flat (context-free) HTTP provider
/// - Perplexity contextualized: `embed` crate, uses document context
/// - Voyage contextualized: `embed` crate, uses document context
///
/// See specs/04-search-pipeline.md §4.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// Embed all chunks for a batch of documents.
    ///
    /// Returns one `EmbeddedDocument` per input document, each containing
    /// one vector per chunk in the same order as the input chunks.
    ///
    /// # Errors
    /// - `Error::ModelMissing` — local model not in cache and download disabled
    /// - `Error::ProviderUnavailable` — hosted provider unreachable/rate-limited after retries
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, Error>;

    /// Return the dimensionality of the embedding vectors this embedder produces.
    fn embedding_dim(&self) -> usize;

    /// Return a human-readable name for the model/provider (for logging and policy versioning).
    fn model_id(&self) -> &str;

    /// Describes how vectors from this embedder should be stored in the backend.
    ///
    /// Most embedders return `Float32`. Perplexity local-ONNX models return `Binary` —
    /// the store binarizes at `(x ≥ 0.0)` and packs MSB-first at index time.
    fn vector_encoding(&self) -> VectorEncoding {
        VectorEncoding::Float32
    }

    /// Return a token-counting function if this embedder has a local tokenizer.
    ///
    /// When present, the ingestion pipeline uses it to size chunks in tokens
    /// (token-accurate chunking). Embedders without a local tokenizer (hosted
    /// providers, `FakeEmbedder`) return `None`, and the pipeline falls back to
    /// character-based sizing.
    fn token_counter(&self) -> Option<TokenCounter> {
        None
    }
}

/// A deterministic fake embedder for testing.
///
/// Produces fixed-length vectors derived from a simple hash of the input text,
/// guaranteeing determinism and uniqueness for distinct inputs.
pub struct FakeEmbedder {
    /// Embedding dimension.
    dim: usize,
    /// Model ID string.
    model: String,
}

impl FakeEmbedder {
    /// Create a new fake embedder with the given dimension.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            model: format!("fake-embedder-dim{dim}"),
        }
    }

    /// Derive a fake embedding for a single text.
    ///
    /// Uses a simple rolling hash to produce distinct, deterministic vectors.
    fn embed_text(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0.0f32; self.dim];
        let bytes = text.as_bytes();
        for (i, slot) in vec.iter_mut().enumerate() {
            // Simple but deterministic: mix position with byte values using wrapping arithmetic
            let byte_sum: u64 = bytes
                .iter()
                .enumerate()
                .map(|(j, &b)| {
                    (b as u64)
                        .wrapping_mul((j as u64).wrapping_add(1))
                        .wrapping_mul((i as u64).wrapping_add(7))
                })
                .fold(0u64, u64::wrapping_add);
            *slot = (byte_sum.wrapping_add((i as u64).wrapping_mul(0x9e3779b97f4a7c15)) as f32)
                / u64::MAX as f32;
        }
        // L2-normalize
        let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-9 {
            for x in vec.iter_mut() {
                *x /= norm;
            }
        }
        vec
    }
}

#[async_trait::async_trait]
impl Embedder for FakeEmbedder {
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, Error> {
        let result = docs
            .iter()
            .map(|doc| {
                doc.chunks
                    .iter()
                    .map(|chunk| self.embed_text(chunk))
                    .collect()
            })
            .collect();
        Ok(result)
    }

    fn embedding_dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Failing tests first (TDD) ---

    #[tokio::test]
    async fn fake_embedder_returns_correct_shape() {
        let embedder = FakeEmbedder::new(128);
        let docs = vec![
            DocumentChunks {
                document_context: "Document about Rust programming".to_string(),
                chunks: vec!["Rust is fast.".to_string(), "Rust is safe.".to_string()],
            },
            DocumentChunks {
                document_context: "Document about Python".to_string(),
                chunks: vec!["Python is dynamic.".to_string()],
            },
        ];
        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 2, "one EmbeddedDocument per input doc");
        assert_eq!(result[0].len(), 2, "two chunks in first doc");
        assert_eq!(result[1].len(), 1, "one chunk in second doc");
        assert_eq!(result[0][0].len(), 128, "vector dim = 128");
        assert_eq!(result[1][0].len(), 128, "vector dim = 128");
    }

    #[tokio::test]
    async fn fake_embedder_is_deterministic() {
        let embedder = FakeEmbedder::new(64);
        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["hello world".to_string()],
        }];
        let r1 = embedder.embed_documents(docs.clone()).await.unwrap();
        let r2 = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(r1, r2, "fake embedder must be deterministic");
    }

    #[tokio::test]
    async fn fake_embedder_distinct_texts_produce_distinct_vectors() {
        let embedder = FakeEmbedder::new(64);
        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["hello world".to_string(), "goodbye world".to_string()],
        }];
        let result = embedder.embed_documents(docs).await.unwrap();
        assert_ne!(
            result[0][0], result[0][1],
            "distinct texts must produce distinct vectors"
        );
    }

    #[tokio::test]
    async fn fake_embedder_empty_docs_returns_empty() {
        let embedder = FakeEmbedder::new(32);
        let result = embedder.embed_documents(vec![]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn fake_embedder_empty_chunks_returns_empty_vectors() {
        let embedder = FakeEmbedder::new(32);
        let docs = vec![DocumentChunks {
            document_context: "doc".to_string(),
            chunks: vec![],
        }];
        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].is_empty());
    }

    #[test]
    fn fake_embedder_dim_accessor() {
        let embedder = FakeEmbedder::new(256);
        assert_eq!(embedder.embedding_dim(), 256);
    }

    #[test]
    fn fake_embedder_model_id_contains_dim() {
        let embedder = FakeEmbedder::new(128);
        assert!(
            embedder.model_id().contains("128"),
            "model_id should mention dimension"
        );
    }

    #[tokio::test]
    async fn degenerate_single_chunk_per_doc_works() {
        // Classic per-chunk embedding is the degenerate case
        let embedder = FakeEmbedder::new(16);
        let docs = vec![
            DocumentChunks {
                document_context: "".to_string(),
                chunks: vec!["chunk one".to_string()],
            },
            DocumentChunks {
                document_context: "".to_string(),
                chunks: vec!["chunk two".to_string()],
            },
        ];
        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 1);
        assert_eq!(result[1].len(), 1);
        // Different inputs → different embeddings
        assert_ne!(result[0][0], result[1][0]);
    }

    #[tokio::test]
    async fn fake_embedder_vectors_are_normalized() {
        let embedder = FakeEmbedder::new(32);
        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["normalize me".to_string()],
        }];
        let result = embedder.embed_documents(docs).await.unwrap();
        let v = &result[0][0];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "vector should be L2-normalized, got norm={norm}"
        );
    }
}
