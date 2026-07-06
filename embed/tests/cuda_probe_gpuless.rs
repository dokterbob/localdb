//! Real (network + `ort::init_from` + CUDA execution-provider registration) integration test
//! for the "clean failure on a GPU-less machine" guarantee (issue #76/#96).
//!
//! CI's `ort-download` job runs this on a `ubuntu-22.04` runner that deliberately has no NVIDIA
//! GPU/driver. The point is to prove that initializing the CUDA-flavored ONNX Runtime and then
//! asking it to register the CUDA execution provider fails *cleanly* — a plain `Err`, never a
//! panic/abort — with our canonical, actionable error text. This is a different guarantee from
//! `factory.rs`'s `detect_cuda_stack` cheap ladder (which would report `DriverMissing` and never
//! even reach `ort`): here we want ground truth from `ort` itself.
//!
//! Kept in its own integration test binary for the same reason as `ort_init_download.rs`: it
//! commits the process-wide `ort` environment via a real `dlopen`, which can only happen once
//! per process, and each `tests/*.rs` file is its own binary.
//!
//! Linux/x86_64 only — the only target the CUDA ONNX Runtime flavor is downloadable for (see
//! `embed/src/ort_download.rs`'s `CUDA_LINUX_X64`).
//!
//! Skipped unless `LOCALDB_TEST_ORT_DOWNLOAD=1`, since it downloads a real ~196 MB release asset
//! from GitHub (or reuses a cache already seeded by a sibling gated test) and performs a real
//! `ort::init_from` + EP registration attempt.
//!
//! ```sh
//! LOCALDB_TEST_ORT_DOWNLOAD=1 cargo test -p embed --features local-onnx --test cuda_probe_gpuless -- --nocapture
//! ```

#![cfg(all(feature = "local-onnx", target_os = "linux", target_arch = "x86_64"))]

use embed::cuda_ep::probe_cuda;
use embed::ort_runtime::{ensure_ort_initialized, OrtFlavor};

#[test]
fn probe_cuda_fails_cleanly_without_a_gpu() {
    if std::env::var("LOCALDB_TEST_ORT_DOWNLOAD").as_deref() != Ok("1") {
        eprintln!(
            "skipping probe_cuda_fails_cleanly_without_a_gpu (set LOCALDB_TEST_ORT_DOWNLOAD=1 to run)"
        );
        return;
    }

    // Downloads (or reuses a cached) CUDA-flavored ONNX Runtime and dlopens it. This step alone
    // must succeed on a GPU-less machine — dlopen only needs the shared libraries on disk, not
    // a GPU — so a failure here would point at the download/verification path, not the probe.
    ensure_ort_initialized(OrtFlavor::Cuda, None)
        .expect("CUDA-flavored ONNX Runtime should dlopen fine even without a GPU present");

    // Ground-truth probe: attempts to actually register the CUDA execution provider. On a
    // GPU-less runner this must fail (no libcudart.so.12-backed device to bind to), but as a
    // clean `Err` — never a panic/abort/process crash — carrying our canonical, actionable
    // message (see `cuda_ep::cuda_unavailable_error`, which `probe_cuda` already routes its
    // failure through internally).
    let err = probe_cuda().expect_err("CUDA EP registration must fail on a GPU-less runner");

    assert!(
        err.contains("CUDA execution provider unavailable"),
        "error should use the canonical lead-in: {err}"
    );
    assert!(
        err.contains("R525+"),
        "error should name the minimum driver requirement: {err}"
    );
    assert!(
        err.contains("CUDA 12.x"),
        "error should name the required CUDA runtime version: {err}"
    );
    assert!(
        err.contains("cuDNN 9"),
        "error should name the required cuDNN version: {err}"
    );
    assert!(
        err.contains("provider 'local'"),
        "error should point at the automatic-fallback provider: {err}"
    );

    // A second call must return the same cached `Err` cheaply (the probe is `OnceLock`-cached)
    // rather than attempting EP registration again.
    let err2 = probe_cuda().expect_err("cached probe result should still be an Err");
    assert_eq!(err, err2, "probe_cuda should be idempotent");
}
