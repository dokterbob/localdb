//! NVIDIA/CUDA detection and execution-provider (EP) dispatch for the local-ONNX embedder.
//!
//! # Background (issue #96/#133 heritage)
//!
//! Never add `ort`'s `cuda` cargo feature, `download-binaries`, or any `api-*` default
//! feature (see `ort_download.rs`'s module doc for the full rationale). `ort = 2.0.0-rc.12`
//! with `load-dynamic` is already sufficient: [`ort::ep::CUDA`] and its
//! [`ExecutionProvider::register`](ort::ep::ExecutionProvider::register) compile fine under
//! `load-dynamic` (see `ort::ep::cuda`'s `register` method, gated
//! `#[cfg(any(feature = "load-dynamic", feature = "cuda"))]` upstream) — CUDA support here means
//! `dlopen`-ing Microsoft's official CUDA execution provider library
//! (`libonnxruntime_providers_cuda.so`, downloaded by `ort_download.rs`), never `ort`'s own
//! linked CUDA bindings.
//!
//! # Detection ladder
//!
//! Querying ONNX Runtime itself (attempting EP registration) is the only *fully* reliable way
//! to know whether CUDA will actually work, but it requires the CUDA-flavored runtime to
//! already be `dlopen`ed (see [`probe_cuda`]) — expensive and only possible after
//! [`crate::ort_runtime::ensure_ort_initialized`] has committed the process to
//! [`OrtFlavor::Cuda`](crate::ort_runtime::OrtFlavor::Cuda). So the factory (a later chunk)
//! consults a cheaper, file-level ladder first ([`detect_cuda_stack`]) to decide *whether to
//! attempt* the CUDA flavor at all, then falls back to [`probe_cuda`] as ground truth once
//! that flavor is loaded:
//!
//! 1. **Driver** — is an NVIDIA driver present at all? Checked via `/proc/driver/nvidia/version`,
//!    `/dev/nvidiactl`, or `ldconfig -p` listing `libcuda.so.1` (libcuda ships with the driver;
//!    the file checks additionally work inside `docker --gpus` containers where `ldconfig` may
//!    not list host-mounted libraries).
//! 2. **CUDA runtime** — does `ldconfig -p` list `libcudart.so.12`?
//! 3. **cuDNN** — does `ldconfig -p` list `libcudnn.so.9`? This is the most commonly missing
//!    piece: cuDNN ships separately from both the driver and the CUDA toolkit metapackages.
//!
//! A stack that passes all three rungs can still fail at the real (`ort`) probe — driver/library
//! version skew, a broken install, etc. — which is why rung 3 (the ladder) is a pre-filter, not
//! a replacement for [`probe_cuda`].

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::path::Path;

/// How eagerly the embedder factory should attempt to use the CUDA execution provider.
///
/// The factory (wired up in a later chunk of issue #96) maps this from the configured
/// provider: `local-onnx` always requests [`CudaPreference::Disabled`]; `local` (automatic mode)
/// requests [`CudaPreference::Preferred`]; a dedicated `local-cuda` provider requests
/// [`CudaPreference::Required`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CudaPreference {
    /// CPU-only: never register the CUDA execution provider. Used by the `local-onnx` provider
    /// (and implicitly by every test that constructs a local embedder — CUDA should never be
    /// attempted just because a test happens to run on a CUDA-capable machine).
    Disabled,
    /// Register the CUDA execution provider, but tolerate a failed registration: `ort` logs the
    /// failure internally and the session silently falls back to running on CPU (see
    /// [`dispatch_list`]'s doc comment). Used by `local` (automatic) mode, where CUDA is a
    /// nice-to-have, not a requirement.
    Preferred,
    /// Register the CUDA execution provider with `.error_on_failure()`: a failed registration is
    /// surfaced as a hard error at session-build time rather than silently falling back to CPU.
    /// Used by the explicit `local-cuda` provider, where the caller specifically asked for GPU
    /// acceleration and silent CPU fallback would be a surprise.
    Required,
}

/// Outcome of the cheap, file-level CUDA stack detection ladder (see module docs). Ordered from
/// "least of the stack present" to "everything the ladder can check is present" — later
/// variants imply earlier rungs passed.
///
/// On targets other than linux/x86_64, only [`CudaStackStatus::DriverMissing`] is ever actually
/// constructed (by the [`detect_cuda_stack`] stub) — the other variants are still fully exercised
/// by this module's unit tests via [`cuda_stack_status`] directly.
#[allow(
    dead_code,
    reason = "not all variants are constructed outside linux/x86_64 non-test builds; exercised by unit tests"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CudaStackStatus {
    /// Rung 1 failed: no NVIDIA driver detected by any of the checks in [`cuda_stack_status`].
    DriverMissing,
    /// Rung 1 passed but rung 2 failed: no `libcudart.so.12` found via `ldconfig -p`.
    CudartMissing,
    /// Rungs 1-2 passed but rung 3 failed: no `libcudnn.so.9` found via `ldconfig -p`.
    CudnnMissing,
    /// All three rungs passed. Does **not** guarantee the ONNX Runtime CUDA execution provider
    /// will actually register successfully — see [`probe_cuda`] for the ground-truth check.
    Ok,
}

/// Plain-old-data inputs to [`cuda_stack_status`], factored out so the ladder's decision logic
/// is unit-testable without touching the filesystem or spawning a process.
#[allow(
    dead_code,
    reason = "constructed by detect_cuda_stack on linux/x86_64; exercised by unit tests elsewhere"
)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CudaProbeInputs {
    /// Whether `/proc/driver/nvidia/version` exists.
    pub proc_driver_version_exists: bool,
    /// Whether `/dev/nvidiactl` exists.
    pub dev_nvidiactl_exists: bool,
    /// Captured stdout of `ldconfig -p`, or `None` if the process could not be spawned (missing
    /// binary, permission error, etc. — treated as "no information", not as "nothing installed").
    pub ldconfig_output: Option<String>,
}

/// Pure decision function for the detection ladder described in the module docs. Takes already
/// -collected inputs (rather than reading the filesystem or spawning a process itself) so it's
/// directly unit-testable.
#[allow(
    dead_code,
    reason = "called by detect_cuda_stack on linux/x86_64; exercised by unit tests elsewhere"
)]
pub(crate) fn cuda_stack_status(inputs: &CudaProbeInputs) -> CudaStackStatus {
    let driver_present = inputs.proc_driver_version_exists
        || inputs.dev_nvidiactl_exists
        || ldconfig_lists(inputs.ldconfig_output.as_deref(), "libcuda.so.1");
    if !driver_present {
        return CudaStackStatus::DriverMissing;
    }

    if !ldconfig_lists(inputs.ldconfig_output.as_deref(), "libcudart.so.12") {
        return CudaStackStatus::CudartMissing;
    }

    if !ldconfig_lists(inputs.ldconfig_output.as_deref(), "libcudnn.so.9") {
        return CudaStackStatus::CudnnMissing;
    }

    CudaStackStatus::Ok
}

/// True iff any line of `ldconfig -p` output contains `needle` as a substring. `None` output
/// (spawn failed) is treated as "cannot confirm", not as a match.
///
/// Substring matching is deliberately simple (no regex) — see the module's test cases for the
/// substring-collision scenarios this was checked against (e.g. `libcuda.so.1` is not a
/// substring of `libcudart.so.12`, and `libcudnn.so.9` correctly matches versioned lines like
/// `libcudnn.so.9.1.0`).
#[allow(
    dead_code,
    reason = "called by cuda_stack_status, itself only live on linux/x86_64 outside tests"
)]
fn ldconfig_lists(output: Option<&str>, needle: &str) -> bool {
    output.is_some_and(|text| text.lines().any(|line| line.contains(needle)))
}

/// Thin OS wrapper around [`cuda_stack_status`]: collects the real inputs (file existence checks
/// plus a single `ldconfig -p` spawn) on linux/x86_64, the only target the CUDA ONNX Runtime
/// flavor is downloadable for (see `ort_download::CUDA_LINUX_X64`). Every other target reports
/// [`CudaStackStatus::DriverMissing`] unconditionally — there is no CUDA flavor to fall back to
/// there anyway, so the factory's "should I even attempt CUDA" question is answered "no" as
/// cheaply as possible.
///
/// Consumed by the embedder factory (issue #96, later chunk) to decide whether to attempt the
/// CUDA ONNX Runtime flavor before spending a download/dlopen on it.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code, reason = "consumed by factory in the next change")
)]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn detect_cuda_stack() -> CudaStackStatus {
    let inputs = CudaProbeInputs {
        proc_driver_version_exists: Path::new("/proc/driver/nvidia/version").exists(),
        dev_nvidiactl_exists: Path::new("/dev/nvidiactl").exists(),
        ldconfig_output: run_ldconfig(),
    };
    cuda_stack_status(&inputs)
}

/// Spawn `ldconfig -p` once and capture its stdout; `None` if the spawn itself fails (missing
/// binary, permission error, etc — never panics, never blocks indefinitely).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run_ldconfig() -> Option<String> {
    std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// No-op stub counterpart of [`detect_cuda_stack`] for every target other than linux/x86_64: the
/// CUDA ONNX Runtime flavor is never downloadable there (see `ort_download::CUDA_LINUX_X64`), so
/// there is nothing to probe — always report [`CudaStackStatus::DriverMissing`].
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[allow(dead_code, reason = "consumed by factory in the next change")]
pub(crate) fn detect_cuda_stack() -> CudaStackStatus {
    CudaStackStatus::DriverMissing
}

/// Build the canonical "CUDA unavailable" message. `cause` names *why* (a specific missing
/// component, or the underlying `ort` registration error); `missing_hint`, when present, names
/// the concrete fix (e.g. "install cuDNN 9`). Reused verbatim by the factory (issue #96, later
/// chunk) for both the cheap ladder's failures and [`probe_cuda`]'s ground-truth failure.
#[allow(dead_code, reason = "consumed by factory in the next change")]
pub(crate) fn cuda_unavailable_error(cause: &str, missing_hint: Option<&str>) -> String {
    let missing = missing_hint
        .map(|hint| format!(" Missing: {hint}."))
        .unwrap_or_default();
    format!(
        "CUDA execution provider unavailable: {cause}. Requires an NVIDIA GPU with driver \
         R525+, the CUDA 12.x runtime, and cuDNN 9 (`libcudnn.so.9`).{missing} Install the \
         missing pieces, or use provider 'local' (automatic CPU fallback). Advanced: set \
         ORT_DYLIB_PATH or embedding.ort_library to a custom ONNX Runtime build."
    )
}

/// `(cause, missing_hint)` pair for each [`CudaStackStatus`] the cheap ladder can report,
/// matching [`cuda_unavailable_error`]'s parameters. Reused by the factory so ladder failures and
/// [`probe_cuda`] failures produce messages in the same canonical shape.
#[allow(dead_code, reason = "consumed by factory in the next change")]
pub(crate) fn stack_status_cause_and_hint(
    status: CudaStackStatus,
) -> (&'static str, Option<&'static str>) {
    match status {
        CudaStackStatus::Ok => (
            "the CUDA stack looked complete (driver, CUDA runtime, and cuDNN all detected) but \
             ONNX Runtime's CUDA execution provider still failed to register",
            None,
        ),
        CudaStackStatus::DriverMissing => (
            "no NVIDIA driver detected (checked /proc/driver/nvidia/version, /dev/nvidiactl, and \
             ldconfig for libcuda.so.1)",
            Some("install an NVIDIA driver, R525 or newer"),
        ),
        CudaStackStatus::CudartMissing => (
            "libcudart.so.12 not found (checked ldconfig -p)",
            Some("install the CUDA 12.x runtime"),
        ),
        CudaStackStatus::CudnnMissing => (
            "libcudnn.so.9 not found (checked ldconfig -p)",
            Some("install cuDNN 9"),
        ),
    }
}

/// EP dispatch construction and the ground-truth `ort` probe. Gated to `local-onnx` since both
/// touch the `ort` crate directly, which is only a dependency under that feature.
#[cfg(feature = "local-onnx")]
mod ort_backed {
    use std::sync::OnceLock;

    use ort::ep::{ExecutionProvider, ExecutionProviderDispatch, CUDA};

    use super::{cuda_unavailable_error, CudaPreference};
    use crate::ort_runtime::{self, OrtFlavor};

    /// Build the execution-provider dispatch list `with_execution_providers` expects, per
    /// [`CudaPreference`]:
    ///
    /// - [`CudaPreference::Disabled`] → empty list (CPU only).
    /// - [`CudaPreference::Preferred`] → `[CUDA::default().build()]`. With a plain `.build()`
    ///   (no `.error_on_failure()`), a failed registration is logged internally by `ort` and the
    ///   session falls back to running on CPU — exactly the "nice-to-have GPU" behavior `local`
    ///   (automatic) mode wants.
    /// - [`CudaPreference::Required`] → `[CUDA::default().build().error_on_failure()]`. This
    ///   surfaces a failed registration as an `Err` at `with_execution_providers` time instead of
    ///   silently falling back — what the explicit `local-cuda` provider wants.
    #[allow(dead_code, reason = "consumed by factory in the next change")]
    pub(crate) fn dispatch_list(pref: CudaPreference) -> Vec<ExecutionProviderDispatch> {
        match pref {
            CudaPreference::Disabled => vec![],
            CudaPreference::Preferred => vec![CUDA::default().build()],
            CudaPreference::Required => vec![CUDA::default().build().error_on_failure()],
        }
    }

    /// Ground-truth CUDA availability probe (detection ladder rung 3): attempts to actually
    /// register the CUDA execution provider against a throwaway [`SessionBuilder`], without
    /// creating a session or loading any model. Cached in a [`OnceLock`] — this is meaningfully
    /// slow (it exercises real ONNX Runtime / CUDA driver calls) and its result cannot change
    /// within a process, so it only needs to run once.
    ///
    /// # Precondition
    ///
    /// [`ort_runtime::committed_flavor`] must be `Some(OrtFlavor::Cuda)` — i.e.
    /// [`ort_runtime::ensure_ort_initialized`] must already have been called (and succeeded)
    /// with [`OrtFlavor::Cuda`]. This is checked *before* any other `ort` API is touched: under
    /// `load-dynamic`, calling `ort` APIs before the environment is initialized aborts/panics
    /// rather than returning an error, so this check is what makes the probe safe to call
    /// speculatively (e.g. from a factory that isn't sure yet whether CUDA init happened).
    ///
    /// [`SessionBuilder`]: ort::session::builder::SessionBuilder
    ///
    /// # Visibility
    ///
    /// `pub` (not `pub(crate)`) so CI's GPU-less `ort-download` job
    /// (`embed/tests/cuda_probe_gpuless.rs`) can exercise this exact probe directly, in its own
    /// process, after a real `ensure_ort_initialized(OrtFlavor::Cuda, None)`. The alternative —
    /// driving this only through `create_embedder`'s `local-cuda` provider — would instead
    /// exercise `detect_cuda_stack`'s cheap file-level ladder, which fails at the driver-missing
    /// rung on a GPU-less runner *before* ever reaching this probe; that's a different (also
    /// useful) guarantee, not this one. This is otherwise a read-only, side-effect-free query
    /// (see the `OnceLock` caching above), so widening it to a public API is low-risk.
    #[allow(dead_code, reason = "consumed by factory in the next change")]
    pub fn probe_cuda() -> Result<(), String> {
        static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
        PROBE.get_or_init(probe_cuda_once).clone()
    }

    fn probe_cuda_once() -> Result<(), String> {
        if ort_runtime::committed_flavor() != Some(OrtFlavor::Cuda) {
            return Err(
                "CUDA probe requires the CUDA-flavored ONNX Runtime to be initialized first"
                    .to_string(),
            );
        }

        // No session/model is created: SessionBuilder::new() plus a single register() call is
        // enough to exercise ONNX Runtime's real CUDA provider registration path (driver
        // presence, CUDA/cuDNN version compatibility, etc.) without the cost of loading a model.
        let mut builder = ort::session::Session::builder()
            .map_err(|e| cuda_unavailable_error(&format!("failed to probe: {e}"), None))?;
        CUDA::default()
            .register(&mut builder)
            .map_err(|e| cuda_unavailable_error(&e.to_string(), None))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn dispatch_list_lengths() {
            assert_eq!(dispatch_list(CudaPreference::Disabled).len(), 0);
            assert_eq!(dispatch_list(CudaPreference::Preferred).len(), 1);
            assert_eq!(dispatch_list(CudaPreference::Required).len(), 1);
        }

        #[test]
        fn dispatch_list_required_sets_error_on_failure() {
            // `ExecutionProviderDispatch`'s `Debug` impl (see `ort::ep`) renders as
            // `<name> { error_on_failure: <bool> }`, so this is the one place we can observe the
            // flag from outside the crate without a session/model. `.build()`'s output for both
            // `Preferred` and `Required` is otherwise the same underlying `CUDA` provider.
            let preferred = format!("{:?}", dispatch_list(CudaPreference::Preferred)[0]);
            let required = format!("{:?}", dispatch_list(CudaPreference::Required)[0]);
            assert!(
                preferred.contains("error_on_failure: false"),
                "Preferred dispatch should not set error_on_failure: {preferred}"
            );
            assert!(
                required.contains("error_on_failure: true"),
                "Required dispatch should set error_on_failure: {required}"
            );
        }

        #[test]
        fn probe_before_init_errors_cleanly() {
            // This test (and every other test in this crate's unit test binary) must never
            // itself call `ort_runtime::ensure_ort_initialized` / touch `ort::init_from` — real
            // ONNX Runtime initialization only happens in `embed/tests/ort_init_download.rs`
            // (a separate integration-test binary). That invariant is what makes it safe to
            // assert `probe_cuda()` fails cleanly here: `committed_flavor()` is guaranteed `None`
            // for the lifetime of this process.
            let err = probe_cuda().expect_err("probe should fail before ort is initialized");
            assert!(
                err.contains("requires the CUDA-flavored ONNX Runtime"),
                "error should explain the precondition: {err}"
            );
        }
    }
}

/// Re-exported at module level (rather than requiring `crate::cuda_ep::ort_backed::...`) so the
/// factory can consume `crate::cuda_ep::dispatch_list` directly without needing `ort_backed`
/// itself to be `pub(crate)`.
#[cfg(feature = "local-onnx")]
#[allow(unused_imports, reason = "consumed by factory in the next change")]
pub(crate) use ort_backed::dispatch_list;

/// Re-exported `pub` (not `pub(crate)`) — see [`ort_backed::probe_cuda`]'s doc comment for why
/// CI's GPU-less integration test needs to reach this from outside the crate.
#[cfg(feature = "local-onnx")]
pub use ort_backed::probe_cuda;

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(proc: bool, dev: bool, ldconfig: Option<&str>) -> CudaProbeInputs {
        CudaProbeInputs {
            proc_driver_version_exists: proc,
            dev_nvidiactl_exists: dev,
            ldconfig_output: ldconfig.map(str::to_string),
        }
    }

    const FULL_STACK_LDCONFIG: &str = "\
        \tlibcudnn.so.9 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudnn.so.9\n\
        \tlibcudart.so.12 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudart.so.12\n\
        \tlibcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1\n";

    #[test]
    fn all_missing_is_driver_missing() {
        assert_eq!(
            cuda_stack_status(&inputs(false, false, None)),
            CudaStackStatus::DriverMissing
        );
    }

    #[test]
    fn driver_via_proc_only() {
        // No ldconfig info at all (spawn failed) — cudart/cudnn can't be confirmed, so the
        // honest answer is CudartMissing, not DriverMissing.
        assert_eq!(
            cuda_stack_status(&inputs(true, false, None)),
            CudaStackStatus::CudartMissing
        );
    }

    #[test]
    fn driver_via_dev_nvidiactl_only() {
        assert_eq!(
            cuda_stack_status(&inputs(false, true, None)),
            CudaStackStatus::CudartMissing
        );
    }

    #[test]
    fn driver_via_ldconfig_libcuda_only() {
        let ldconfig = "\tlibcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1\n";
        assert_eq!(
            cuda_stack_status(&inputs(false, false, Some(ldconfig))),
            CudaStackStatus::CudartMissing
        );
    }

    #[test]
    fn driver_ok_cudart_missing() {
        let ldconfig = "\tlibcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1\n";
        assert_eq!(
            cuda_stack_status(&inputs(true, false, Some(ldconfig))),
            CudaStackStatus::CudartMissing
        );
    }

    #[test]
    fn driver_and_cudart_ok_cudnn_missing() {
        let ldconfig = "\
            \tlibcudart.so.12 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudart.so.12\n\
            \tlibcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1\n";
        assert_eq!(
            cuda_stack_status(&inputs(true, false, Some(ldconfig))),
            CudaStackStatus::CudnnMissing
        );
    }

    #[test]
    fn full_stack_is_ok() {
        assert_eq!(
            cuda_stack_status(&inputs(true, true, Some(FULL_STACK_LDCONFIG))),
            CudaStackStatus::Ok
        );
    }

    #[test]
    fn ldconfig_spawn_failure_with_proc_present_still_requires_cudart_confirmation() {
        // Documents the deliberate choice: a `None` ldconfig output (spawn failed) can never
        // upgrade past CudartMissing even when the driver is confirmed present some other way —
        // we have no evidence cudart/cudnn are installed, so we don't claim they are.
        assert_eq!(
            cuda_stack_status(&inputs(true, true, None)),
            CudaStackStatus::CudartMissing
        );
    }

    #[test]
    fn libcudart_line_does_not_satisfy_libcuda_driver_rung() {
        // "libcudart.so.12" does NOT contain "libcuda.so.1" as a substring (the 'r' after
        // "libcuda" breaks the match) — a system with only the CUDA runtime installed and no
        // driver must not be reported as having a driver.
        let ldconfig =
            "\tlibcudart.so.12 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudart.so.12\n";
        assert!(!"libcudart.so.12".contains("libcuda.so.1"));
        assert_eq!(
            cuda_stack_status(&inputs(false, false, Some(ldconfig))),
            CudaStackStatus::DriverMissing
        );
    }

    #[test]
    fn bare_libcuda_so_without_version_suffix_does_not_satisfy_driver_rung() {
        // A line listing only the unversioned dev-symlink `libcuda.so` (no `.1`) must not count
        // — we require the versioned runtime library name.
        let ldconfig = "\tlibcuda.so (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so\n";
        assert_eq!(
            cuda_stack_status(&inputs(false, false, Some(ldconfig))),
            CudaStackStatus::DriverMissing
        );
    }

    #[test]
    fn versioned_cudnn_line_still_matches() {
        // Real ldconfig output often lists the fully-versioned soname (e.g. `.so.9.1.0`), not
        // just `.so.9` — `contains` must still match it.
        let ldconfig = "\
            \tlibcudnn.so.9.1.0 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudnn.so.9.1.0\n\
            \tlibcudart.so.12 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcudart.so.12\n\
            \tlibcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1\n";
        assert_eq!(
            cuda_stack_status(&inputs(true, false, Some(ldconfig))),
            CudaStackStatus::Ok
        );
    }

    #[test]
    fn detect_cuda_stack_runs_without_panicking() {
        // Real OS wrapper: on this (macOS) dev machine it must take the "not linux/x86_64" stub
        // path and report DriverMissing without touching the filesystem/process APIs in a way
        // that could panic.
        let status = detect_cuda_stack();
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        assert_eq!(status, CudaStackStatus::DriverMissing);
        let _ = status; // silence unused-on-linux warning if this ever runs there in CI
    }

    #[test]
    fn canonical_error_mentions_requirements() {
        for status in [
            CudaStackStatus::DriverMissing,
            CudaStackStatus::CudartMissing,
            CudaStackStatus::CudnnMissing,
        ] {
            let (cause, hint) = stack_status_cause_and_hint(status);
            let msg = cuda_unavailable_error(cause, hint);
            assert!(msg.contains("R525+"), "{status:?}: {msg}");
            assert!(msg.contains("CUDA 12.x"), "{status:?}: {msg}");
            assert!(msg.contains("cuDNN 9"), "{status:?}: {msg}");
            assert!(msg.contains("provider 'local'"), "{status:?}: {msg}");
            assert!(msg.contains("ORT_DYLIB_PATH"), "{status:?}: {msg}");
        }
    }

    #[test]
    fn canonical_error_without_hint_omits_missing_clause() {
        let msg = cuda_unavailable_error("some underlying ort error", None);
        assert!(!msg.contains("Missing:"));
        assert!(msg.contains("some underlying ort error"));
    }

    #[test]
    fn canonical_error_with_hint_includes_missing_clause() {
        let msg = cuda_unavailable_error("libcudnn.so.9 not found", Some("install cuDNN 9"));
        assert!(msg.contains("Missing: install cuDNN 9."));
    }
}
