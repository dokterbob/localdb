//! Shared HuggingFace file downloader for local ONNX embedders.
//!
//! Provides a reusable, parameterized download stack that both `pplx_onnx` and
//! `pplx_context_onnx` delegate to.  All download logic lives here; each
//! embedder module retains only its own repo constants, file lists, and
//! per-repo guidance text.
//!
//! # Error messages
//!
//! Non-success HTTP responses produce a [`crate::EmbedError::ProviderError`]
//! whose `message` is:
//!
//! ```text
//! HTTP <STATUS> downloading <remote_path><err_suffix>
//! ```
//!
//! Callers supply `err_suffix` via [`HfSpec`] — the string appended verbatim
//! after the base `"HTTP {status} downloading {remote_path}"` — so that each
//! module can preserve its exact pre-extraction error text.
//!
//! - `pplx_context_onnx` suffix: `" from perplexity-ai/… The repo is public …"`
//! - `pplx_onnx` suffix: `" (if 401: set HF_TOKEN env var …)"`
//!
//! This approach keeps the messages byte-identical to the original.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use futures_util::StreamExt;
use tracing::info;

use crate::error::EmbedError;

// ---------------------------------------------------------------------------
// Static configuration struct
// ---------------------------------------------------------------------------

/// Static per-repo configuration for a HuggingFace model.
///
/// All fields are `'static` so the struct can be stored in `const` context
/// and moved into threads without lifetime friction.
pub(crate) struct HfSpec {
    /// HF repo id, e.g. `"perplexity-ai/pplx-embed-context-v1-0.6b"`.
    pub repo: &'static str,
    /// Git revision (commit sha or branch).
    pub revision: &'static str,
    /// Repo-relative paths that must download successfully.
    pub required: &'static [&'static str],
    /// Repo-relative paths silently skipped on 404.
    pub optional: &'static [&'static str],
    /// Appended verbatim after `"HTTP {status} downloading {remote_path}"` in
    /// non-success error messages.  Preserves per-repo actionable text.
    pub err_suffix: &'static str,
}

// ---------------------------------------------------------------------------
// Core download primitive
// ---------------------------------------------------------------------------

/// Download a single file from a HuggingFace repository.
///
/// Skips if `dest` already exists.  On non-success HTTP status, returns
/// `EmbedError::ProviderError` with a message of the form
/// `"HTTP {status} downloading {remote_path}{err_suffix}"`.
pub(crate) async fn download_file(
    client: &reqwest::Client,
    spec: &HfSpec,
    remote_path: &str,
    dest: &Path,
    optional: bool,
    show_progress: bool,
) -> Result<(), EmbedError> {
    if dest.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(EmbedError::Io)?;

    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{remote_path}",
        spec.repo, spec.revision
    );
    info!("downloading {url}");

    let mut req = client.get(&url).header("user-agent", "localdb/0.1");
    if let Ok(token) = std::env::var("HF_TOKEN") {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().await.map_err(EmbedError::Http)?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND && optional {
        info!("skip {remote_path}: 404 (optional file absent)");
        return Ok(());
    }
    if !resp.status().is_success() {
        return Err(EmbedError::ProviderError {
            provider: "huggingface".into(),
            message: format!(
                "HTTP {} downloading {remote_path}{}",
                resp.status(),
                spec.err_suffix
            ),
        });
    }

    let total_mb = resp.content_length().map(|n| n / 1_048_576);
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp).map_err(EmbedError::Io)?;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_reported: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(EmbedError::Http)?;
        file.write_all(&chunk).map_err(EmbedError::Io)?;
        downloaded += chunk.len() as u64;
        if show_progress {
            let mb = downloaded / 1_048_576;
            if mb >= last_reported + 50 {
                match total_mb {
                    Some(t) => info!("{remote_path}: {mb}/{t} MB"),
                    None => info!("{remote_path}: {mb} MB"),
                }
                last_reported = mb;
            }
        }
    }
    drop(file);
    std::fs::rename(&tmp, dest).map_err(EmbedError::Io)?;
    info!("saved {} ({} MB)", dest.display(), downloaded / 1_048_576);
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch download helpers
// ---------------------------------------------------------------------------

/// Build a reqwest client and download all required and optional model files
/// described in `spec` into `model_dir`.
///
/// - Required paths must succeed; errors are propagated immediately.
/// - Optional paths are silently skipped on 404.
pub(crate) async fn ensure_files(
    client_timeout_secs: u64,
    spec: &HfSpec,
    model_dir: &Path,
    show_progress: bool,
) -> Result<(), EmbedError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(client_timeout_secs))
        .build()
        .map_err(EmbedError::Http)?;

    for &f in spec.required {
        download_file(&client, spec, f, &model_dir.join(f), false, show_progress).await?;
    }
    for &f in spec.optional {
        download_file(&client, spec, f, &model_dir.join(f), true, show_progress).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Blocking shim
// ---------------------------------------------------------------------------

/// Synchronously download model files, bridging from sync callers into async.
///
/// Checks `sentinel_relpath` (e.g. `"onnx/model_quantized.onnx"`) before
/// spinning up a runtime; returns immediately if the sentinel already exists.
///
/// Handles three runtime contexts:
///
/// 1. **Multi-thread tokio**: `block_in_place` to avoid blocking the worker.
/// 2. **Current-thread tokio**: spawns a dedicated thread with its own runtime
///    to avoid nesting `block_on` calls.
/// 3. **No runtime**: builds a fresh current-thread runtime inline.
pub(crate) fn download_blocking(
    model_dir: &Path,
    sentinel_relpath: &str,
    spec: &'static HfSpec,
    show_progress: bool,
) -> Result<(), EmbedError> {
    // Split on '/' so this works cross-platform (avoid Path::join on a literal
    // that already contains the separator).
    let sentinel = sentinel_relpath
        .split('/')
        .fold(model_dir.to_path_buf(), |acc, part| acc.join(part));
    if sentinel.exists() {
        return Ok(());
    }

    // Capture model_dir as owned for the move closures below.
    let model_dir_owned: PathBuf = model_dir.to_path_buf();

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| {
                handle.block_on(ensure_files(600, spec, &model_dir_owned, show_progress))
            })
        }
        Ok(_) => {
            // current-thread runtime: can't block_in_place from within it.
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| EmbedError::Internal(format!("create tokio runtime: {e}")))?
                    .block_on(ensure_files(600, spec, &model_dir_owned, show_progress))
            })
            .join()
            .map_err(|_| EmbedError::Internal("download thread panicked".into()))?
        }
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| EmbedError::Internal(format!("create tokio runtime: {e}")))?
            .block_on(ensure_files(600, spec, model_dir, show_progress)),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// Verify that sentinel path splitting works for both flat and nested paths.
    #[test]
    fn sentinel_path_splitting() {
        let model_dir = PathBuf::from("/cache/my-model");

        let sentinel_nested = "onnx/model_quantized.onnx"
            .split('/')
            .fold(model_dir.clone(), |acc, part| acc.join(part));
        assert_eq!(
            sentinel_nested,
            PathBuf::from("/cache/my-model/onnx/model_quantized.onnx")
        );

        let sentinel_flat = "tokenizer.json"
            .split('/')
            .fold(model_dir.clone(), |acc, part| acc.join(part));
        assert_eq!(
            sentinel_flat,
            PathBuf::from("/cache/my-model/tokenizer.json")
        );
    }
}
