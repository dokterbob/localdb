//! Embedder factory — selects and constructs an `Embedder` from config.
//!
//! See specs/03-config.md §6 for provider configuration details.

use localdb_core::config::schema::{EmbeddingPolicy, ProviderConfig};
use localdb_core::{Embedder, VectorEncoding};
use std::path::Path;

use crate::error::EmbedError;

type BoxedEmbedder = Box<dyn Embedder>;

struct ShapeRule {
    providers: &'static [&'static str],
    model: Option<&'static str>,
    dim: usize,
    encoding: VectorEncoding,
}

impl ShapeRule {
    fn matches(&self, policy: &EmbeddingPolicy) -> bool {
        self.providers.contains(&policy.provider.as_str())
            && self.model.is_none_or(|model| model == policy.model.as_str())
    }
}

const SHAPES: &[ShapeRule] = &[
    ShapeRule {
        providers: &["fake"],
        model: Some("bge-small-en-v1.5"),
        dim: 384,
        encoding: VectorEncoding::Float32,
    },
    ShapeRule {
        providers: &["fake"],
        model: None,
        dim: 128,
        encoding: VectorEncoding::Float32,
    },
    ShapeRule {
        providers: &["local", "local-coreml", "local-onnx"],
        model: Some("pplx-embed-context-v1-0.6b"),
        dim: 1024,
        encoding: VectorEncoding::Binary,
    },
    ShapeRule {
        providers: &["local", "local-onnx"],
        model: Some("pplx-embed-v1-0.6b"),
        dim: 1024,
        encoding: VectorEncoding::Binary,
    },
    ShapeRule {
        providers: &["local", "local-onnx"],
        model: Some("bge-small-en-v1.5"),
        dim: 384,
        encoding: VectorEncoding::Float32,
    },
    ShapeRule {
        providers: &["openai-compatible"],
        model: None,
        dim: 1536,
        encoding: VectorEncoding::Float32,
    },
    ShapeRule {
        providers: &["perplexity"],
        model: None,
        dim: 1024,
        encoding: VectorEncoding::Float32,
    },
    ShapeRule {
        providers: &["voyage"],
        model: None,
        dim: 1024,
        encoding: VectorEncoding::Float32,
    },
];

/// Statically map an `EmbeddingPolicy` to `(embedding_dim, encoding)` without
/// constructing an embedder. The unified DB needs these at open time even for
/// metadata-only commands; constructing the embedder there would trigger a
/// ~706 MB model download for the default `local` provider. Values mirror the
/// concrete `Embedder` impls; round-trip parity is asserted in unit tests.
pub fn infer_dim_encoding(
    policy: &EmbeddingPolicy,
    _providers: &[ProviderConfig],
) -> Result<(usize, VectorEncoding), EmbedError> {
    SHAPES
        .iter()
        .find(|rule| rule.matches(policy))
        .map(|rule| (rule.dim, rule.encoding))
        .ok_or_else(|| {
            EmbedError::Internal(format!(
                "cannot infer embedding shape for provider '{}' model '{}'. \
                 Supported providers: 'fake', 'local', 'local-coreml', 'local-onnx', \
                 'openai-compatible', 'perplexity', 'voyage'.",
                policy.provider, policy.model,
            ))
        })
}

pub fn create_embedder(
    policy: &EmbeddingPolicy,
    providers: &[ProviderConfig],
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    match policy.provider.as_str() {
        "fake" => create_fake(policy),
        "local" => create_local_auto(policy, models_dir),
        #[cfg(all(target_os = "macos", feature = "local-coreml"))]
        "local-coreml" => create_coreml(policy, models_dir),
        #[cfg(not(all(target_os = "macos", feature = "local-coreml")))]
        "local-coreml" => create_coreml_unavailable(),
        #[cfg(feature = "local-onnx")]
        "local-onnx" => create_onnx(policy, models_dir),
        #[cfg(not(feature = "local-onnx"))]
        "local-onnx" => create_onnx_unavailable(),
        "openai-compatible" => create_openai_compatible(policy, providers),
        "perplexity" => create_perplexity(providers),
        "voyage" => create_voyage(providers),
        unknown => unknown_provider(unknown),
    }
}

fn create_fake(policy: &EmbeddingPolicy) -> Result<BoxedEmbedder, EmbedError> {
    let dim = match policy.model.as_str() {
        "bge-small-en-v1.5" => 384,
        _ => 128,
    };
    Ok(Box::new(localdb_core::FakeEmbedder::new(dim)))
}

fn unknown_provider(unknown: &str) -> Result<BoxedEmbedder, EmbedError> {
    Err(EmbedError::Internal(format!(
        "unknown provider: '{unknown}'. \
         Supported: 'fake', 'local', 'local-coreml', 'local-onnx', \
         'openai-compatible', 'perplexity', 'voyage'."
    )))
}

fn create_openai_compatible(
    policy: &EmbeddingPolicy,
    providers: &[ProviderConfig],
) -> Result<BoxedEmbedder, EmbedError> {
    let provider = provider_config(
        providers,
        "openai-compatible",
        "no openai-compatible provider block in config; add a 'providers:' \
         entry with kind: openai-compatible",
    )?;
    let api_key = optional_api_key(provider);
    let base_url = provider
        .base_url
        .as_deref()
        .unwrap_or("https://api.openai.com");
    let embedder = crate::OpenAiEmbedder::new(
        base_url,
        api_key,
        policy.model.as_str(),
        1536,
        None,
        crate::RetryPolicy::default(),
    )?;
    Ok(Box::new(embedder))
}

fn create_perplexity(providers: &[ProviderConfig]) -> Result<BoxedEmbedder, EmbedError> {
    let provider = provider_config(
        providers,
        "perplexity",
        "no perplexity provider block in config; add a 'providers:' entry \
         with kind: perplexity and api_key_env pointing to your API key",
    )?;
    let api_key = required_api_key(
        provider,
        "perplexity provider requires 'api_key_env' to be set in config",
    )?;
    let embedder =
        crate::PerplexityEmbedder::new(api_key, None, None, crate::RetryPolicy::default())?;
    Ok(Box::new(embedder))
}

fn create_voyage(providers: &[ProviderConfig]) -> Result<BoxedEmbedder, EmbedError> {
    let provider = provider_config(
        providers,
        "voyage",
        "no voyage provider block in config; add a 'providers:' entry \
         with kind: voyage and api_key_env pointing to your API key",
    )?;
    let api_key = required_api_key(
        provider,
        "voyage provider requires 'api_key_env' to be set in config",
    )?;
    let embedder = crate::VoyageEmbedder::new(api_key, None, None, crate::RetryPolicy::default())?;
    Ok(Box::new(embedder))
}

fn provider_config<'a>(
    providers: &'a [ProviderConfig],
    kind: &str,
    missing_message: &str,
) -> Result<&'a ProviderConfig, EmbedError> {
    providers
        .iter()
        .find(|provider| provider.kind == kind)
        .ok_or_else(|| EmbedError::ProviderNotConfigured(missing_message.to_string()))
}

fn optional_api_key(provider: &ProviderConfig) -> Option<String> {
    provider
        .api_key_env
        .as_deref()
        .and_then(|env| std::env::var(env).ok())
}

fn required_api_key(
    provider: &ProviderConfig,
    missing_message: &str,
) -> Result<String, EmbedError> {
    let Some(env) = &provider.api_key_env else {
        return Err(EmbedError::ProviderNotConfigured(
            missing_message.to_string(),
        ));
    };
    let key = std::env::var(env).unwrap_or_default();
    if key.is_empty() {
        return Err(EmbedError::ProviderNotConfigured(format!(
            "{} API key env var '{}' is unset or empty",
            provider.kind, env
        )));
    }
    Ok(key)
}

#[cfg(all(target_os = "macos", feature = "local-coreml"))]
fn create_coreml(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    let cache_dir = models_dir.map(|p| p.to_path_buf());
    match policy.model.as_str() {
        "pplx-embed-context-v1-0.6b" => {
            let embedder =
                crate::pplx_context_coreml::PplxContextCoreMLEmbedder::new(cache_dir, true)?;
            Ok(Box::new(embedder))
        }
        unknown => Err(EmbedError::Internal(format!(
            "unknown local-coreml model: '{unknown}'. \
             Supported: 'pplx-embed-context-v1-0.6b'."
        ))),
    }
}

#[cfg(not(all(target_os = "macos", feature = "local-coreml")))]
fn create_coreml_unavailable() -> Result<BoxedEmbedder, EmbedError> {
    Err(EmbedError::Internal(
        "provider 'local-coreml' requires macOS with the 'local-coreml' feature. \
         Use provider 'local-onnx' or a hosted provider instead."
            .to_string(),
    ))
}

#[cfg(not(feature = "local-onnx"))]
fn create_onnx_unavailable() -> Result<BoxedEmbedder, EmbedError> {
    Err(EmbedError::Internal(
        "provider 'local-onnx' requires the 'local-onnx' feature flag. \
         Rebuild with `--features local-onnx` or choose a hosted provider."
            .to_string(),
    ))
}

#[cfg(feature = "local-onnx")]
fn create_onnx(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
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

#[allow(clippy::needless_return)]
fn create_local_auto(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
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
        let provider = ProviderConfig {
            name: "pplx".to_string(),
            kind: "perplexity".to_string(),
            base_url: None,
            api_key_env: Some("LOCALDB_TEST_UNSET_VAR_PERPLEXITY_XYZ".to_string()),
        };
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

    #[test]
    fn infer_dim_encoding_matches_fake_default() {
        let policy = fake_policy("fake", "unknown-model");
        let (dim, encoding) = infer_dim_encoding(&policy, &[]).unwrap();
        let embedder = create_embedder(&policy, &[], None).unwrap();
        assert_eq!(dim, embedder.embedding_dim());
        assert_eq!(encoding, embedder.vector_encoding());
    }

    #[test]
    fn infer_dim_encoding_matches_fake_bge_dim() {
        let policy = fake_policy("fake", "bge-small-en-v1.5");
        let (dim, encoding) = infer_dim_encoding(&policy, &[]).unwrap();
        let embedder = create_embedder(&policy, &[], None).unwrap();
        assert_eq!(dim, embedder.embedding_dim());
        assert_eq!(encoding, embedder.vector_encoding());
    }

    #[test]
    fn infer_dim_encoding_known_hosted_pairs() {
        let cases = [
            ("openai-compatible", "text-embedding-3-small", 1536),
            ("perplexity", "pplx-embed-context-v1", 1024),
            ("voyage", "voyage-context-3", 1024),
        ];
        for (provider, model, expected_dim) in cases {
            let policy = fake_policy(provider, model);
            let (dim, encoding) = infer_dim_encoding(&policy, &[]).unwrap();
            assert_eq!(dim, expected_dim, "{provider}/{model} dim");
            assert_eq!(
                encoding,
                VectorEncoding::Float32,
                "{provider}/{model} encoding"
            );
        }
    }

    #[test]
    fn infer_dim_encoding_known_local_pairs() {
        let cases = [
            (
                "local",
                "pplx-embed-context-v1-0.6b",
                1024,
                VectorEncoding::Binary,
            ),
            (
                "local-onnx",
                "pplx-embed-context-v1-0.6b",
                1024,
                VectorEncoding::Binary,
            ),
            (
                "local-coreml",
                "pplx-embed-context-v1-0.6b",
                1024,
                VectorEncoding::Binary,
            ),
            (
                "local-onnx",
                "bge-small-en-v1.5",
                384,
                VectorEncoding::Float32,
            ),
        ];
        for (provider, model, expected_dim, expected_encoding) in cases {
            let policy = fake_policy(provider, model);
            let (dim, encoding) = infer_dim_encoding(&policy, &[]).unwrap();
            assert_eq!(dim, expected_dim, "{provider}/{model} dim");
            assert_eq!(encoding, expected_encoding, "{provider}/{model} encoding");
        }
    }

    #[test]
    fn infer_dim_encoding_rejects_unknown_provider() {
        let policy = fake_policy("nonexistent", "model");
        let err = infer_dim_encoding(&policy, &[]).unwrap_err();
        assert!(
            matches!(err, EmbedError::Internal(_)),
            "unknown provider should fail, got: {err:?}"
        );
    }
}
