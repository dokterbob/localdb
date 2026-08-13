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
//! Batch size and per-request timeout are hosted-provider concerns documented in [`retry`]
//! (batch size: 32 chunks per request, timeout: 30 s per request, both configurable).
//!
//! Retry/backoff/jitter/`Retry-After` handling is **not** reimplemented here — hosted providers
//! send their requests through [`http_helper::send_with_retry`], which drives `fetch::http`'s
//! shared outgoing-HTTP retry policy (issue #207): jittered exponential back-off, honoring a
//! server's `Retry-After` header, retrying on network errors/429/408/5xx, by default up to 3
//! retries (4 total attempts). Hosted providers are reactive-only: retry policy applies, but
//! there is deliberately no proactive per-host rate limiter (`fetch::http::HostLimiter`) here —
//! that mechanism exists for `fetch`'s document ingestion, where many requests can target one
//! operator-unaware origin; a paid embedding API's own rate limiting is the right place for that
//! for a hosted provider.
//!
//! # Model cache
//!
//! Local models are cached in the platform model cache directory (from config `paths.models`).
//! Download is resumable; integrity is verified with SHA-256. When downloads are disabled and
//! the cache is empty, `Error::ModelMissing` is raised with an actionable message.
//!
//! See specs/04-search-pipeline.md §4.

pub mod contextual;
pub mod error;
pub mod factory;
pub mod http_helper;
pub mod model_cache;
pub mod openai;
pub mod ort_runtime;
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

pub use contextual::{ContextualEmbedder, ContextualProviderSpec};
pub use error::EmbedError;
pub use factory::{create_embedder, infer_dim_encoding};
pub use model_cache::{ModelCache, ModelSpec};
pub use openai::OpenAiEmbedder;
pub use perplexity::{Perplexity, PerplexityEmbedder};
pub use retry::RetryPolicy;
pub use voyage::{Voyage, VoyageEmbedder};

#[cfg(feature = "local-onnx")]
pub use onnx::OnnxEmbedder;

#[cfg(feature = "local-onnx")]
pub use pplx_onnx::PplxOnnxEmbedder;

#[cfg(feature = "local-onnx")]
pub use pplx_context_onnx::PplxContextOnnxEmbedder;

#[cfg(all(target_os = "macos", feature = "local-coreml"))]
pub use pplx_context_coreml::PplxContextCoreMLEmbedder;
