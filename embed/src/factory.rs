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
            && self
                .model
                .is_none_or(|model| model == policy.model.as_str())
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

/// # `http_settings`
///
/// Threaded through to the three hosted providers only (`openai-compatible`,
/// `perplexity`, `voyage`) — every request they make is an outbound HTTP call
/// this crate builds itself, so the operator's `http:` config (user agent,
/// retry count, per-host rate limit — see `specs/03-config.md` §2/§8) must
/// reach the `reqwest::Client` and retry loop those constructors build. Local
/// providers (`local`/`local-onnx`/`local-coreml`) never make an HTTP request
/// to embed a document — inference runs in-process — so the parameter is
/// simply unused on those branches; it is still required here (rather than
/// threaded conditionally) so every caller pays the same cost of remembering
/// it exists, instead of only the three call paths that currently need it.
///
/// Callers already hold a `localdb_core::config::schema::HttpConfig` (the
/// parsed `http:` YAML section); convert it once with
/// `fetch::http::HttpSettings::from(&cfg.http)` (or `(&cfg.http).into()`) —
/// the same conversion `fetch::HttpUrlFetcher::new_pair` uses for document/
/// URL/feed fetches — and pass the same instance here. Keeping the parameter
/// typed as `fetch::http::HttpSettings` rather than `HttpConfig` keeps this
/// crate, like `fetch` itself, free of `core`'s config-format concerns (no
/// need to know about `#[serde(default = ...)]` defaulting here) and reuses
/// a conversion that already exists rather than inventing a second one.
pub fn create_embedder(
    policy: &EmbeddingPolicy,
    providers: &[ProviderConfig],
    models_dir: Option<&Path>,
    http_settings: &fetch::http::HttpSettings,
) -> Result<BoxedEmbedder, EmbedError> {
    match policy.provider.as_str() {
        "fake" => create_fake(policy),
        "local" => create_local_auto(policy, models_dir),
        #[cfg(all(target_os = "macos", feature = "local-coreml"))]
        "local-coreml" => create_coreml(policy, models_dir),
        #[cfg(not(all(target_os = "macos", feature = "local-coreml")))]
        "local-coreml" => create_coreml_unavailable(),
        "local-onnx" => create_onnx(policy, models_dir),
        "openai-compatible" => create_openai_compatible(policy, providers, http_settings),
        "perplexity" => create_perplexity(providers, http_settings),
        "voyage" => create_voyage(providers, http_settings),
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
    http_settings: &fetch::http::HttpSettings,
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
        http_settings.clone(),
    )?;
    Ok(Box::new(embedder))
}

/// Build a hosted, document-context provider (`perplexity`, `voyage`) whose
/// `ProviderConfig` lookup, missing-block/missing-key error messages, and
/// embedder construction call all follow the same shape — only the provider
/// `kind` string and the concrete constructor differ. `ctor` is one of
/// `PerplexityEmbedder::new`/`VoyageEmbedder::new` partially applied to
/// everything but the resolved `api_key`; `E` is boxed here so both call
/// sites can share this one function despite returning different concrete
/// `Embedder` types.
fn create_hosted_contextual<E: Embedder + 'static>(
    providers: &[ProviderConfig],
    kind: &str,
    http_settings: &fetch::http::HttpSettings,
    ctor: impl FnOnce(String, fetch::http::HttpSettings) -> Result<E, EmbedError>,
) -> Result<BoxedEmbedder, EmbedError> {
    let provider = provider_config(
        providers,
        kind,
        &format!(
            "no {kind} provider block in config; add a 'providers:' entry \
             with kind: {kind} and api_key_env pointing to your API key"
        ),
    )?;
    let api_key = required_api_key(
        provider,
        &format!("{kind} provider requires 'api_key_env' to be set in config"),
    )?;
    let embedder = ctor(api_key, http_settings.clone())?;
    Ok(Box::new(embedder))
}

fn create_perplexity(
    providers: &[ProviderConfig],
    http_settings: &fetch::http::HttpSettings,
) -> Result<BoxedEmbedder, EmbedError> {
    create_hosted_contextual(providers, "perplexity", http_settings, |api_key, http| {
        crate::PerplexityEmbedder::new(api_key, None, None, crate::RetryPolicy::default(), http)
    })
}

fn create_voyage(
    providers: &[ProviderConfig],
    http_settings: &fetch::http::HttpSettings,
) -> Result<BoxedEmbedder, EmbedError> {
    create_hosted_contextual(providers, "voyage", http_settings, |api_key, http| {
        crate::VoyageEmbedder::new(api_key, None, None, crate::RetryPolicy::default(), http)
    })
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

/// Why this build has no local ONNX backend. Two distinct causes, and telling them apart
/// matters: one is a build flag the user can just turn on, the other is a platform localdb
/// embeds no ONNX Runtime for at all (see `embed/build.rs`). `build.rs` emits the
/// `ort_embedded` cfg exactly when it embeds one, so this text and the cfg gates below
/// cannot disagree about which case holds.
#[cfg(not(ort_embedded))]
const NO_LOCAL_ONNX: &str = if cfg!(feature = "local-onnx") {
    "localdb embeds an ONNX Runtime only for Linux and macOS, so local inference is \
     unavailable on this platform. Build with LOCALDB_ORT_LIB set to your own ONNX Runtime \
     shared library, or choose a hosted provider ('openai-compatible', 'perplexity', \
     'voyage')."
} else {
    "this binary was built without the 'local-onnx' feature. Rebuild with \
     `--features local-onnx`, or choose a hosted provider ('openai-compatible', \
     'perplexity', 'voyage')."
};

/// Stand-in for [`create_onnx`] on builds with no embedded ONNX Runtime. Same signature, so
/// every caller stays free of `#[cfg]` — the whole conditional surface for local ONNX is
/// this one pair of definitions.
#[cfg(not(ort_embedded))]
fn create_onnx(
    _policy: &EmbeddingPolicy,
    _models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    Err(EmbedError::Internal(format!(
        "no local ONNX backend: {NO_LOCAL_ONNX}"
    )))
}

#[cfg(ort_embedded)]
fn create_onnx(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    // Idempotent: extracts/dlopens the embedded ONNX Runtime once per process. Also called
    // at the top of each embedder constructor below (OnnxEmbedder::new etc.) so that direct
    // construction (tests, examples) doesn't skip it — the OnceLock makes the repeat cheap.
    crate::ort_runtime::ensure_ort_initialized()?;
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

/// `provider: local` — pick the best local backend for this build and model.
///
/// CoreML gets first refusal on Apple Silicon for the one model it serves; everything else,
/// including a CoreML that fails to load, falls through to ONNX. Both backends emit
/// index-interchangeable vectors, so which one answers is not observable in the index.
///
/// When no ONNX Runtime is embedded, `create_onnx` is the stand-in above and reports why —
/// which is the whole reason this function needs no `#[cfg]` of its own beyond the CoreML
/// branch.
fn create_local_auto(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    #[cfg(all(target_os = "macos", feature = "local-coreml"))]
    if policy.model == "pplx-embed-context-v1-0.6b" {
        let cache_dir = models_dir.map(|p| p.to_path_buf());
        match crate::pplx_context_coreml::PplxContextCoreMLEmbedder::new(cache_dir, true) {
            Ok(embedder) => return Ok(Box::new(embedder)),
            // Not fatal on its own — ONNX serves this model too. If it can't, the error
            // from below explains that, and this line is the record of why CoreML didn't.
            Err(e) => tracing::warn!(
                error = %e,
                "CoreML embedder unavailable; falling back to ONNX"
            ),
        }
    }

    create_onnx(policy, models_dir)
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
        let embedder =
            create_embedder(&policy, &[], None, &fetch::http::HttpSettings::default()).unwrap();
        assert_eq!(embedder.embedding_dim(), 384);
    }

    #[test]
    fn fake_provider_default_dim() {
        let policy = fake_policy("fake", "unknown-model");
        let embedder =
            create_embedder(&policy, &[], None, &fetch::http::HttpSettings::default()).unwrap();
        assert_eq!(embedder.embedding_dim(), 128);
    }

    /// `local` is the default provider, so on a build with no embedded ONNX Runtime the
    /// very first `localdb index` lands here. It must fail as a clean, actionable error
    /// rather than diving into ORT code that was never initialized — the failure mode
    /// this cfg gate exists to prevent.
    #[cfg(not(ort_embedded))]
    #[test]
    fn local_providers_are_unavailable_without_an_embedded_runtime() {
        // `bge-small-en-v1.5` deliberately: CoreML serves only the pplx context model, so
        // this stays an ONNX-backend question on a macOS `local-coreml` build too, instead
        // of wandering into CoreML construction and its model download.
        for provider in ["local", "local-onnx"] {
            let policy = fake_policy(provider, "bge-small-en-v1.5");
            let Err(err) =
                create_embedder(&policy, &[], None, &fetch::http::HttpSettings::default())
            else {
                panic!("'{provider}' must not claim to build a local embedder");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("openai-compatible"),
                "'{provider}' error must point at a way forward, got: {msg}"
            );
            // Exactly one of the two causes, never a blend of both.
            assert_eq!(
                msg.contains("only for Linux and macOS"),
                cfg!(feature = "local-onnx"),
                "'{provider}' names the wrong cause: {msg}"
            );
        }
    }

    #[test]
    fn unknown_provider_returns_error() {
        let policy = fake_policy("does-not-exist", "some-model");
        let result = create_embedder(&policy, &[], None, &fetch::http::HttpSettings::default());
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
        let result = create_embedder(
            &policy,
            &[provider],
            None,
            &fetch::http::HttpSettings::default(),
        );
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
        let result = create_embedder(
            &policy,
            &[provider],
            None,
            &fetch::http::HttpSettings::default(),
        );
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
        let result = create_embedder(
            &policy,
            &[provider],
            None,
            &fetch::http::HttpSettings::default(),
        );
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
        let result = create_embedder(
            &policy,
            &[provider],
            None,
            &fetch::http::HttpSettings::default(),
        );
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
        let embedder =
            create_embedder(&policy, &[], None, &fetch::http::HttpSettings::default()).unwrap();
        assert_eq!(dim, embedder.embedding_dim());
        assert_eq!(encoding, embedder.vector_encoding());
    }

    #[test]
    fn infer_dim_encoding_matches_fake_bge_dim() {
        let policy = fake_policy("fake", "bge-small-en-v1.5");
        let (dim, encoding) = infer_dim_encoding(&policy, &[]).unwrap();
        let embedder =
            create_embedder(&policy, &[], None, &fetch::http::HttpSettings::default()).unwrap();
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
