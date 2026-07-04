//! Process-wide initialization of the dynamically-loaded (dlopen) ONNX Runtime.
//!
//! # Background (issue #133)
//!
//! `embed`'s `ort` dependency uses the `load-dynamic` feature: our executable links no
//! ONNX Runtime ABI at all, and instead `dlopen`s a shared library at a path we choose at
//! runtime. This avoids pyke.io's prebuilt archive (`ort`'s `download-binaries` feature),
//! whose GCC-14/Ubuntu 24.04 build gave release binaries a `GLIBC_2.38` floor and broke
//! startup on older glibc distros (Linux Mint 21.x, Ubuntu 22.04) — see pykeio/ort#523
//! (unresolved upstream). `ort/cuda` must never be added either, for the same reason: any
//! CUDA support here is `dlopen`-based against Microsoft's official CUDA execution provider
//! library, never `ort`'s own linked CUDA bindings.
//!
//! # Flavors and runtime download (issue #76)
//!
//! [`OrtFlavor`] names the two supported ONNX Runtime builds: `Cpu` (every supported target)
//! and `Cuda` (linux/x86_64 only). Rather than embedding a single CPU library at *build* time
//! (the pre-#76 approach), [`ensure_ort_initialized`] downloads (or reuses an already-verified,
//! cached copy of) the chosen flavor's sha256-pinned payloads from the flavor table in
//! `ort_download.rs`, then calls `ort::init_from` on the main library before any other `ort`
//! API is touched.
//!
//! Only *Microsoft's official* ONNX Runtime release builds are ever fetched (see
//! `ort_download.rs`'s module doc for the pinned table and verification details).
//!
//! # Once-only / flavor-committed semantics
//!
//! `ort::init_from` + `.commit()` can only meaningfully happen once per process. The first
//! call to [`ensure_ort_initialized`] performs the real work and records both its outcome and
//! the flavor it was called with; every later call is checked against that committed flavor:
//! the *same* flavor returns the cached outcome cheaply (idempotent, safe to call from every
//! local-ONNX embedder constructor), a *different* flavor is a hard error, since the runtime
//! library the process actually loaded cannot retroactively change. The embedder factory is
//! responsible for deciding the flavor once per process (via CUDA availability probing) before
//! constructing the first embedder.
//!
//! # Override precedence
//!
//! Regardless of flavor, in priority order:
//! 1. `ORT_DYLIB_PATH` env var — existing power-user / system-package escape hatch.
//! 2. `ort_library_override` parameter — the future `embedding.ort_library` config value,
//!    threaded in by the factory in a later chunk (callers pass `None` today).
//! 3. Runtime download of the requested flavor's pinned payloads.
//!
//! A caller supplying either override is assumed to know what they're doing (e.g. pointing at
//! a system package or a GPU build the flavor table doesn't know about), so overrides bypass
//! flavor-specific download logic entirely.
//!
//! # Cache layout
//!
//! Downloaded payloads land under `<cache_dir>/localdb/ort/<cache_subdir>/`, where
//! `cache_subdir` is the flavor's namespace from the flavor table (`1.24.4` for CPU builds,
//! `1.24.4-cuda` for the CUDA build) — see `imp::cache_dir_for` below and `ort_download.rs`.
//! This is byte-identical (directory and file name) to the pre-#76 `build.rs`-embedded
//! extraction path for the CPU flavor, so existing user caches remain valid pre-seeds; no
//! re-download occurs.

use std::path::Path;

use crate::error::EmbedError;

/// Which ONNX Runtime build flavor a process commits to.
///
/// The factory decides this once per process (before constructing the first local-ONNX
/// embedder), typically based on config plus a CUDA availability probe (see
/// [`committed_flavor`]'s doc comment). Every embedder constructor then calls
/// [`ensure_ort_initialized`] with that same flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrtFlavor {
    /// CPU-only build (`CPU_LINUX_X64` / `CPU_LINUX_AARCH64` / `CPU_OSX_ARM64` in
    /// `ort_download.rs`). Supported on every target `local-onnx` supports.
    Cpu,
    /// CUDA-accelerated build (`CUDA_LINUX_X64` in `ort_download.rs`). Only downloadable on
    /// linux/x86_64.
    Cuda,
}

/// Ensure the process-wide ONNX Runtime environment is initialized with `flavor`, honouring
/// the override precedence documented at module level (`ORT_DYLIB_PATH`, then
/// `ort_library_override`, then a runtime download of the flavor's pinned payloads).
///
/// Idempotent per flavor: the first call performs the real work and caches the outcome
/// alongside the committed flavor; a later call with the *same* flavor returns that cached
/// `Result` cheaply. A later call with a *different* flavor is a hard error (see module docs).
///
/// On platforms/build configurations where no ONNX Runtime flavor is downloadable (the
/// `local-onnx` feature is disabled, or the target OS has no supported flavor), this is a
/// no-op that always returns `Ok(())` — callers on those configurations never reach
/// ORT-dependent code anyway (see `factory.rs`'s `local-onnx`-gated call sites).
#[cfg(all(feature = "local-onnx", any(target_os = "linux", target_os = "macos")))]
pub fn ensure_ort_initialized(
    flavor: OrtFlavor,
    ort_library_override: Option<&Path>,
) -> Result<(), EmbedError> {
    imp::ensure_ort_initialized(flavor, ort_library_override)
}

/// No-op stub: no ONNX Runtime flavor is downloadable for this build configuration.
#[cfg(not(all(feature = "local-onnx", any(target_os = "linux", target_os = "macos"))))]
pub fn ensure_ort_initialized(
    _flavor: OrtFlavor,
    _ort_library_override: Option<&Path>,
) -> Result<(), EmbedError> {
    Ok(())
}

/// The flavor the process-wide ONNX Runtime has committed to, if [`ensure_ort_initialized`] has
/// completed successfully at least once; `None` if it has never been called (or every call so
/// far has failed).
///
/// Consumed by the CUDA availability probe (`factory.rs`, wired up in a later chunk of issue
/// #76) to fail cleanly — rather than silently reusing an already-loaded flavor — when the
/// caller asks to initialize as CUDA but the process has already committed to CPU (or vice
/// versa).
#[cfg(all(feature = "local-onnx", any(target_os = "linux", target_os = "macos")))]
#[allow(dead_code)]
pub(crate) fn committed_flavor() -> Option<OrtFlavor> {
    imp::committed_flavor()
}

/// No-op stub counterpart of [`committed_flavor`] for build configurations without a
/// downloadable ONNX Runtime: never committed, since [`ensure_ort_initialized`] never does
/// real work here.
#[cfg(not(all(feature = "local-onnx", any(target_os = "linux", target_os = "macos"))))]
#[allow(dead_code)]
pub(crate) fn committed_flavor() -> Option<OrtFlavor> {
    None
}

#[cfg(all(feature = "local-onnx", any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::{
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use super::OrtFlavor;
    use crate::{
        error::EmbedError,
        ort_download::{self, RemoteRuntime},
    };

    /// `(committed flavor, init outcome)`, set once by the first call (whether it succeeded or
    /// failed). Recording the flavor alongside the outcome lets later calls detect a flavor
    /// mismatch (see [`check_flavor`]) even when the first call itself returned an error.
    static INIT: OnceLock<(OrtFlavor, Result<(), String>)> = OnceLock::new();

    pub(super) fn ensure_ort_initialized(
        flavor: OrtFlavor,
        ort_library_override: Option<&Path>,
    ) -> Result<(), EmbedError> {
        let (committed, result) = INIT.get_or_init(|| {
            let outcome = init_once(flavor, ort_library_override).map_err(|e| e.to_string());
            (flavor, outcome)
        });
        check_flavor(*committed, flavor)?;
        result.clone().map_err(EmbedError::Internal)
    }

    pub(super) fn committed_flavor() -> Option<OrtFlavor> {
        INIT.get().map(|(flavor, _)| *flavor)
    }

    /// Pure flavor-consistency check, factored out of [`ensure_ort_initialized`] so it's
    /// unit-testable without touching the process-global `OnceLock` or `ort` itself — real
    /// `ort::init_from` can only meaningfully run once per test process, so a test can't
    /// actually exercise a second, differently-flavored `ensure_ort_initialized` call.
    fn check_flavor(committed: OrtFlavor, requested: OrtFlavor) -> Result<(), EmbedError> {
        if committed == requested {
            return Ok(());
        }
        Err(EmbedError::Internal(format!(
            "ONNX Runtime already initialized with the {committed:?} flavor; cannot \
             re-initialize as {requested:?} — the runtime library is loaded once per process"
        )))
    }

    fn init_once(flavor: OrtFlavor, ort_library_override: Option<&Path>) -> Result<(), EmbedError> {
        let env_override = std::env::var("ORT_DYLIB_PATH").ok().map(PathBuf::from);
        match resolve_lib_source(env_override, ort_library_override, flavor) {
            LibSource::Env(path) => {
                tracing::info!(
                    path = %path.display(),
                    "ORT_DYLIB_PATH set; using external ONNX Runtime"
                );
                commit_from(&path)
            }
            LibSource::Config(path) => {
                tracing::info!(
                    path = %path.display(),
                    "embedding.ort_library set; using external ONNX Runtime"
                );
                commit_from(path)
            }
            LibSource::Download(flavor) => download_and_commit(flavor),
        }
    }

    /// Where a `dlopen`-able ONNX Runtime library should come from, in override-precedence
    /// order. A pure function over already-read inputs (not `std::env` itself), so precedence
    /// is directly unit-testable without any env-var mutation (which would race across tests
    /// run in parallel within this process).
    #[derive(Debug, PartialEq, Eq)]
    enum LibSource<'a> {
        /// `ORT_DYLIB_PATH` env var — existing power-user/system-package escape hatch.
        Env(PathBuf),
        /// `embedding.ort_library` config value (threaded in by the factory in a later chunk;
        /// always `None` today).
        Config(&'a Path),
        /// No override: download (or reuse the already-verified cached copy of) the flavor
        /// table entry for `OrtFlavor`.
        Download(OrtFlavor),
    }

    fn resolve_lib_source(
        env: Option<PathBuf>,
        config: Option<&Path>,
        flavor: OrtFlavor,
    ) -> LibSource<'_> {
        if let Some(path) = env {
            LibSource::Env(path)
        } else if let Some(path) = config {
            LibSource::Config(path)
        } else {
            LibSource::Download(flavor)
        }
    }

    fn download_and_commit(flavor: OrtFlavor) -> Result<(), EmbedError> {
        match flavor {
            OrtFlavor::Cpu => {
                let rt = ort_download::cpu_flavor_for_target()?;
                download_rt_and_commit(rt)
            }
            OrtFlavor::Cuda => cuda_download_and_commit(),
        }
    }

    /// The CUDA flavor only has a pinned table entry (and is only ever meaningfully requested
    /// by the factory) on linux/x86_64 — see `ort_download::CUDA_LINUX_X64`.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn cuda_download_and_commit() -> Result<(), EmbedError> {
        download_rt_and_commit(&ort_download::CUDA_LINUX_X64)
    }

    /// On every other target, the factory should never route a CUDA request here.
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    fn cuda_download_and_commit() -> Result<(), EmbedError> {
        Err(EmbedError::Internal(
            "the CUDA ONNX Runtime flavor is only available on linux/x86_64; the embedder \
             factory should never request it on this target"
                .to_string(),
        ))
    }

    fn download_rt_and_commit(rt: &'static RemoteRuntime) -> Result<(), EmbedError> {
        let dir = cache_dir_for(rt);
        ort_download::ensure_downloaded(rt, &dir)?;
        let lib_path = dir.join(main_lib_filename(rt));
        tracing::info!(
            path = %lib_path.display(),
            cache_subdir = rt.cache_subdir,
            "initializing ONNX Runtime (downloaded)"
        );
        commit_from(&lib_path)
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
            // downloaded/overridden runtime choice was not actually applied.
            tracing::warn!(
                "ort environment was already configured before embed::ort_runtime could \
                 commit {}; a different ONNX Runtime library may be in use",
                path.display()
            );
        }
        Ok(())
    }

    /// `<cache_dir>/localdb/ort/<rt.cache_subdir>/` — mirrors the convention of
    /// [`crate::model_cache::ModelCache::default_cache_dir`], namespaced under
    /// `ort/<cache_subdir>` (rather than `models`) so it never collides with model caches, and
    /// so CPU vs. CUDA builds (or a future ONNX Runtime version bump) never collide with each
    /// other.
    ///
    /// CRITICAL: for the CPU flavor this resolves to exactly `<cache>/localdb/ort/1.24.4/` —
    /// byte-identical to the directory the pre-#76 `build.rs`-embedded extraction used — so
    /// existing user caches from before this chunk remain valid pre-seeds and
    /// `ort_download::ensure_downloaded` takes its network-free fast path.
    fn cache_dir_for(rt: &RemoteRuntime) -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("localdb")
            .join("ort")
            .join(rt.cache_subdir)
    }

    /// File name of `rt`'s main runtime library (e.g. `libonnxruntime.so.1.24.4` on Linux,
    /// `libonnxruntime.1.24.4.dylib` on macOS), derived from its first payload's in-tar path.
    /// The flavor table always lists the main runtime library first (see `ort_download.rs`);
    /// any additional payloads (e.g. the CUDA execution provider libraries) are discovered by
    /// ONNX Runtime itself via `dladdr` once it's loaded from the same directory.
    fn main_lib_filename(rt: &RemoteRuntime) -> &'static str {
        Path::new(rt.payloads[0].0)
            .file_name()
            .and_then(|f| f.to_str())
            .expect("payload path always has a file name")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn same_flavor_is_ok() {
            check_flavor(OrtFlavor::Cpu, OrtFlavor::Cpu).unwrap();
            check_flavor(OrtFlavor::Cuda, OrtFlavor::Cuda).unwrap();
        }

        #[test]
        fn second_init_with_different_flavor_errors() {
            let err = check_flavor(OrtFlavor::Cpu, OrtFlavor::Cuda).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("Cpu"),
                "message should name the committed flavor: {msg}"
            );
            assert!(
                msg.contains("Cuda"),
                "message should name the requested flavor: {msg}"
            );
        }

        #[test]
        fn override_precedence_env_over_config() {
            let env_path = PathBuf::from("/env/libonnxruntime.so");
            let config_path = Path::new("/config/libonnxruntime.so");

            match resolve_lib_source(Some(env_path.clone()), Some(config_path), OrtFlavor::Cpu) {
                LibSource::Env(p) => assert_eq!(p, env_path),
                other => panic!("expected LibSource::Env, got {other:?}"),
            }
        }

        #[test]
        fn override_precedence_config_over_download() {
            let config_path = Path::new("/config/libonnxruntime.so");

            match resolve_lib_source(None, Some(config_path), OrtFlavor::Cuda) {
                LibSource::Config(p) => assert_eq!(p, config_path),
                other => panic!("expected LibSource::Config, got {other:?}"),
            }
        }

        #[test]
        fn no_override_downloads_requested_flavor() {
            match resolve_lib_source(None, None, OrtFlavor::Cuda) {
                LibSource::Download(f) => assert_eq!(f, OrtFlavor::Cuda),
                other => panic!("expected LibSource::Download, got {other:?}"),
            }
            match resolve_lib_source(None, None, OrtFlavor::Cpu) {
                LibSource::Download(f) => assert_eq!(f, OrtFlavor::Cpu),
                other => panic!("expected LibSource::Download, got {other:?}"),
            }
        }

        #[test]
        fn cuda_cache_dir_is_version_cuda_namespaced() {
            let cuda_dir = cache_dir_for(&ort_download::CUDA_LINUX_X64);
            assert!(
                cuda_dir.ends_with("ort/1.24.4-cuda"),
                "cuda cache dir should end with ort/1.24.4-cuda, got {}",
                cuda_dir.display()
            );

            let cpu_dir = cache_dir_for(&ort_download::CPU_LINUX_X64);
            assert!(
                cpu_dir.ends_with("ort/1.24.4"),
                "cpu cache dir should end with ort/1.24.4, got {}",
                cpu_dir.display()
            );
        }

        #[test]
        fn main_lib_filename_matches_expected_names() {
            assert_eq!(
                main_lib_filename(&ort_download::CPU_LINUX_X64),
                "libonnxruntime.so.1.24.4"
            );
            assert_eq!(
                main_lib_filename(&ort_download::CPU_OSX_ARM64),
                "libonnxruntime.1.24.4.dylib"
            );
            assert_eq!(
                main_lib_filename(&ort_download::CUDA_LINUX_X64),
                "libonnxruntime.so.1.24.4"
            );
        }
    }
}
