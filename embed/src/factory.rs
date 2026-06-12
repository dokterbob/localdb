//! Embedder factory — selects and constructs an `Embedder` from config.
//!
//! See specs/03-config.md §6 for provider configuration details.

use localdb_core::config::schema::{EmbeddingPolicy, ProviderConfig};
use localdb_core::Embedder;

use crate::error::EmbedError;

/// Construct a boxed `Embedder` from an `EmbeddingPolicy` and the list of
/// provider configs declared in the YAML config.
///
/// Provider dispatch:
/// - `"fake"` — in-process `FakeEmbedder`; no I/O, no model download.
/// - `"local-onnx"` — in-process ONNX inference via fastembed (requires feature `local-onnx`).
/// - `"openai-compatible"` — flat HTTP provider targeting any `/v1/embeddings` endpoint.
/// - `"perplexity"` — contextualized HTTP provider.
/// - `"voyage"` — contextualized HTTP provider.
pub fn create_embedder(
    policy: &EmbeddingPolicy,
    providers: &[ProviderConfig],
    models_dir: Option<&std::path::Path>,
) -> Result<Box<dyn Embedder>, EmbedError> {
    match policy.provider.as_str() {
        "fake" => {
            let dim = match policy.model.as_str() {
                "bge-small-en-v1.5" => 384,
                _ => 128,
            };
            Ok(Box::new(localdb_core::FakeEmbedder::new(dim)))
        }

        #[cfg(feature = "local-onnx")]
        "local-onnx" => {
            use crate::onnx::{ModelChoice, OnnxEmbedder};
            let model_choice = match policy.model.as_str() {
                "bge-small-en-v1.5" | "default" => ModelChoice::Default,
                unknown => {
                    return Err(EmbedError::Internal(format!(
                        "unknown local-onnx model: '{unknown}'. \
                         Supported: 'bge-small-en-v1.5', 'default'."
                    )))
                }
            };
            let cache_dir = models_dir.map(|p| p.to_path_buf());
            let embedder = OnnxEmbedder::new(model_choice, cache_dir, true)?;
            Ok(Box::new(embedder))
        }

        #[cfg(not(feature = "local-onnx"))]
        "local-onnx" => Err(EmbedError::Internal(
            "provider 'local-onnx' requires the 'local-onnx' feature flag. \
             Rebuild with `--features local-onnx` or choose a hosted provider."
                .to_string(),
        )),

        "openai-compatible" => {
            let provider = providers
                .iter()
                .find(|p| p.kind == "openai-compatible")
                .ok_or_else(|| {
                    EmbedError::ProviderNotConfigured(
                        "no openai-compatible provider block in config; add a 'providers:' \
                         entry with kind: openai-compatible"
                            .to_string(),
                    )
                })?;
            let api_key = provider
                .api_key_env
                .as_deref()
                .and_then(|env| std::env::var(env).ok());
            let base_url = provider
                .base_url
                .as_deref()
                .unwrap_or("https://api.openai.com");
            let e = crate::OpenAiEmbedder::new(
                base_url,
                api_key,
                policy.model.as_str(),
                1536,
                None,
                crate::RetryPolicy::default(),
            )?;
            Ok(Box::new(e))
        }

        "perplexity" => {
            let provider = providers
                .iter()
                .find(|p| p.kind == "perplexity")
                .ok_or_else(|| {
                    EmbedError::ProviderNotConfigured(
                        "no perplexity provider block in config; add a 'providers:' entry \
                         with kind: perplexity and api_key_env pointing to your API key"
                            .to_string(),
                    )
                })?;
            let api_key = provider
                .api_key_env
                .as_deref()
                .and_then(|env| std::env::var(env).ok())
                .unwrap_or_default();
            let e =
                crate::PerplexityEmbedder::new(api_key, None, None, crate::RetryPolicy::default())?;
            Ok(Box::new(e))
        }

        "voyage" => {
            let provider = providers
                .iter()
                .find(|p| p.kind == "voyage")
                .ok_or_else(|| {
                    EmbedError::ProviderNotConfigured(
                        "no voyage provider block in config; add a 'providers:' entry \
                         with kind: voyage and api_key_env pointing to your API key"
                            .to_string(),
                    )
                })?;
            let api_key = provider
                .api_key_env
                .as_deref()
                .and_then(|env| std::env::var(env).ok())
                .unwrap_or_default();
            let e = crate::VoyageEmbedder::new(api_key, None, None, crate::RetryPolicy::default())?;
            Ok(Box::new(e))
        }

        unknown => Err(EmbedError::Internal(format!(
            "unknown provider: '{unknown}'. \
             Supported: 'fake', 'local-onnx', 'openai-compatible', 'perplexity', 'voyage'."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::config::schema::EmbeddingPolicy;

    fn fake_policy(provider: &str, model: &str) -> EmbeddingPolicy {
        EmbeddingPolicy {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    #[test]
    fn fake_provider_creates_fake_embedder() {
        let policy = fake_policy("fake", "bge-small-en-v1.5");
        let embedder = create_embedder(&policy, &[], None).unwrap();
        assert_eq!(embedder.embedding_dim(), 384);
    }

    #[test]
    fn fake_provider_default_dim() {
        let policy = fake_policy("fake", "unknown-model");
        let embedder = create_embedder(&policy, &[], None).unwrap();
        assert_eq!(embedder.embedding_dim(), 128);
    }

    #[test]
    fn unknown_provider_returns_error() {
        let policy = fake_policy("does-not-exist", "some-model");
        let result = create_embedder(&policy, &[], None);
        assert!(result.is_err(), "unknown provider should return Err");
    }
}
