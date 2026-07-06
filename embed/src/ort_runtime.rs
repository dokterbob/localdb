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

/// Actionable-error stub: `local-onnx` is enabled, but this target (e.g. a hypothetical Windows
/// build) has no entry in `ort_download`'s flavor table — only linux/x86_64, linux/aarch64, and
/// macos/aarch64 are downloadable (see `ort_download::cpu_flavor_for`). Unlike the fully no-op
/// stub below, this must not silently return `Ok(())`: a caller with `local-onnx` enabled *will*
/// go on to touch other `ort` APIs, which abort/panic under `load-dynamic` if the environment was
/// never initialized (see this module's precondition docs) — silently OK-ing here would trade a
/// clean error now for a hard panic later.
///
/// Still honors the `ORT_DYLIB_PATH` env var / `ort_library_override`: calling `ort::init_from`
/// on an explicit path is sound on every target `ort` itself compiles for (only the
/// *downloadable flavor table* is linux/macos-only), so an override lets `local-onnx` work here
/// too, same as every other target.
///
/// Deliberately minimal: no once-only/flavor-commit state machine like `imp` below (a second call
/// just re-invokes `ort::init_from`, which `ort` itself already treats as an idempotent no-op —
/// see `commit()`'s `bool` return, ignored here). This whole branch is never exercised by CI (no
/// Windows target), so it's kept as simple as possible rather than risk rotting unnoticed.
#[cfg(all(
    feature = "local-onnx",
    not(any(target_os = "linux", target_os = "macos"))
))]
pub fn ensure_ort_initialized(
    _flavor: OrtFlavor,
    ort_library_override: Option<&Path>,
) -> Result<(), EmbedError> {
    let override_path = std::env::var_os("ORT_DYLIB_PATH")
        .map(std::path::PathBuf::from)
        .or_else(|| ort_library_override.map(Path::to_path_buf));

    match override_path {
        Some(path) => {
            ort::init_from(&path)
                .map_err(|e| {
                    EmbedError::Internal(format!(
                        "failed to load ONNX Runtime from {}: {e}",
                        path.display()
                    ))
                })?
                .commit();
            Ok(())
        }
        None => Err(EmbedError::Internal(format!(
            "localdb's `local-onnx` feature has no downloadable ONNX Runtime build for target \
             {os}/{arch}. Supported: linux/x86_64, linux/aarch64, macos/aarch64. Set \
             ORT_DYLIB_PATH to the path of a local ONNX Runtime shared library to use \
             `local-onnx` on this target.",
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
        ))),
    }
}

/// No-op stub: `local-onnx` is disabled, so no code path ever touches `ort` at all — unreachable
/// by construction (see `factory.rs`'s `local-onnx`-gated call sites), so `Ok(())` is safe here
/// regardless of target.
#[cfg(not(feature = "local-onnx"))]
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
        sync::Mutex,
    };

    use super::OrtFlavor;
    use crate::{
        error::EmbedError,
        ort_download::{self, RemoteRuntime},
    };

    /// `(committed flavor, init outcome)`, set only once real `ort::init_from` has actually
    /// been invoked (see [`InitAttempt`] and [`record_attempt`]) — *not* simply on the first
    /// call to [`ensure_ort_initialized`]. Recording the flavor alongside the outcome lets
    /// later calls detect a flavor mismatch (see [`check_flavor`]) even when the committing
    /// call itself returned an error (e.g. a bad `dlopen` path).
    ///
    /// A plain `OnceLock` cannot express this: its closure runs (and its result is cached)
    /// exactly once regardless of outcome, which would permanently "poison" the process
    /// against a *different* flavor even when the first call failed for a reason that never
    /// touched `ort` at all (a failed download, say) — see the module-level doc's "Once-only"
    /// section and [`record_attempt`]'s doc comment for the failure mode this avoids: a failed
    /// CUDA *download* must not block a subsequent CPU fallback in the same process.
    static INIT: Mutex<Option<(OrtFlavor, Result<(), String>)>> = Mutex::new(None);

    pub(super) fn ensure_ort_initialized(
        flavor: OrtFlavor,
        ort_library_override: Option<&Path>,
    ) -> Result<(), EmbedError> {
        let mut guard = INIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((committed, result)) = guard.as_ref() {
            check_flavor(*committed, flavor)?;
            return result.clone().map_err(EmbedError::Internal);
        }
        let attempt = init_once(flavor, ort_library_override);
        record_attempt(&mut guard, flavor, attempt)
    }

    pub(super) fn committed_flavor() -> Option<OrtFlavor> {
        INIT.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|(flavor, _)| *flavor)
    }

    /// Outcome of one [`init_once`] attempt, distinguishing whether `ort::init_from` was
    /// actually invoked from whether the attempt failed before ever reaching it.
    #[derive(Debug)]
    enum InitAttempt {
        /// `ort::init_from` was actually called — even if it returned an error (e.g. `dlopen`
        /// failed to load the path). This has real, irreversible process-wide side effects
        /// (per the `ort` docs: the environment can only be configured once), so the outcome
        /// must be committed permanently regardless of success or failure.
        Committed(Result<(), String>),
        /// Failed before `ort::init_from` was ever touched (resolving the lib source, or
        /// downloading/verifying the flavor's payloads). No process-wide `ort` state changed,
        /// so this must *not* be committed — a later call, even with a different flavor, can
        /// retry cleanly.
        NotAttempted(String),
    }

    /// Applies one [`init_once`] outcome to the process-wide `INIT` state. Factored out of
    /// [`ensure_ort_initialized`] so the commit-vs-no-commit decision is unit-testable without
    /// touching `ort`, the filesystem, or the network.
    ///
    /// This is the fix for the failed-CUDA-init-must-not-poison-CPU-fallback problem: a
    /// [`InitAttempt::NotAttempted`] outcome (e.g. the CUDA runtime download failed) leaves
    /// `guard` as `None`, so a subsequent call — e.g. the factory's automatic-mode CPU
    /// fallback, requesting [`OrtFlavor::Cpu`] instead — is not rejected by [`check_flavor`]
    /// and gets a fresh attempt of its own. Only [`InitAttempt::Committed`] (real `ort::init_from`
    /// invocation, e.g. after a download succeeded) permanently records the flavor, matching
    /// the fact that a real `dlopen` cannot be undone or retried with a different library.
    fn record_attempt(
        guard: &mut Option<(OrtFlavor, Result<(), String>)>,
        flavor: OrtFlavor,
        attempt: InitAttempt,
    ) -> Result<(), EmbedError> {
        match attempt {
            InitAttempt::Committed(result) => {
                *guard = Some((flavor, result.clone()));
                result.map_err(EmbedError::Internal)
            }
            InitAttempt::NotAttempted(err) => Err(EmbedError::Internal(err)),
        }
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
             re-initialize as {requested:?} — the runtime library is loaded once per process. \
             This process already initialized the {committed:?} ONNX Runtime; run the \
             {requested:?}-flavored operation in a separate invocation."
        )))
    }

    fn init_once(flavor: OrtFlavor, ort_library_override: Option<&Path>) -> InitAttempt {
        let env_override = std::env::var("ORT_DYLIB_PATH").ok().map(PathBuf::from);
        match resolve_lib_source(env_override, ort_library_override, flavor) {
            LibSource::Env(path) => {
                tracing::info!(
                    path = %path.display(),
                    "ORT_DYLIB_PATH set; using external ONNX Runtime"
                );
                InitAttempt::Committed(commit_from(&path).map_err(|e| e.to_string()))
            }
            LibSource::Config(path) => {
                tracing::info!(
                    path = %path.display(),
                    "embedding.ort_library set; using external ONNX Runtime"
                );
                InitAttempt::Committed(commit_from(path).map_err(|e| e.to_string()))
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

    fn download_and_commit(flavor: OrtFlavor) -> InitAttempt {
        match flavor {
            OrtFlavor::Cpu => match ort_download::cpu_flavor_for_target() {
                Ok(rt) => download_rt_and_commit(rt),
                // Unsupported (os, arch): never touched `ort::init_from`, so this must not
                // poison the process against a later, differently-flavored attempt.
                Err(e) => InitAttempt::NotAttempted(e.to_string()),
            },
            OrtFlavor::Cuda => cuda_download_and_commit(),
        }
    }

    /// The CUDA flavor only has a pinned table entry (and is only ever meaningfully requested
    /// by the factory) on linux/x86_64 — see `ort_download::CUDA_LINUX_X64`.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn cuda_download_and_commit() -> InitAttempt {
        download_rt_and_commit(&ort_download::CUDA_LINUX_X64)
    }

    /// On every other target, the factory should never route a CUDA request here.
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    fn cuda_download_and_commit() -> InitAttempt {
        InitAttempt::NotAttempted(
            "the CUDA ONNX Runtime flavor is only available on linux/x86_64; the embedder \
             factory should never request it on this target"
                .to_string(),
        )
    }

    fn download_rt_and_commit(rt: &'static RemoteRuntime) -> InitAttempt {
        let dir = cache_dir_for(rt);
        // A download/verification failure here (network error, sha256 mismatch, etc.) never
        // touches `ort::init_from` — report it as `NotAttempted` so it doesn't permanently
        // poison the process against a later, differently-flavored attempt (e.g. the
        // factory's automatic-mode CPU fallback after a CUDA download failure).
        if let Err(e) = ort_download::ensure_downloaded(rt, &dir) {
            return InitAttempt::NotAttempted(e.to_string());
        }
        let lib_path = dir.join(main_lib_filename(rt));
        tracing::info!(
            path = %lib_path.display(),
            cache_subdir = rt.cache_subdir,
            "initializing ONNX Runtime (downloaded)"
        );
        InitAttempt::Committed(commit_from(&lib_path).map_err(|e| e.to_string()))
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

        // --- record_attempt state machine (the failed-CUDA-init-must-not-poison-CPU-fallback
        // fix) --------------------------------------------------------------------------------
        //
        // These exercise `record_attempt` directly against a local `guard` variable — never the
        // real process-wide `INIT` static — so they're fully isolated from every other test in
        // this binary (including the real-`ort` integration tests in `tests/*.rs`, which run in
        // separate binaries anyway) and require no network or filesystem access.

        #[test]
        fn not_attempted_outcome_does_not_commit() {
            let mut guard = None;
            let err = record_attempt(
                &mut guard,
                OrtFlavor::Cuda,
                InitAttempt::NotAttempted("download failed: network unreachable".to_string()),
            )
            .unwrap_err();
            assert!(err.to_string().contains("network unreachable"));
            assert!(
                guard.is_none(),
                "a pre-init (download/resolve) failure must not commit a flavor"
            );
        }

        #[test]
        fn committed_outcome_commits_even_on_failure() {
            let mut guard = None;
            let err = record_attempt(
                &mut guard,
                OrtFlavor::Cuda,
                InitAttempt::Committed(Err("dlopen failed".to_string())),
            )
            .unwrap_err();
            assert!(err.to_string().contains("dlopen failed"));
            assert_eq!(
                guard,
                Some((OrtFlavor::Cuda, Err("dlopen failed".to_string()))),
                "a Committed outcome must be recorded permanently, even when it failed"
            );
        }

        #[test]
        fn committed_outcome_commits_on_success() {
            let mut guard = None;
            record_attempt(&mut guard, OrtFlavor::Cpu, InitAttempt::Committed(Ok(()))).unwrap();
            assert_eq!(guard, Some((OrtFlavor::Cpu, Ok(()))));
        }

        #[test]
        fn cuda_download_failure_does_not_block_subsequent_cpu_fallback() {
            // The scenario this whole state machine exists for: `create_local_auto` attempts
            // CUDA first (download fails, e.g. no network) and must be able to fall back to a
            // normal CPU init in the very same process — the CUDA attempt must never have
            // committed the flavor.
            let mut guard = None;

            let cuda_err = record_attempt(
                &mut guard,
                OrtFlavor::Cuda,
                InitAttempt::NotAttempted(
                    "ONNX Runtime download failed: connection reset".to_string(),
                ),
            )
            .unwrap_err();
            assert!(cuda_err.to_string().contains("connection reset"));
            assert!(
                guard.is_none(),
                "failed CUDA download must leave the flavor uncommitted"
            );

            // The CPU fallback attempt now runs cleanly — not rejected by a stale Cuda commit.
            record_attempt(&mut guard, OrtFlavor::Cpu, InitAttempt::Committed(Ok(()))).unwrap();
            assert_eq!(
                guard,
                Some((OrtFlavor::Cpu, Ok(()))),
                "CPU fallback must be able to commit after a failed CUDA download"
            );
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
            assert!(
                msg.contains("separate invocation"),
                "message should give the actionable hint to retry in a separate process: {msg}"
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
