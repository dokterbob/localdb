//! Real (network + `ort::init_from`) integration test for the runtime-downloaded ONNX Runtime
//! flavor table (issue #76).
//!
//! Kept in its own integration test binary: unlike the pure-function unit tests in
//! `embed/src/ort_runtime.rs`, this test actually commits the process-wide `ort` environment
//! via a real `dlopen`, which can only happen once per process — it must not share a process
//! with any other test that also calls `ensure_ort_initialized` (each `tests/*.rs` file is
//! compiled as its own binary, so this is naturally isolated from the unit tests and from the
//! other integration tests in this directory).
//!
//! Skipped unless `LOCALDB_TEST_ORT_DOWNLOAD=1`, since it downloads a real multi-MB release
//! asset from GitHub (or reuses a cache already seeded by `ort_download.rs`'s own gated test)
//! and performs a real `ort::init_from`.
//!
//! ```sh
//! LOCALDB_TEST_ORT_DOWNLOAD=1 cargo test -p embed --features local-onnx real_init_cpu_flavor -- --nocapture
//! ```

#![cfg(feature = "local-onnx")]

use embed::ort_runtime::{ensure_ort_initialized, OrtFlavor};

#[test]
fn real_init_cpu_flavor() {
    if std::env::var("LOCALDB_TEST_ORT_DOWNLOAD").as_deref() != Ok("1") {
        eprintln!("skipping real_init_cpu_flavor (set LOCALDB_TEST_ORT_DOWNLOAD=1 to run)");
        return;
    }

    ensure_ort_initialized(OrtFlavor::Cpu, None).expect("first Cpu init should succeed");

    // Idempotent: a second call with the same flavor returns the cached Ok cheaply, without
    // trying to re-init `ort`.
    ensure_ort_initialized(OrtFlavor::Cpu, None)
        .expect("second Cpu init (same flavor) should be cached Ok");

    // The process already committed to Cpu; requesting Cuda now is a hard error naming the
    // committed flavor, not a silent fall-through.
    let err = ensure_ort_initialized(OrtFlavor::Cuda, None).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Cpu"),
        "error should mention the already-committed Cpu flavor: {msg}"
    );
}
