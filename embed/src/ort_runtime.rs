//! Process-wide initialization of the dynamically-loaded (dlopen) ONNX Runtime.
//!
//! # Background (issue #133)
//!
//! `embed`'s `ort` dependency uses the `load-dynamic` feature: our executable links no
//! ONNX Runtime ABI at all, and instead `dlopen`s a shared library at a path we choose at
//! runtime. `embed/build.rs` downloads *Microsoft's official* ONNX Runtime release for the
//! build's target platform, verifies it against a pinned sha256, and embeds it into this
//! binary via `include_bytes!`. This avoids pyke.io's prebuilt archive, whose GCC-14/Ubuntu
//! 24.04 build gave release binaries a `GLIBC_2.38` floor and broke startup on older glibc
//! distros (Linux Mint 21.x, Ubuntu 22.04) — see pykeio/ort#523 (unresolved upstream).
//!
//! [`ensure_ort_initialized`] extracts the embedded library to the user's cache directory
//! on first use (skipping re-extraction if an up-to-date copy is already cached) and calls
//! `ort::init_from` on it, before any other `ort` API is touched. It is idempotent — safe
//! to call from every local-ONNX embedder constructor — and process-wide: only the first
//! call actually configures the ONNX Runtime environment; later calls return the cached
//! outcome.
//!
//! Override with `ORT_DYLIB_PATH` (a power-user / system-package escape hatch honoured
//! directly here) to use a different ONNX Runtime build instead of the embedded one.

use crate::error::EmbedError;

/// Ensure the process-wide ONNX Runtime environment is initialized from the embedded (or
/// `ORT_DYLIB_PATH`-overridden) ONNX Runtime shared library.
///
/// Idempotent: the first call performs extraction + `ort::init_from` + `.commit()` and
/// caches the outcome; every subsequent call (from any local-ONNX embedder constructor)
/// returns that cached `Result` cheaply.
///
/// On build configurations where no ONNX Runtime is embedded — signalled by `build.rs` not
/// emitting the `ort_embedded` cfg, because the `local-onnx` feature is off or the target OS
/// has no embedded runtime — this is a no-op that always returns `Ok(())`. Callers on those
/// configurations never reach ORT-dependent code anyway: `factory.rs` gates its local-ONNX
/// constructors on the same cfg and returns a clean error instead.
#[cfg(ort_embedded)]
pub fn ensure_ort_initialized() -> Result<(), EmbedError> {
    imp::ensure_ort_initialized()
}

/// No-op stub: no ONNX Runtime is embedded for this build configuration.
#[cfg(not(ort_embedded))]
pub fn ensure_ort_initialized() -> Result<(), EmbedError> {
    Ok(())
}

#[cfg(ort_embedded)]
mod imp {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use crate::{error::EmbedError, model_cache::ModelCache};

    /// The embedded ONNX Runtime shared library, baked in at compile time by `build.rs`
    /// (which downloads, verifies, and extracts it into `OUT_DIR` before this file compiles).
    static EMBEDDED_LIB_BYTES: &[u8] = include_bytes!(env!("LOCALDB_ORT_LIB_PATH"));
    /// sha256 of `EMBEDDED_LIB_BYTES`, computed by `build.rs` from the same file.
    const EMBEDDED_LIB_SHA256: &str = env!("LOCALDB_ORT_LIB_SHA256");
    /// ONNX Runtime version embedded (see `build.rs`); also namespaces the cache directory
    /// so upgrading the pinned version doesn't reuse a stale extracted copy.
    const ORT_VERSION: &str = env!("LOCALDB_ORT_VERSION");

    static INIT: OnceLock<Result<(), String>> = OnceLock::new();

    pub(super) fn ensure_ort_initialized() -> Result<(), EmbedError> {
        INIT.get_or_init(|| init_once().map_err(|e| e.to_string()))
            .clone()
            .map_err(EmbedError::Internal)
    }

    fn init_once() -> Result<(), EmbedError> {
        // Power-user / system-package override: dlopen a caller-provided ONNX Runtime
        // instead of the embedded one. Honoured directly (ort itself does not read this
        // env var — `init_from` requires an explicit path).
        if let Ok(path) = std::env::var("ORT_DYLIB_PATH") {
            tracing::info!(path = %path, "ORT_DYLIB_PATH set; using external ONNX Runtime");
            return commit_from(Path::new(&path));
        }

        let dest = cache_lib_path();
        ensure_extracted(&dest)?;
        tracing::info!(
            path = %dest.display(),
            version = ORT_VERSION,
            "initializing embedded ONNX Runtime"
        );
        commit_from(&dest)
    }

    fn commit_from(path: &Path) -> Result<(), EmbedError> {
        let committed = ort::init_from(path)
            .map_err(|e| {
                EmbedError::Internal(format!(
                    "failed to load ONNX Runtime from {}: {e}",
                    path.display()
                ))
            })?
            .commit();
        if !committed {
            // Another code path already initialized the ort environment (e.g. a different
            // ONNX Runtime build) before we got here. Not fatal — inference may still work
            // if that environment is compatible — but worth surfacing since it means our
            // embedded/overridden runtime choice was not actually applied.
            tracing::warn!(
                "ort environment was already configured before embed::ort_runtime could \
                 commit {}; a different ONNX Runtime library may be in use",
                path.display()
            );
        }
        Ok(())
    }

    /// File name of the embedded library (e.g. `libonnxruntime.so.1.24.4` on Linux,
    /// `libonnxruntime.1.24.4.dylib` on macOS), derived from the path `build.rs` recorded.
    fn embedded_lib_filename() -> &'static str {
        Path::new(env!("LOCALDB_ORT_LIB_PATH"))
            .file_name()
            .and_then(|f| f.to_str())
            .expect("LOCALDB_ORT_LIB_PATH is always set by build.rs to a file path")
    }

    /// `<cache_dir>/localdb/ort/<version>/` — mirrors the convention of
    /// [`ModelCache::default_cache_dir`], namespaced under `ort/<version>` rather than
    /// `models` so it never collides with model caches or stale versions after an upgrade.
    fn cache_root() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("localdb")
            .join("ort")
            .join(ORT_VERSION)
    }

    fn cache_lib_path() -> PathBuf {
        cache_root().join(embedded_lib_filename())
    }

    /// Ensure the embedded ONNX Runtime library is present at `dest` with a checksum
    /// matching the embedded copy, (re)writing it if missing or corrupted.
    ///
    /// Pure filesystem logic — no `ort`/dlopen calls — so it's directly unit-testable
    /// without touching process-global ort state.
    fn ensure_extracted(dest: &Path) -> Result<(), EmbedError> {
        if is_up_to_date(dest) {
            return Ok(());
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(EmbedError::Io)?;
        }
        // Atomic write: temp file in the same directory, then rename (matches the
        // model_cache.rs download_model pattern).
        let tmp = tmp_path(dest);
        fs::write(&tmp, EMBEDDED_LIB_BYTES).map_err(EmbedError::Io)?;
        if let Err(err) = fs::rename(&tmp, dest) {
            let _ = fs::remove_file(&tmp);
            return recover_from_rename_failure(err, dest);
        }
        Ok(())
    }

    /// Whether `dest` already holds exactly the embedded library.
    fn is_up_to_date(dest: &Path) -> bool {
        ModelCache::sha256_file(dest)
            .map(|h| h == EMBEDDED_LIB_SHA256)
            .unwrap_or(false)
    }

    /// Staging path for the atomic write, namespaced by process id.
    ///
    /// Two localdb processes hitting a cold cache extract at the same time. Under one shared
    /// `.tmp` name they interleave writes into the same file and then rename whatever mixture
    /// results into place; the sha256 check on the next run would catch it, but only after a
    /// failed load. Distinct staging names make the two extractions independent, and the
    /// rename — the only shared step — atomic.
    fn tmp_path(dest: &Path) -> PathBuf {
        PathBuf::from(format!("{}.{}.tmp", dest.display(), std::process::id()))
    }

    /// Decide whether a failed rename is fatal.
    ///
    /// Windows refuses to replace a file that another process currently has loaded, so a
    /// concurrent localdb running inference sends us here where POSIX would have renamed
    /// silently. Losing the rename only matters if we lost to the *wrong* bytes: `dest`
    /// holding a byte-exact copy of the library we were about to write means the caller's
    /// postcondition already holds and the winner did our work for us. Any other state —
    /// missing, truncated, a different ONNX Runtime build — propagates the original error.
    fn recover_from_rename_failure(err: std::io::Error, dest: &Path) -> Result<(), EmbedError> {
        if is_up_to_date(dest) {
            tracing::debug!(
                path = %dest.display(),
                "another process installed the embedded ONNX Runtime first"
            );
            return Ok(());
        }
        Err(EmbedError::Io(err))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tempfile::TempDir;

        #[test]
        fn extraction_produces_file_with_matching_sha256() {
            let dir = TempDir::new().unwrap();
            let dest = dir.path().join("libonnxruntime.test");

            ensure_extracted(&dest).unwrap();

            assert!(dest.is_file());
            assert_eq!(ModelCache::sha256_file(&dest).unwrap(), EMBEDDED_LIB_SHA256);
        }

        #[test]
        fn corrupted_cached_file_is_reextracted() {
            let dir = TempDir::new().unwrap();
            let dest = dir.path().join("libonnxruntime.test");
            fs::write(&dest, b"not the real onnxruntime library").unwrap();

            ensure_extracted(&dest).unwrap();

            assert_eq!(ModelCache::sha256_file(&dest).unwrap(), EMBEDDED_LIB_SHA256);
        }

        #[test]
        fn already_up_to_date_file_is_not_rewritten() {
            let dir = TempDir::new().unwrap();
            let dest = dir.path().join("libonnxruntime.test");
            ensure_extracted(&dest).unwrap();
            let before = fs::metadata(&dest).unwrap().modified().unwrap();

            std::thread::sleep(std::time::Duration::from_millis(20));
            ensure_extracted(&dest).unwrap();

            let after = fs::metadata(&dest).unwrap().modified().unwrap();
            assert_eq!(
                before, after,
                "file should not be rewritten once its checksum already matches"
            );
        }

        #[test]
        fn creates_missing_parent_directories() {
            let dir = TempDir::new().unwrap();
            let dest = dir
                .path()
                .join("nested")
                .join("dir")
                .join("libonnxruntime.test");

            ensure_extracted(&dest).unwrap();

            assert!(dest.is_file());
        }

        /// The staging file is per-process, so two concurrent extractions cannot write into
        /// each other's.
        #[test]
        fn tmp_path_is_namespaced_by_process_id() {
            let tmp = tmp_path(Path::new("/cache/libonnxruntime.test"));
            let tmp = tmp.to_string_lossy();

            assert!(tmp.ends_with(".tmp"), "{tmp}");
            assert!(
                tmp.contains(&std::process::id().to_string()),
                "staging path {tmp} is shared between processes"
            );
            assert!(tmp.contains("libonnxruntime.test"), "{tmp}");
        }

        /// Losing the rename to a process that installed the *same* library is success: the
        /// postcondition callers rely on — the embedded library is at `dest` — already holds.
        #[test]
        fn rename_failure_succeeds_when_the_winner_wrote_the_same_library() {
            let dir = TempDir::new().unwrap();
            let dest = dir.path().join("libonnxruntime.test");
            fs::write(&dest, EMBEDDED_LIB_BYTES).unwrap();

            recover_from_rename_failure(sharing_violation(), &dest)
                .expect("a destination holding the embedded library is not a failure");
        }

        /// The case that must never be swallowed: reporting success would leave the process
        /// about to `dlopen` a library that is not the one this binary was built against.
        #[test]
        fn rename_failure_propagates_when_the_destination_holds_other_bytes() {
            let dir = TempDir::new().unwrap();
            let dest = dir.path().join("libonnxruntime.test");
            fs::write(&dest, b"a different ONNX Runtime build").unwrap();

            let err = recover_from_rename_failure(sharing_violation(), &dest)
                .expect_err("a mismatched destination must not be reported as success");
            assert!(matches!(err, EmbedError::Io(_)), "{err:?}");
        }

        #[test]
        fn rename_failure_propagates_when_the_destination_is_missing() {
            let dir = TempDir::new().unwrap();
            let dest = dir.path().join("libonnxruntime.test");

            recover_from_rename_failure(sharing_violation(), &dest)
                .expect_err("an absent destination must not be reported as success");
        }

        /// Stand-in for the Windows `ERROR_SHARING_VIOLATION` that motivates the recovery
        /// path. Only its identity as an error matters here, not its kind.
        fn sharing_violation() -> std::io::Error {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the process cannot access the file because it is being used by another process",
            )
        }

        #[test]
        fn cache_lib_path_is_namespaced_by_version_and_filename() {
            let path = cache_lib_path();
            let path_str = path.to_string_lossy();
            assert!(path_str.contains("localdb"));
            assert!(path_str.contains("ort"));
            assert!(path_str.contains(ORT_VERSION));
            assert!(path_str.ends_with(embedded_lib_filename()));
        }
    }
}
