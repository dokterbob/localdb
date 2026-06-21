//! Embedder implementations for localdb.
//!
//! # Providers
//!
//! - **Local ONNX** (`OnnxEmbedder`, feature `local-onnx`): runs models in-process via
//!   fastembed/ONNX Runtime. Default model: `bge-small-en-v1.5` (384 dims). Downloads and
//!   caches models on first use.
//! - **OpenAI-compatible** (`OpenAiEmbedder`): flat (context-free) HTTP provider targeting any
//!   `/v1/embeddings`-compatible endpoint (OpenAI, Ollama, etc.).
//! - **Perplexity** (`PerplexityEmbedder`): contextualized provider using
//!   `/v1/contextualizedembeddings`. Passes document context + chunks.
//! - **Voyage** (`VoyageEmbedder`): contextualized provider using `voyage-context-3`. Passes
//!   document context + chunks.
//!
//! # Batching, retry, and timeout policy
//!
//! Hosted providers use sensible defaults documented in [`retry`]:
//! - Batch size: 32 chunks per request (configurable)
//! - Timeout: 30 s per request
//! - Retries: 3 attempts with exponential back-off (1 s, 2 s, 4 s)
//! - Retry on: network errors, 429 Too Many Requests, 5xx errors
//!
//! # Model cache
//!
//! Local models are cached in the platform model cache directory (from config `paths.models`).
//! Download is resumable; integrity is verified with SHA-256. When downloads are disabled and
//! the cache is empty, `Error::ModelMissing` is raised with an actionable message.
//!
//! See specs/04-search-pipeline.md §4.

pub mod error;
pub mod factory;
pub mod model_cache;
pub mod openai;
pub mod perplexity;
pub mod retry;
pub mod voyage;

#[cfg(feature = "local-onnx")]
pub mod hf_download;

#[cfg(feature = "local-onnx")]
pub mod onnx;

#[cfg(feature = "local-onnx")]
pub mod pplx_onnx;

#[cfg(feature = "local-onnx")]
pub mod pplx_context_onnx;

#[cfg(all(target_os = "macos", feature = "local-coreml"))]
mod coreml;

#[cfg(all(target_os = "macos", feature = "local-coreml"))]
pub mod pplx_context_coreml;

pub use error::EmbedError;
pub use factory::create_embedder;
pub use model_cache::{ModelCache, ModelSpec};
pub use openai::OpenAiEmbedder;
pub use perplexity::PerplexityEmbedder;
pub use retry::RetryPolicy;
pub use voyage::VoyageEmbedder;

#[cfg(feature = "local-onnx")]
pub use onnx::OnnxEmbedder;

#[cfg(feature = "local-onnx")]
pub use pplx_onnx::PplxOnnxEmbedder;

#[cfg(feature = "local-onnx")]
pub use pplx_context_onnx::PplxContextOnnxEmbedder;

#[cfg(all(target_os = "macos", feature = "local-coreml"))]
pub use pplx_context_coreml::PplxContextCoreMLEmbedder;
