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
/// - `"local"` — auto: on macOS with the `local-coreml` feature, use CoreML for
///   the pplx context model (falling back to ONNX on error); otherwise ONNX.
/// - `"local-coreml"` — force in-process CoreML (macOS + `local-coreml` only).
/// - `"local-onnx"` — in-process ONNX inference (requires feature `local-onnx`).
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

        // -------------------------------------------------------------------
        // "local" — AUTO: prefer CoreML on macOS, else ONNX.
        // -------------------------------------------------------------------
        "local" => create_local_auto(policy, models_dir),

        // -------------------------------------------------------------------
        // "local-coreml" — FORCE CoreML (no fallback).
        // -------------------------------------------------------------------
        #[cfg(all(target_os = "macos", feature = "local-coreml"))]
        "local-coreml" => {
            let cache_dir = models_dir.map(|p| p.to_path_buf());
            match policy.model.as_str() {
                "pplx-embed-context-v1-0.6b" => {
                    let embedder = crate::pplx_context_coreml::PplxContextCoreMLEmbedder::new(
                        cache_dir, true,
                    )?;
                    Ok(Box::new(embedder))
                }
                unknown => Err(EmbedError::Internal(format!(
                    "unknown local-coreml model: '{unknown}'. \
                     Supported: 'pplx-embed-context-v1-0.6b'."
                ))),
            }
        }

        #[cfg(not(all(target_os = "macos", feature = "local-coreml")))]
        "local-coreml" => Err(EmbedError::Internal(
            "provider 'local-coreml' requires macOS with the 'local-coreml' feature. \
             Use provider 'local-onnx' or a hosted provider instead."
                .to_string(),
        )),

        // -------------------------------------------------------------------
        // "local-onnx" — FORCE ONNX.
        // -------------------------------------------------------------------
        #[cfg(feature = "local-onnx")]
        "local-onnx" => create_onnx(policy, models_dir),

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
            let api_key = match &provider.api_key_env {
                None => {
                    return Err(EmbedError::ProviderNotConfigured(
                        "perplexity provider requires 'api_key_env' to be set in config"
                            .to_string(),
                    ))
                }
                Some(env) => {
                    let key = std::env::var(env).unwrap_or_default();
                    if key.is_empty() {
                        return Err(EmbedError::ProviderNotConfigured(format!(
                            "perplexity API key env var '{}' is unset or empty",
                            env
                        )));
                    }
                    key
                }
            };
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
            let api_key = match &provider.api_key_env {
                None => {
                    return Err(EmbedError::ProviderNotConfigured(
                        "voyage provider requires 'api_key_env' to be set in config".to_string(),
                    ))
                }
                Some(env) => {
                    let key = std::env::var(env).unwrap_or_default();
                    if key.is_empty() {
                        return Err(EmbedError::ProviderNotConfigured(format!(
                            "voyage API key env var '{}' is unset or empty",
                            env
                        )));
                    }
                    key
                }
            };
            let e = crate::VoyageEmbedder::new(api_key, None, None, crate::RetryPolicy::default())?;
            Ok(Box::new(e))
        }

        unknown => Err(EmbedError::Internal(format!(
            "unknown provider: '{unknown}'. \
             Supported: 'fake', 'local', 'local-coreml', 'local-onnx', \
             'openai-compatible', 'perplexity', 'voyage'."
        ))),
    }
}

// ---------------------------------------------------------------------------
// Local provider helpers
// ---------------------------------------------------------------------------

/// Build the in-process ONNX embedder for `policy.model` (requires `local-onnx`).
#[cfg(feature = "local-onnx")]
fn create_onnx(
    policy: &EmbeddingPolicy,
    models_dir: Option<&std::path::Path>,
) -> Result<Box<dyn Embedder>, EmbedError> {
    let cache_dir = models_dir.map(|p| p.to_path_buf());
    match policy.model.as_str() {
        "pplx-embed-context-v1-0.6b" => {
            let embedder = crate::pplx_context_onnx::PplxContextOnnxEmbedder::new(cache_dir, true)?;
            Ok(Box::new(embedder))
        }
        "pplx-embed-v1-0.6b" => {
            let embedder = crate::pplx_onnx::PplxOnnxEmbedder::new(cache_dir, true)?;
            Ok(Box::new(embedder))
        }
        "bge-small-en-v1.5" => {
            use crate::onnx::{ModelChoice, OnnxEmbedder};
            let embedder = OnnxEmbedder::new(ModelChoice::BgeSmallEnV15, cache_dir, true)?;
            Ok(Box::new(embedder))
        }
        unknown => Err(EmbedError::Internal(format!(
            "unknown local-onnx model: '{unknown}'. \
             Supported: 'pplx-embed-context-v1-0.6b', 'pplx-embed-v1-0.6b', 'bge-small-en-v1.5'."
        ))),
    }
}

/// AUTO local provider: prefer CoreML on macOS (falling back to ONNX on error),
/// else use ONNX. Returns a clear error if no local backend is compiled in.
//
// `return` is used throughout so the cfg-gated branches compose across every
// feature/platform combination; the trailing position differs per config.
#[allow(clippy::needless_return)]
fn create_local_auto(
    policy: &EmbeddingPolicy,
    models_dir: Option<&std::path::Path>,
) -> Result<Box<dyn Embedder>, EmbedError> {
    // macOS + CoreML: try CoreML for the context model, fall back to ONNX.
    #[cfg(all(target_os = "macos", feature = "local-coreml"))]
    {
        if policy.model == "pplx-embed-context-v1-0.6b" {
            let cache_dir = models_dir.map(|p| p.to_path_buf());
            match crate::pplx_context_coreml::PplxContextCoreMLEmbedder::new(cache_dir, true) {
                Ok(embedder) => return Ok(Box::new(embedder)),
                Err(e) => {
                    #[cfg(feature = "local-onnx")]
                    {
                        tracing::warn!(
                            error = %e,
                            "CoreML embedder unavailable; falling back to ONNX"
                        );
                        return create_onnx(policy, models_dir);
                    }
                    #[cfg(not(feature = "local-onnx"))]
                    {
                        return Err(e);
                    }
                }
            }
        }
        // Non-context models: go straight to ONNX when available.
        #[cfg(feature = "local-onnx")]
        {
            return create_onnx(policy, models_dir);
        }
        #[cfg(not(feature = "local-onnx"))]
        {
            return Err(EmbedError::Internal(format!(
                "provider 'local' with model '{}' needs the 'local-onnx' feature \
                 (only 'pplx-embed-context-v1-0.6b' is available via CoreML). \
                 Rebuild with `--features local-onnx`.",
                policy.model
            )));
        }
    }

    // Non-macOS or no CoreML: ONNX if available, else a clear error.
    #[cfg(not(all(target_os = "macos", feature = "local-coreml")))]
    {
        #[cfg(feature = "local-onnx")]
        {
            return create_onnx(policy, models_dir);
        }
        #[cfg(not(feature = "local-onnx"))]
        {
            let _ = (policy, models_dir);
            return Err(EmbedError::Internal(
                "provider 'local' requires a local backend: rebuild with \
                 `--features local-onnx` (all platforms) or `--features local-coreml` \
                 (macOS), or choose a hosted provider."
                    .to_string(),
            ));
        }
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

    #[test]
    fn perplexity_missing_api_key_env_returns_error() {
        use localdb_core::config::schema::ProviderConfig;
        let policy = fake_policy("perplexity", "pplx-embed-context-v1");
        let provider = ProviderConfig {
            name: "pplx".to_string(),
            kind: "perplexity".to_string(),
            base_url: None,
            api_key_env: None,
        };
        let result = create_embedder(&policy, &[provider], None);
        assert!(
            matches!(result, Err(EmbedError::ProviderNotConfigured(_))),
            "missing api_key_env should return ProviderNotConfigured, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn perplexity_empty_api_key_returns_error() {
        use localdb_core::config::schema::ProviderConfig;
        let policy = fake_policy("perplexity", "pplx-embed-context-v1");
        // Use an env var that is guaranteed to be unset.
        let provider = ProviderConfig {
            name: "pplx".to_string(),
            kind: "perplexity".to_string(),
            base_url: None,
            api_key_env: Some("LOCALDB_TEST_UNSET_VAR_PERPLEXITY_XYZ".to_string()),
        };
        // Ensure it's not set.
        std::env::remove_var("LOCALDB_TEST_UNSET_VAR_PERPLEXITY_XYZ");
        let result = create_embedder(&policy, &[provider], None);
        assert!(
            matches!(result, Err(EmbedError::ProviderNotConfigured(_))),
            "unset api key env var should return ProviderNotConfigured, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn voyage_missing_api_key_env_returns_error() {
        use localdb_core::config::schema::ProviderConfig;
        let policy = fake_policy("voyage", "voyage-3");
        let provider = ProviderConfig {
            name: "voy".to_string(),
            kind: "voyage".to_string(),
            base_url: None,
            api_key_env: None,
        };
        let result = create_embedder(&policy, &[provider], None);
        assert!(
            matches!(result, Err(EmbedError::ProviderNotConfigured(_))),
            "missing api_key_env should return ProviderNotConfigured, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn voyage_empty_api_key_returns_error() {
        use localdb_core::config::schema::ProviderConfig;
        let policy = fake_policy("voyage", "voyage-3");
        let provider = ProviderConfig {
            name: "voy".to_string(),
            kind: "voyage".to_string(),
            base_url: None,
            api_key_env: Some("LOCALDB_TEST_UNSET_VAR_VOYAGE_XYZ".to_string()),
        };
        std::env::remove_var("LOCALDB_TEST_UNSET_VAR_VOYAGE_XYZ");
        let result = create_embedder(&policy, &[provider], None);
        assert!(
            matches!(result, Err(EmbedError::ProviderNotConfigured(_))),
            "unset api key env var should return ProviderNotConfigured, got: {:?}",
            result.err()
        );
    }
}
