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
        providers: &["local", "local-coreml", "local-onnx", "local-cuda"],
        model: Some("pplx-embed-context-v1-0.6b"),
        dim: 1024,
        encoding: VectorEncoding::Binary,
    },
    ShapeRule {
        providers: &["local", "local-onnx", "local-cuda"],
        model: Some("pplx-embed-v1-0.6b"),
        dim: 1024,
        encoding: VectorEncoding::Binary,
    },
    ShapeRule {
        providers: &["local", "local-onnx", "local-cuda"],
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
                 'local-cuda', 'openai-compatible', 'perplexity', 'voyage'.",
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
        "local-cuda" => create_cuda(policy, models_dir),
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
         Supported: 'fake', 'local', 'local-coreml', 'local-onnx', 'local-cuda', \
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

/// Thin CPU-only wrapper around [`create_onnx_with`], used by the explicit `local-onnx`
/// provider (the metered-connection / "never touch CUDA" opt-out — CPU-only by definition,
/// regardless of what hardware is available).
#[cfg(feature = "local-onnx")]
fn create_onnx(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    create_onnx_with(policy, models_dir, crate::cuda_ep::CudaPreference::Disabled)
}

#[cfg(feature = "local-onnx")]
fn create_onnx_with(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
    cuda: crate::cuda_ep::CudaPreference,
) -> Result<BoxedEmbedder, EmbedError> {
    // Idempotent: downloads/dlopens the ONNX Runtime once per process. Also called at the top
    // of each embedder constructor below (OnnxEmbedder::new etc.) so that direct construction
    // (tests, examples) doesn't skip it — the once-only init makes the repeat cheap.
    let flavor = match cuda {
        crate::cuda_ep::CudaPreference::Disabled => crate::ort_runtime::OrtFlavor::Cpu,
        crate::cuda_ep::CudaPreference::Preferred | crate::cuda_ep::CudaPreference::Required => {
            crate::ort_runtime::OrtFlavor::Cuda
        }
    };
    crate::ort_runtime::ensure_ort_initialized(flavor, policy.ort_library.as_deref())?;
    let cache_dir = models_dir.map(|p| p.to_path_buf());
    match policy.model.as_str() {
        "pplx-embed-context-v1-0.6b" => {
            let embedder =
                crate::pplx_context_onnx::PplxContextOnnxEmbedder::new(cache_dir, true, cuda)?;
            Ok(Box::new(embedder))
        }
        "pplx-embed-v1-0.6b" => {
            let embedder = crate::pplx_onnx::PplxOnnxEmbedder::new(cache_dir, true, cuda)?;
            Ok(Box::new(embedder))
        }
        "bge-small-en-v1.5" => {
            use crate::onnx::{ModelChoice, OnnxEmbedder};
            let embedder = OnnxEmbedder::new(ModelChoice::BgeSmallEnV15, cache_dir, true, cuda)?;
            Ok(Box::new(embedder))
        }
        unknown => Err(EmbedError::Internal(format!(
            "unknown local-onnx model: '{unknown}'. \
             Supported: 'pplx-embed-context-v1-0.6b', 'pplx-embed-v1-0.6b', 'bge-small-en-v1.5'."
        ))),
    }
}

/// Explicit `local-cuda` provider: forces the CUDA execution provider. Linux x86_64 with an
/// NVIDIA GPU only — hard error (no CPU fallback) if the CUDA stack is unavailable, since the
/// caller specifically asked for GPU acceleration.
fn create_cuda(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        create_cuda_with_status(crate::cuda_ep::detect_cuda_stack(), policy, models_dir)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (policy, models_dir);
        // ProviderError (not Internal): maps to core's ProviderUnavailable → exit code 5
        // ("unavailable", specs/05-surfaces.md §5) rather than 1.
        Err(EmbedError::ProviderError {
            provider: "local-cuda".to_string(),
            message: "provider 'local-cuda' requires Linux x86_64 with an NVIDIA GPU. \
                      Use provider 'local' (auto-detects, CPU fallback) or 'local-onnx' instead."
                .to_string(),
        })
    }
}

/// Core of [`create_cuda`], parameterized over an already-known [`CudaStackStatus`] so it's
/// unit-testable on every platform (including this macOS dev machine) with injected statuses —
/// without needing a real Linux/CUDA host to exercise [`detect_cuda_stack`]'s real branch.
///
/// Errors on any non-`Ok` status *before* touching `ensure_ort_initialized` or any download —
/// by construction, the early return happens before any of that code is reached, so a
/// `DriverMissing`/`CudartMissing`/`CudnnMissing` status can never trigger the ~196 MB CUDA
/// ONNX Runtime download.
///
/// [`CudaStackStatus`]: crate::cuda_ep::CudaStackStatus
/// [`detect_cuda_stack`]: crate::cuda_ep::detect_cuda_stack
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(
        dead_code,
        reason = "only called from create_cuda's linux/x86_64 branch; exercised directly by \
                   cross-platform unit tests on every other target"
    )
)]
fn create_cuda_with_status(
    status: crate::cuda_ep::CudaStackStatus,
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    if let Some(err) = cuda_stack_error(status) {
        return Err(err);
    }
    create_cuda_after_stack_ok(policy, models_dir)
}

/// Pure decision over an already-known [`CudaStackStatus`](crate::cuda_ep::CudaStackStatus):
/// `None` when the stack looks complete (caller should proceed), `Some(err)` with the
/// canonical actionable message otherwise.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(
        dead_code,
        reason = "only called from create_cuda_with_status on linux/x86_64; exercised directly \
                   by cross-platform unit tests on every other target"
    )
)]
fn cuda_stack_error(status: crate::cuda_ep::CudaStackStatus) -> Option<EmbedError> {
    if status == crate::cuda_ep::CudaStackStatus::Ok {
        return None;
    }
    let (cause, hint) = crate::cuda_ep::stack_status_cause_and_hint(status);
    // ProviderError (not Internal): maps to core's ProviderUnavailable → exit code 5
    // ("unavailable", specs/05-surfaces.md §5) rather than 1.
    Some(EmbedError::ProviderError {
        provider: "local-cuda".to_string(),
        message: crate::cuda_ep::cuda_unavailable_error(cause, hint),
    })
}

/// Runs once the cheap detection ladder has already reported [`CudaStackStatus::Ok`]: inits
/// the CUDA-flavored ONNX Runtime, ground-truths it with [`probe_cuda`], then constructs the
/// embedder with [`CudaPreference::Required`] (hard error on EP registration failure — no
/// silent CPU fallback for the explicit `local-cuda` provider).
///
/// [`CudaStackStatus::Ok`]: crate::cuda_ep::CudaStackStatus::Ok
/// [`probe_cuda`]: crate::cuda_ep::probe_cuda
/// [`CudaPreference::Required`]: crate::cuda_ep::CudaPreference::Required
#[cfg(feature = "local-onnx")]
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(
        dead_code,
        reason = "only called from create_cuda_with_status on linux/x86_64; exercised directly \
                   by cross-platform unit tests on every other target"
    )
)]
fn create_cuda_after_stack_ok(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    crate::ort_runtime::ensure_ort_initialized(
        crate::ort_runtime::OrtFlavor::Cuda,
        policy.ort_library.as_deref(),
    )?;
    crate::cuda_ep::probe_cuda().map_err(|e| EmbedError::ProviderError {
        provider: "local-cuda".to_string(),
        message: crate::cuda_ep::cuda_unavailable_error(&e, None),
    })?;
    create_onnx_with(policy, models_dir, crate::cuda_ep::CudaPreference::Required)
}

#[cfg(not(feature = "local-onnx"))]
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(
        dead_code,
        reason = "only called from create_cuda_with_status on linux/x86_64; exercised directly \
                   by cross-platform unit tests on every other target"
    )
)]
fn create_cuda_after_stack_ok(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Result<BoxedEmbedder, EmbedError> {
    let _ = (policy, models_dir);
    Err(EmbedError::Internal(
        "provider 'local-cuda' requires the 'local-onnx' feature flag. \
         Rebuild with `--features local-onnx`."
            .to_string(),
    ))
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
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            {
                if let Some(result) = try_local_auto_cuda(policy, models_dir) {
                    return result;
                }
            }
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

/// Automatic ("local") mode's linux/x86_64 CUDA attempt: `Some(Ok(embedder))` once CUDA is
/// loaded and construction is attempted, `None` (having already logged why via `tracing`) when
/// CUDA isn't even attempted — the caller falls through to the normal CPU ONNX path only in
/// the `None` case.
///
/// Falling back to CPU is only sound *before* [`ensure_ort_initialized`] commits the process
/// to the CUDA flavor: once that succeeds, the process-wide runtime is irreversibly the
/// CUDA-flavored library (see `ort_runtime.rs`'s once-only/flavor-committed semantics), so a
/// later construction failure (e.g. the model download itself failing) must be surfaced as an
/// error rather than silently retried as `OrtFlavor::Cpu` — that retry would itself fail with
/// "already initialized as Cuda". This is why only [`detect_cuda_stack`] and
/// [`ensure_ort_initialized`] failures return `None` here; a [`create_onnx_with`] failure after
/// a successful CUDA commit is returned as `Some(Err(..))` instead.
///
/// No [`probe_cuda`] ground-truth check either (unlike `local-cuda`'s
/// [`create_cuda_after_stack_ok`]): once the CUDA-flavored runtime is loaded,
/// [`CudaPreference::Preferred`]'s EP registration is allowed to fail silently at the `ort`
/// session level (see `cuda_ep.rs`'s module docs) — exactly the "nice-to-have GPU, transparent
/// CPU fallback" behavior automatic mode wants.
///
/// [`ensure_ort_initialized`]: crate::ort_runtime::ensure_ort_initialized
/// [`detect_cuda_stack`]: crate::cuda_ep::detect_cuda_stack
/// [`probe_cuda`]: crate::cuda_ep::probe_cuda
/// [`CudaPreference::Preferred`]: crate::cuda_ep::CudaPreference::Preferred
#[cfg(all(feature = "local-onnx", target_os = "linux", target_arch = "x86_64"))]
fn try_local_auto_cuda(
    policy: &EmbeddingPolicy,
    models_dir: Option<&Path>,
) -> Option<Result<BoxedEmbedder, EmbedError>> {
    let status = crate::cuda_ep::detect_cuda_stack();
    if !should_attempt_cuda_auto(status) {
        tracing::info!(
            ?status,
            "no complete NVIDIA/CUDA stack detected; using CPU ONNX Runtime"
        );
        return None;
    }

    if let Err(e) = crate::ort_runtime::ensure_ort_initialized(
        crate::ort_runtime::OrtFlavor::Cuda,
        policy.ort_library.as_deref(),
    ) {
        tracing::warn!(
            error = %e,
            "CUDA-flavored ONNX Runtime initialization failed; falling back to CPU"
        );
        return None;
    }

    tracing::info!(
        "NVIDIA/CUDA stack detected; using CUDA-enabled ONNX Runtime \
         (CUDA preferred, automatic CPU fallback)"
    );
    // The CUDA flavor is now committed for the rest of the process — any failure past this
    // point can no longer fall back to a Cpu-flavored init (see doc comment above), so it is
    // surfaced as an error rather than swallowed into another `None`.
    Some(create_onnx_with(
        policy,
        models_dir,
        crate::cuda_ep::CudaPreference::Preferred,
    ))
}

/// Whether automatic ("local") mode should even attempt the CUDA execution provider, given an
/// already-known [`CudaStackStatus`](crate::cuda_ep::CudaStackStatus) — the linux/x86_64
/// branch's decision, factored out as a pure function so it's unit-testable on every platform
/// (including this macOS dev machine) without a real Linux/CUDA host. `false` for any status
/// short of [`Ok`](crate::cuda_ep::CudaStackStatus::Ok) means [`try_local_auto_cuda`] returns
/// before ever calling [`ensure_ort_initialized`](crate::ort_runtime::ensure_ort_initialized) —
/// i.e. before any CUDA ONNX Runtime download could be triggered.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(
        dead_code,
        reason = "only consulted from try_local_auto_cuda on linux/x86_64; exercised directly \
                   by cross-platform unit tests on every other target"
    )
)]
fn should_attempt_cuda_auto(status: crate::cuda_ep::CudaStackStatus) -> bool {
    status == crate::cuda_ep::CudaStackStatus::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::config::schema::EmbeddingPolicy;

    fn fake_policy(provider: &str, model: &str) -> EmbeddingPolicy {
        EmbeddingPolicy {
            provider: provider.to_string(),
            model: model.to_string(),
            ..Default::default()
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

    // --- local-cuda provider ---------------------------------------------------------------

    #[test]
    fn infer_dim_encoding_local_cuda_pairs() {
        let cases = [
            (
                "local-cuda",
                "pplx-embed-context-v1-0.6b",
                1024,
                VectorEncoding::Binary,
            ),
            (
                "local-cuda",
                "pplx-embed-v1-0.6b",
                1024,
                VectorEncoding::Binary,
            ),
            (
                "local-cuda",
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

            // Parity with 'local'/'local-onnx': switching providers must never change shape.
            let local_policy = fake_policy("local", model);
            let (local_dim, local_encoding) = infer_dim_encoding(&local_policy, &[]).unwrap();
            assert_eq!(dim, local_dim, "{model}: local-cuda dim should match local");
            assert_eq!(
                encoding, local_encoding,
                "{model}: local-cuda encoding should match local"
            );
        }
    }

    #[test]
    fn unknown_provider_error_lists_local_cuda() {
        let policy = fake_policy("nonexistent", "model");
        let err = create_embedder(&policy, &[], None).err().unwrap();
        assert!(
            err.to_string().contains("local-cuda"),
            "unknown-provider error should list 'local-cuda' among supported providers: {err}"
        );

        let err2 = infer_dim_encoding(&policy, &[]).unwrap_err();
        assert!(
            err2.to_string().contains("local-cuda"),
            "infer_dim_encoding's error should also list 'local-cuda': {err2}"
        );
    }

    #[test]
    fn local_cuda_on_unsupported_target_returns_actionable_error() {
        // This test runs on the current (macOS) dev machine — not linux/x86_64 — so
        // `create_cuda` must take its "unsupported target" branch unconditionally.
        let policy = fake_policy("local-cuda", "bge-small-en-v1.5");
        let err = create_embedder(&policy, &[], None).err().unwrap();
        let msg = err.to_string();

        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            assert!(
                msg.contains("Linux x86_64"),
                "error should name the required platform: {msg}"
            );
            assert!(
                msg.contains("'local'") && msg.contains("'local-onnx'"),
                "error should suggest the 'local' and 'local-onnx' alternatives: {msg}"
            );
        }
        // On an actual linux/x86_64 CI runner (no NVIDIA stack), `create_cuda` instead takes
        // the missing-driver path exercised by `create_cuda_with_status_*` below — still an
        // error, just a different (also actionable) message.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            assert!(!msg.is_empty());
        }
    }

    /// Injected-status coverage of `create_cuda_with_status`, runnable on every platform
    /// (including this macOS dev machine): a `DriverMissing`/`CudartMissing`/`CudnnMissing`
    /// status must produce an actionable error naming the missing piece, and — by the
    /// function's own structure (`cuda_stack_error` short-circuits before
    /// `ensure_ort_initialized`/any download is ever reached) — must do so without attempting
    /// the ~196 MB CUDA ONNX Runtime download. There is no real download call left to
    /// accidentally hit in this branch, so "before any download" is a structural guarantee
    /// here, not something exercised via a filesystem assertion.
    #[test]
    fn local_cuda_missing_driver_errors_before_download() {
        use crate::cuda_ep::CudaStackStatus;

        let policy = fake_policy("local-cuda", "bge-small-en-v1.5");
        let err = create_cuda_with_status(CudaStackStatus::DriverMissing, &policy, None)
            .err()
            .unwrap();
        let msg = err.to_string();
        assert!(
            msg.contains("driver"),
            "error should name the missing driver: {msg}"
        );
    }

    #[test]
    fn create_cuda_with_status_cudart_missing_names_cudart() {
        use crate::cuda_ep::CudaStackStatus;

        let policy = fake_policy("local-cuda", "bge-small-en-v1.5");
        let err = create_cuda_with_status(CudaStackStatus::CudartMissing, &policy, None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("libcudart"));
    }

    #[test]
    fn create_cuda_with_status_cudnn_missing_names_cudnn() {
        use crate::cuda_ep::CudaStackStatus;

        let policy = fake_policy("local-cuda", "bge-small-en-v1.5");
        let err = create_cuda_with_status(CudaStackStatus::CudnnMissing, &policy, None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("libcudnn"));
    }

    #[test]
    fn cuda_stack_error_is_none_only_for_ok_status() {
        use crate::cuda_ep::CudaStackStatus;

        assert!(cuda_stack_error(CudaStackStatus::Ok).is_none());
        for status in [
            CudaStackStatus::DriverMissing,
            CudaStackStatus::CudartMissing,
            CudaStackStatus::CudnnMissing,
        ] {
            assert!(
                cuda_stack_error(status).is_some(),
                "{status:?} should produce Some(error)"
            );
        }
    }

    /// Automatic ("local") mode's linux/x86_64 CUDA-attempt decision, tested here as a pure
    /// function so it runs on every platform (including this macOS dev machine) without a real
    /// Linux/CUDA host. `false` for any non-`Ok` status means `try_local_auto_cuda` returns
    /// `None` before ever calling `ensure_ort_initialized` — i.e. before the CUDA ONNX Runtime
    /// download could be triggered — so automatic mode without a driver never attempts it.
    #[test]
    fn local_auto_without_driver_never_downloads_cuda() {
        use crate::cuda_ep::CudaStackStatus;

        assert!(should_attempt_cuda_auto(CudaStackStatus::Ok));
        for status in [
            CudaStackStatus::DriverMissing,
            CudaStackStatus::CudartMissing,
            CudaStackStatus::CudnnMissing,
        ] {
            assert!(
                !should_attempt_cuda_auto(status),
                "{status:?} must not attempt the CUDA path"
            );
        }
    }
}
