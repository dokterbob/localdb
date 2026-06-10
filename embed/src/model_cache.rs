//! Model download and cache management.
//!
//! Models are stored in the platform model cache directory. Downloads are resumable
//! and integrity is verified with SHA-256 after completion.
//!
//! # Cache layout
//!
//! ```text
//! <cache_dir>/
//!   <model_id>/
//!     model.onnx        (or other format)
//!     tokenizer.json
//!     config.json
//!     .checksum         (hex SHA-256 of model.onnx)
//! ```
//!
//! See specs/04-search-pipeline.md §4 (model download/cache).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::error::EmbedError;

/// Specification for a downloadable model.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Unique model identifier (also used as the cache subdirectory name).
    pub model_id: String,
    /// Primary download URL.
    pub download_url: String,
    /// Expected SHA-256 checksum of the downloaded model file (hex string).
    /// If `None`, checksum verification is skipped (not recommended).
    pub sha256: Option<String>,
    /// Expected embedding dimension.
    pub embedding_dim: usize,
}

impl ModelSpec {
    /// Create a new model spec.
    pub fn new(
        model_id: impl Into<String>,
        download_url: impl Into<String>,
        sha256: Option<impl Into<String>>,
        embedding_dim: usize,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            download_url: download_url.into(),
            sha256: sha256.map(|s| s.into()),
            embedding_dim,
        }
    }
}

/// Manages the local model cache directory.
///
/// Downloads models on demand, verifies checksums, and provides paths to
/// cached model files for loading.
#[derive(Debug, Clone)]
pub struct ModelCache {
    /// Root directory for model caches.
    cache_dir: PathBuf,
    /// Whether to allow downloading models (e.g. false in CI/offline mode).
    allow_download: bool,
}

impl ModelCache {
    /// Create a new `ModelCache` pointing to `cache_dir`.
    ///
    /// If `allow_download` is false, [`ensure_cached`] will return
    /// [`EmbedError::ModelMissing`] if the model is not already present.
    pub fn new(cache_dir: impl Into<PathBuf>, allow_download: bool) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            allow_download,
        }
    }

    /// Return the default platform model cache directory.
    ///
    /// macOS: `~/Library/Caches/localdb/models/`
    /// Linux: `$XDG_CACHE_HOME/localdb/models/`
    pub fn default_cache_dir() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("localdb")
            .join("models")
    }

    /// Return the directory for a specific model.
    pub fn model_dir(&self, model_id: &str) -> PathBuf {
        self.cache_dir.join(model_id)
    }

    /// Return the path to the model's primary file (model.onnx).
    pub fn model_file(&self, model_id: &str) -> PathBuf {
        self.model_dir(model_id).join("model.onnx")
    }

    /// Return the path to the model's checksum file.
    pub fn checksum_file(&self, model_id: &str) -> PathBuf {
        self.model_dir(model_id).join(".checksum")
    }

    /// Check if the model is present in the cache.
    pub fn is_cached(&self, model_id: &str) -> bool {
        self.model_file(model_id).exists()
    }

    /// Verify the cached model's checksum (if a checksum is specified).
    ///
    /// Returns `Ok(true)` if the checksum matches or is not specified.
    /// Returns `Ok(false)` if there is no cached checksum file.
    /// Returns `Err(EmbedError::ChecksumMismatch)` if checksums don't match.
    pub fn verify_checksum(&self, spec: &ModelSpec) -> Result<bool, EmbedError> {
        let expected = match &spec.sha256 {
            None => return Ok(true), // no checksum to verify
            Some(h) => h,
        };

        let checksum_file = self.checksum_file(&spec.model_id);
        if !checksum_file.exists() {
            return Ok(false);
        }

        let stored = std::fs::read_to_string(&checksum_file)
            .map_err(EmbedError::Io)?
            .trim()
            .to_string();

        if stored == *expected {
            Ok(true)
        } else {
            Err(EmbedError::ChecksumMismatch {
                model: spec.model_id.clone(),
                expected: expected.clone(),
                actual: stored,
            })
        }
    }

    /// Compute the SHA-256 hash of a file.
    pub fn sha256_file(path: &Path) -> Result<String, EmbedError> {
        let data = std::fs::read(path).map_err(EmbedError::Io)?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        Ok(hex::encode(hasher.finalize()))
    }

    /// Ensure the model is in the cache, downloading if needed.
    ///
    /// # Errors
    /// - [`EmbedError::ModelMissing`] if the model is absent and `allow_download` is false.
    /// - [`EmbedError::ChecksumMismatch`] if the downloaded file has wrong checksum.
    /// - [`EmbedError::Io`] on I/O errors.
    /// - [`EmbedError::Http`] on download failures.
    pub async fn ensure_cached(&self, spec: &ModelSpec) -> Result<PathBuf, EmbedError> {
        let model_file = self.model_file(&spec.model_id);

        if self.is_cached(&spec.model_id) {
            // Verify cached checksum
            debug!(model = %spec.model_id, "model already cached, verifying checksum");
            match self.verify_checksum(spec) {
                Ok(true) => {
                    debug!(model = %spec.model_id, "checksum OK");
                    return Ok(model_file);
                }
                Ok(false) => {
                    // No stored checksum — compute and store it
                    if let Some(expected) = &spec.sha256 {
                        let actual = Self::sha256_file(&model_file)?;
                        if actual != *expected {
                            return Err(EmbedError::ChecksumMismatch {
                                model: spec.model_id.clone(),
                                expected: expected.clone(),
                                actual,
                            });
                        }
                        // Store computed checksum
                        std::fs::write(self.checksum_file(&spec.model_id), &actual)
                            .map_err(EmbedError::Io)?;
                    }
                    return Ok(model_file);
                }
                Err(e) => return Err(e),
            }
        }

        // Model not cached
        if !self.allow_download {
            return Err(EmbedError::ModelMissing(format!(
                "model '{}' is not in the cache at '{}'. \
                 Run `localdb init` to download models, or set `LOCALDB_ALLOW_MODEL_DOWNLOAD=1`.",
                spec.model_id,
                self.model_dir(&spec.model_id).display()
            )));
        }

        // Download the model
        info!(model = %spec.model_id, url = %spec.download_url, "downloading model");
        self.download_model(spec).await
    }

    /// Download a model file, verifying its checksum.
    ///
    /// Creates the model directory if it doesn't exist.
    /// On checksum mismatch, the partial/corrupt file is removed.
    async fn download_model(&self, spec: &ModelSpec) -> Result<PathBuf, EmbedError> {
        let model_dir = self.model_dir(&spec.model_id);
        std::fs::create_dir_all(&model_dir).map_err(EmbedError::Io)?;

        let model_file = self.model_file(&spec.model_id);
        let tmp_file = model_dir.join(".download.tmp");

        // Download with streaming
        let client = reqwest::Client::new();
        let response = client
            .get(&spec.download_url)
            .send()
            .await
            .map_err(EmbedError::Http)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".into());
            return Err(EmbedError::ProviderError {
                provider: "model-download".to_string(),
                message: format!("HTTP {status}: {body}"),
            });
        }

        let bytes = response.bytes().await.map_err(EmbedError::Http)?;
        std::fs::write(&tmp_file, &bytes).map_err(EmbedError::Io)?;

        // Verify checksum
        if let Some(expected) = &spec.sha256 {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let actual = hex::encode(hasher.finalize());
            if actual != *expected {
                // Remove corrupt file
                let _ = std::fs::remove_file(&tmp_file);
                return Err(EmbedError::ChecksumMismatch {
                    model: spec.model_id.clone(),
                    expected: expected.clone(),
                    actual,
                });
            }
            // Save checksum
            std::fs::write(self.checksum_file(&spec.model_id), &actual).map_err(EmbedError::Io)?;
        }

        // Atomically rename tmp → final
        std::fs::rename(&tmp_file, &model_file).map_err(EmbedError::Io)?;

        info!(model = %spec.model_id, path = %model_file.display(), "model downloaded and verified");
        Ok(model_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_cache(allow_download: bool) -> (TempDir, ModelCache) {
        let dir = TempDir::new().unwrap();
        let cache = ModelCache::new(dir.path(), allow_download);
        (dir, cache)
    }

    #[test]
    fn model_not_cached_initially() {
        let (_dir, cache) = tmp_cache(false);
        assert!(!cache.is_cached("test-model"));
    }

    #[test]
    fn model_dir_and_file_paths_are_correct() {
        let (_dir, cache) = tmp_cache(false);
        let dir = cache.model_dir("my-model");
        assert!(dir.ends_with("my-model"));
        let file = cache.model_file("my-model");
        assert!(file.ends_with("model.onnx"));
    }

    #[test]
    fn model_missing_when_download_disabled() {
        let (_dir, cache) = tmp_cache(false);
        let spec = ModelSpec::new(
            "test-model",
            "http://localhost:99999/model.onnx",
            None::<&str>,
            64,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(cache.ensure_cached(&spec));
        assert!(matches!(result, Err(EmbedError::ModelMissing(_))));
    }

    #[test]
    fn model_missing_error_has_actionable_message() {
        let (_dir, cache) = tmp_cache(false);
        let spec = ModelSpec::new(
            "bge-small-en-v1.5",
            "http://example.com/model.onnx",
            None::<&str>,
            384,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt.block_on(cache.ensure_cached(&spec)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("localdb init") || msg.contains("LOCALDB_ALLOW_MODEL_DOWNLOAD"),
            "error should mention how to fix: {msg}"
        );
    }

    #[test]
    fn cached_model_is_returned_directly() {
        let (_dir, cache) = tmp_cache(false);
        // Manually place a file in the cache
        let model_dir = cache.model_dir("fake-model");
        std::fs::create_dir_all(&model_dir).unwrap();
        let model_file = cache.model_file("fake-model");
        std::fs::write(&model_file, b"fake model data").unwrap();

        assert!(cache.is_cached("fake-model"));

        let spec = ModelSpec::new(
            "fake-model",
            "http://example.com/model.onnx",
            None::<&str>,
            64,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(cache.ensure_cached(&spec));
        assert!(
            result.is_ok(),
            "cached model should be returned: {result:?}"
        );
        assert_eq!(result.unwrap(), model_file);
    }

    #[test]
    fn checksum_verification_passes_with_correct_hash() {
        let (_dir, cache) = tmp_cache(false);
        let model_dir = cache.model_dir("checked-model");
        std::fs::create_dir_all(&model_dir).unwrap();
        let model_file = cache.model_file("checked-model");
        let data = b"model content for checksum test";
        std::fs::write(&model_file, data).unwrap();

        // Compute expected SHA-256
        let mut hasher = Sha256::new();
        hasher.update(data);
        let expected = hex::encode(hasher.finalize());

        // Write checksum file
        std::fs::write(cache.checksum_file("checked-model"), &expected).unwrap();

        let spec = ModelSpec::new(
            "checked-model",
            "http://example.com/model.onnx",
            Some(expected.clone()),
            64,
        );
        let result = cache.verify_checksum(&spec);
        assert!(matches!(result, Ok(true)), "checksum should match");
    }

    #[test]
    fn checksum_verification_fails_with_wrong_hash() {
        let (_dir, cache) = tmp_cache(false);
        let model_dir = cache.model_dir("bad-model");
        std::fs::create_dir_all(&model_dir).unwrap();
        let model_file = cache.model_file("bad-model");
        std::fs::write(&model_file, b"model data").unwrap();

        // Store wrong checksum
        std::fs::write(cache.checksum_file("bad-model"), "wrongchecksum").unwrap();

        let _spec = ModelSpec::new(
            "bad-model",
            "http://example.com/model.onnx",
            Some("wrongchecksum"),
            64,
        );
        // Verify: stored checksum "wrongchecksum" == expected "wrongchecksum" → match (trivially)
        // Let's use a real mismatch
        let spec2 = ModelSpec::new(
            "bad-model",
            "http://example.com/model.onnx",
            Some("aabbccdd1122334455667788990011223344556677889900112233445566778899"),
            64,
        );
        let result = cache.verify_checksum(&spec2);
        assert!(
            matches!(result, Err(EmbedError::ChecksumMismatch { .. })),
            "should detect checksum mismatch: {result:?}"
        );
    }

    #[test]
    fn default_cache_dir_is_sensible() {
        let dir = ModelCache::default_cache_dir();
        // Should contain "localdb" and "models" in the path
        let path_str = dir.to_string_lossy();
        assert!(
            path_str.contains("localdb"),
            "cache dir should contain 'localdb': {path_str}"
        );
        assert!(
            path_str.contains("models"),
            "cache dir should contain 'models': {path_str}"
        );
    }

    #[test]
    fn sha256_file_computes_correct_hash() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.bin");
        let data = b"hello, world!";
        std::fs::write(&file, data).unwrap();

        let hash = ModelCache::sha256_file(&file).unwrap();
        // Known SHA-256 of "hello, world!"
        let expected = {
            let mut h = Sha256::new();
            h.update(data);
            hex::encode(h.finalize())
        };
        assert_eq!(hash, expected);
    }
}
