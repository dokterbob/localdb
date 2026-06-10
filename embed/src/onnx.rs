//! Local ONNX embedding provider via fastembed.
//!
//! Uses fastembed-rs for in-process inference with the ONNX Runtime.
//! No network calls during inference; models are downloaded on first use.
//!
//! # Supported models
//!
//! | Role | Model | Dim | Notes |
//! |------|-------|-----|-------|
//! | Default | `bge-small-en-v1.5` | 384 | Reliable, fast, good quality for English |
//! | Lightweight fallback | `bge-small-en-v1.5` | 384 | Same model used as fallback for weak hardware |
//!
//! Note: `pplx-embed-context-v1-0.6b` is the spec's headline model pending benchmark
//! (see specs/04-search-pipeline.md §4). When it becomes available in fastembed, the
//! `ModelChoice::Default` will be updated. For now, `bge-small-en-v1.5` is used as
//! the default and for CI tests.
//!
//! # ONNX Runtime
//!
//! This module uses fastembed which wraps `ort` (ONNX Runtime Rust bindings).
//! The ONNX Runtime binary is downloaded automatically by the `ort-download-binaries-native-tls`
//! feature of fastembed.
//!
//! # Model cache
//!
//! fastembed handles its own model cache via the HuggingFace Hub. The cache directory
//! can be configured via the `cache_dir` parameter.

use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use localdb_core::{DocumentChunks, EmbeddedDocument, Embedder, Error as CoreError};
use tracing::info;

use crate::error::EmbedError;

/// Model choice for the local ONNX embedder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelChoice {
    /// Default model: BGE Small EN v1.5 (384 dims, English, fast).
    ///
    /// Will be replaced by `pplx-embed-context-v1-0.6b` pending benchmark
    /// (see specs/04-search-pipeline.md §4).
    BgeSmallEnV15,
    /// Alias for the current default (same as `BgeSmallEnV15` for now).
    Default,
}

impl ModelChoice {
    /// Convert to the fastembed `EmbeddingModel` variant.
    pub fn to_fastembed_model(self) -> EmbeddingModel {
        match self {
            ModelChoice::BgeSmallEnV15 | ModelChoice::Default => EmbeddingModel::BGESmallENV15,
        }
    }

    /// Expected embedding dimension for this model.
    pub fn embedding_dim(self) -> usize {
        match self {
            ModelChoice::BgeSmallEnV15 | ModelChoice::Default => 384,
        }
    }

    /// Model ID string used in policy versioning.
    pub fn model_id(self) -> &'static str {
        match self {
            ModelChoice::BgeSmallEnV15 | ModelChoice::Default => "bge-small-en-v1.5",
        }
    }
}

/// Local ONNX embedding provider.
///
/// Runs inference in-process via fastembed and the ONNX Runtime.
/// Inference happens on a blocking thread pool (not the async executor).
///
/// See specs/04-search-pipeline.md §4.
pub struct OnnxEmbedder {
    /// The underlying fastembed model (mutable because `embed` requires `&mut self`).
    model: Mutex<TextEmbedding>,
    model_choice: ModelChoice,
}

impl OnnxEmbedder {
    /// Create a new ONNX embedder with the given model, downloading if needed.
    ///
    /// # Arguments
    /// * `model_choice` - Which model to use
    /// * `cache_dir` - Optional override for the model cache directory.
    ///   Defaults to the fastembed default (XDG/Platform cache + `.fastembed_cache`)
    /// * `show_download_progress` - Whether to show a progress bar during model download
    ///
    /// # Errors
    /// Returns `EmbedError::ModelMissing` if the model cannot be loaded and no download is possible.
    pub fn new(
        model_choice: ModelChoice,
        cache_dir: Option<PathBuf>,
        show_download_progress: bool,
    ) -> Result<Self, EmbedError> {
        info!(
            model = model_choice.model_id(),
            "loading ONNX embedding model"
        );

        let fastembed_model = model_choice.to_fastembed_model();

        let mut opts = TextInitOptions::new(fastembed_model)
            .with_show_download_progress(show_download_progress);

        if let Some(dir) = cache_dir {
            opts = opts.with_cache_dir(dir);
        }

        let model = TextEmbedding::try_new(opts).map_err(|e| {
            EmbedError::ModelMissing(format!(
                "failed to load ONNX model '{}': {e}. \
                 Run `localdb init` to download models, or ensure the model cache is populated.",
                model_choice.model_id()
            ))
        })?;

        info!(
            model = model_choice.model_id(),
            "ONNX model loaded successfully"
        );
        Ok(Self {
            model: Mutex::new(model),
            model_choice,
        })
    }

    /// Embed a batch of texts synchronously (for use in blocking contexts).
    fn embed_sync(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let mut model = self
            .model
            .lock()
            .map_err(|e| EmbedError::Internal(format!("model mutex poisoned: {e}")))?;
        let embeddings = model
            .embed(texts, None)
            .map_err(|e| EmbedError::Internal(format!("ONNX inference error: {e}")))?;
        Ok(embeddings)
    }
}

#[async_trait]
impl Embedder for OnnxEmbedder {
    /// Embed all chunks for a batch of documents.
    ///
    /// Note: for local ONNX, document context is currently not used
    /// (classic per-chunk embedding). Contextualized embedding via
    /// `pplx-embed-context-v1-0.6b` will use document context when available.
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, CoreError> {
        if docs.is_empty() {
            return Ok(vec![]);
        }

        // Flatten all chunks from all docs
        let mut all_chunks: Vec<String> = Vec::new();
        let mut doc_offsets: Vec<(usize, usize)> = Vec::new();

        for doc in &docs {
            let start = all_chunks.len();
            all_chunks.extend(doc.chunks.iter().cloned());
            doc_offsets.push((start, doc.chunks.len()));
        }

        if all_chunks.is_empty() {
            return Ok(docs.iter().map(|_| vec![]).collect());
        }

        // Run inference on the blocking pool (ONNX is CPU-bound).
        // Use block_in_place to call the sync method from async context.
        // This is valid on multi-thread tokio runtimes.
        let all_embeddings: Vec<Vec<f32>> =
            tokio::task::block_in_place(|| self.embed_sync(all_chunks).map_err(CoreError::from))?;

        // Re-group by document
        let result = doc_offsets
            .into_iter()
            .map(|(start, len)| all_embeddings[start..start + len].to_vec())
            .collect();

        Ok(result)
    }

    fn embedding_dim(&self) -> usize {
        self.model_choice.embedding_dim()
    }

    fn model_id(&self) -> &str {
        self.model_choice.model_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::{DocumentChunks, Embedder};
    use tempfile::TempDir;

    /// Helper: create an ONNX embedder with a temp cache dir.
    ///
    /// Downloads the model if not already cached.
    fn make_embedder() -> (TempDir, OnnxEmbedder) {
        let dir = TempDir::new().unwrap();
        let embedder = OnnxEmbedder::new(
            ModelChoice::BgeSmallEnV15,
            Some(dir.path().to_path_buf()),
            false,
        )
        .expect("ONNX embedder should load BGE Small model");
        (dir, embedder)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onnx_embedder_returns_correct_dim() {
        let (_dir, embedder) = make_embedder();
        assert_eq!(embedder.embedding_dim(), 384, "BGE Small EN has 384 dims");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onnx_embedder_returns_correct_shape() {
        let (_dir, embedder) = make_embedder();

        let docs = vec![DocumentChunks {
            document_context: "Test document about Rust programming".to_string(),
            chunks: vec![
                "Rust is a systems programming language.".to_string(),
                "It provides memory safety without garbage collection.".to_string(),
            ],
        }];

        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 1, "one document");
        assert_eq!(result[0].len(), 2, "two chunks");
        assert_eq!(result[0][0].len(), 384, "BGE Small EN dim = 384");
        assert_eq!(result[0][1].len(), 384, "BGE Small EN dim = 384");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onnx_embedder_is_deterministic() {
        let (_dir, embedder) = make_embedder();

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["determinism test string".to_string()],
        }];

        let r1 = embedder.embed_documents(docs.clone()).await.unwrap();
        let r2 = embedder.embed_documents(docs).await.unwrap();

        assert_eq!(r1[0][0], r2[0][0], "ONNX embedder must be deterministic");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onnx_embedder_distinct_texts_produce_distinct_vectors() {
        let (_dir, embedder) = make_embedder();

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec![
                "The quick brown fox".to_string(),
                "A completely different sentence about cats".to_string(),
            ],
        }];

        let result = embedder.embed_documents(docs).await.unwrap();
        assert_ne!(
            result[0][0], result[0][1],
            "distinct texts must produce distinct vectors"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onnx_embedder_similar_texts_are_closer() {
        let (_dir, embedder) = make_embedder();

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec![
                "The quick brown fox jumps over the lazy dog".to_string(),
                "A quick brown fox jumped over a lazy dog".to_string(), // similar
                "Machine learning is a subset of artificial intelligence".to_string(), // different topic
            ],
        }];

        let result = embedder.embed_documents(docs).await.unwrap();
        let v1 = &result[0][0];
        let v2 = &result[0][1]; // similar
        let v3 = &result[0][2]; // different

        let cosine_sim = |a: &[f32], b: &[f32]| -> f32 {
            let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
            let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            if na * nb < 1e-9 {
                0.0
            } else {
                dot / (na * nb)
            }
        };

        let sim_12 = cosine_sim(v1, v2);
        let sim_13 = cosine_sim(v1, v3);

        assert!(
            sim_12 > sim_13,
            "similar text pairs should have higher cosine similarity: sim_12={sim_12:.4}, sim_13={sim_13:.4}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onnx_embedder_empty_docs() {
        let (_dir, embedder) = make_embedder();
        let result = embedder.embed_documents(vec![]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onnx_embedder_multi_doc() {
        let (_dir, embedder) = make_embedder();

        let docs = vec![
            DocumentChunks {
                document_context: "doc1".to_string(),
                chunks: vec!["first chunk".to_string(), "second chunk".to_string()],
            },
            DocumentChunks {
                document_context: "doc2".to_string(),
                chunks: vec!["third chunk".to_string()],
            },
        ];

        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 2);
        assert_eq!(result[1].len(), 1);
        assert_eq!(result[0][0].len(), 384);
    }

    #[test]
    fn model_choice_dim() {
        assert_eq!(ModelChoice::BgeSmallEnV15.embedding_dim(), 384);
        assert_eq!(ModelChoice::Default.embedding_dim(), 384);
    }

    #[test]
    fn model_choice_id() {
        assert_eq!(ModelChoice::BgeSmallEnV15.model_id(), "bge-small-en-v1.5");
        assert_eq!(ModelChoice::Default.model_id(), "bge-small-en-v1.5");
    }
}
