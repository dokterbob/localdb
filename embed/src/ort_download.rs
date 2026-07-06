//! Runtime (first-use) download of Microsoft's official ONNX Runtime from a sha256-pinned
//! "flavor table" — CPU flavors per target, plus a CUDA flavor for linux/x86_64.
//!
//! # Why download at runtime, not embed at build time (issue #76)
//!
//! Before this module existed, `embed/build.rs` downloaded the ONNX Runtime *at build time* and
//! baked it into the binary via `include_bytes!`. That meant every build embedded a single CPU
//! library regardless of what the machine that eventually ran the binary actually needed —
//! roughly a third of that artifact size was dead weight on any given machine: macOS defaults to
//! CoreML and never touches the embedded ONNX Runtime at all, and CUDA machines need a
//! GPU-capable library the CPU build can't provide. `build.rs` has since been deleted; this
//! flavor table, downloaded on first use instead, lets one release binary serve CPU-only
//! Linux/macOS, Linux+CUDA today, and Linux+ROCm (or other accelerators) later, without bloating
//! every binary with libraries most installs will never load.
//!
//! # The #133 heritage
//!
//! This module inherits `build.rs`'s non-negotiable constraint: only *Microsoft's official*
//! ONNX Runtime release builds are ever fetched, each pinned by sha256 in the table below and
//! re-verified after download. `ort`'s `download-binaries` feature (and any `api-*` default
//! feature) must never be re-enabled — that feature statically links pyke.io's prebuilt
//! archive, which is built with GCC 14 on Ubuntu 24.04 and gives the *release binary itself* a
//! `GLIBC_2.38` floor, breaking startup on glibc-2.35 distros (Linux Mint 21.x, Ubuntu 22.04);
//! see issue #133 and pykeio/ort#523 (unresolved upstream). `ort/cuda` must never be added
//! either — CUDA support here means dlopen-ing Microsoft's official CUDA execution provider
//! library ourselves (`load-dynamic`), not linking `ort`'s own CUDA bindings.
//!
//! This module only *downloads and verifies* tarball payloads to a cache directory; it does
//! not touch `ort` initialization — `ort_runtime.rs` consumes [`cpu_flavor_for_target`] and
//! [`ensure_downloaded`] to actually `dlopen` the downloaded library. `factory.rs`'s CUDA
//! availability probe (choosing [`OrtFlavor::Cuda`](crate::ort_runtime::OrtFlavor::Cuda) over
//! `Cpu`) is wired up in a later chunk.

use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{error::EmbedError, model_cache::ModelCache};

/// ONNX Runtime version this flavor table is pinned to. Previously kept in sync with a copy of
/// the same constant in `embed/build.rs`; that copy — and `build.rs` itself — was removed once
/// this module took over ONNX Runtime acquisition (issue #76).
pub(crate) const ORT_VERSION: &str = "1.24.4";

/// One downloadable ONNX Runtime "flavor": a release tarball for a specific target/backend,
/// the shared library payload(s) inside it, and where to cache them once extracted.
#[derive(Debug)]
pub(crate) struct RemoteRuntime {
    /// Direct download URL of the release tarball (`.tgz`).
    pub url: &'static str,
    /// Pinned sha256 of the whole tarball, verified before it is trusted or extracted.
    pub tarball_sha256: &'static str,
    /// `(path_in_tar, sha256)` for each shared library to extract. Paths carry the tarball's
    /// top-level directory prefix, e.g. `onnxruntime-linux-x64-1.24.4/lib/libonnxruntime.so.1.24.4`.
    pub payloads: &'static [(&'static str, &'static str)],
    /// Cache subdirectory this flavor's extracted payloads live under (namespaces CPU vs.
    /// CUDA builds of the same ONNX Runtime version so they never collide).
    pub cache_subdir: &'static str,
    /// Approximate tarball download size in MiB, surfaced in the one-time download log line.
    pub approx_download_mb: u32,
}

/// Linux x86_64 CPU build. Same tarball/payload the now-deleted `build.rs` used to embed; the
/// extracted file name and bytes are identical, so existing user caches under this file name
/// remained valid once this module superseded `build.rs`.
pub(crate) const CPU_LINUX_X64: RemoteRuntime = RemoteRuntime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.24.4/onnxruntime-linux-x64-1.24.4.tgz",
    tarball_sha256: "3a211fbea252c1e66290658f1b735b772056149f28321e71c308942cdb54b747",
    payloads: &[(
        "onnxruntime-linux-x64-1.24.4/lib/libonnxruntime.so.1.24.4",
        "d132535d051344ff5c64c9c200004150559049a81ed330eb4422c1962fb6b7e4",
    )],
    cache_subdir: ORT_VERSION,
    approx_download_mb: 8,
};

/// Linux aarch64 CPU build.
pub(crate) const CPU_LINUX_AARCH64: RemoteRuntime = RemoteRuntime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.24.4/onnxruntime-linux-aarch64-1.24.4.tgz",
    tarball_sha256: "866109a9248d057671a039b9d725be4bd86888e3754140e6701ec621be9d4d7e",
    payloads: &[(
        "onnxruntime-linux-aarch64-1.24.4/lib/libonnxruntime.so.1.24.4",
        "52b2a0e75e79468404284fec38ad5ee1a7a996232274f5e1b84f3e793fb07554",
    )],
    cache_subdir: ORT_VERSION,
    approx_download_mb: 7,
};

/// macOS aarch64 (Apple Silicon) CPU build.
pub(crate) const CPU_OSX_ARM64: RemoteRuntime = RemoteRuntime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.24.4/onnxruntime-osx-arm64-1.24.4.tgz",
    tarball_sha256: "93787795f47e1eee369182e43ed51b9e5da0878ab0346aecf4258979b8bba989",
    payloads: &[(
        "onnxruntime-osx-arm64-1.24.4/lib/libonnxruntime.1.24.4.dylib",
        "872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289",
    )],
    cache_subdir: ORT_VERSION,
    approx_download_mb: 30,
};

/// Linux x86_64 CUDA build. Three payloads: the core runtime library, the shared provider
/// bridge, and the CUDA execution provider itself. Deliberately excludes
/// `libonnxruntime_providers_tensorrt.so` — no TensorRT execution provider support, smaller
/// download, one less native dependency to dlopen.
///
/// Dead code on non-(linux, x86_64) targets: nothing on macOS ever selects this flavor, but
/// it's kept unconditionally compiled (rather than `#[cfg]`-removed) so the flavor table's
/// shape stays uniform across targets for later chunks (factory.rs's flavor selection).
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) const CUDA_LINUX_X64: RemoteRuntime = RemoteRuntime {
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.24.4/onnxruntime-linux-x64-gpu-1.24.4.tgz",
    tarball_sha256: "c5f804ff5d239b436fa59e9f2fb288a39f7eb9552f6a636c8b71e792e91a8808",
    payloads: &[
        (
            "onnxruntime-linux-x64-gpu-1.24.4/lib/libonnxruntime.so.1.24.4",
            "1aacefdf0b4afa145d410b2381bbc3db3d978c485fb182c42a2b0b09f91f5310",
        ),
        (
            "onnxruntime-linux-x64-gpu-1.24.4/lib/libonnxruntime_providers_shared.so",
            "c6a12593396095f5670160e284c35d1700b7708cf3037b7042e2a5200ccae772",
        ),
        (
            "onnxruntime-linux-x64-gpu-1.24.4/lib/libonnxruntime_providers_cuda.so",
            "1defa2f82f2195a0667f2003e14c6715107af7d2716364cfdfa1a8c5e708ddaa",
        ),
    ],
    cache_subdir: "1.24.4-cuda",
    approx_download_mb: 196,
};

/// Pick the CPU flavor for `(os, arch)`, pure and independent of the actual compile target so
/// the unsupported-target error path is unit-testable on any host.
pub(crate) fn cpu_flavor_for(os: &str, arch: &str) -> Result<&'static RemoteRuntime, EmbedError> {
    match (os, arch) {
        ("linux", "x86_64") => Ok(&CPU_LINUX_X64),
        ("linux", "aarch64") => Ok(&CPU_LINUX_AARCH64),
        ("macos", "aarch64") => Ok(&CPU_OSX_ARM64),
        (os, arch) => Err(EmbedError::Internal(format!(
            "localdb's `local-onnx` feature has no downloadable ONNX Runtime build for target \
             {os}/{arch}. Supported: linux/x86_64, linux/aarch64, macos/aarch64. Build without \
             `--features local-onnx`, or set ORT_DYLIB_PATH to the path of a local ONNX \
             Runtime shared library to bypass this check."
        ))),
    }
}

/// [`cpu_flavor_for`] applied to the actual compile target (`env::consts::OS`/`ARCH` reflect
/// the target this binary was built for, not necessarily the host running the build).
pub(crate) fn cpu_flavor_for_target() -> Result<&'static RemoteRuntime, EmbedError> {
    cpu_flavor_for(env::consts::OS, env::consts::ARCH)
}

/// Ensure every payload of `rt` is present and verified under `dest_dir`, downloading and
/// extracting `rt`'s tarball if needed.
///
/// If all payloads already exist with matching sha256, this returns `Ok(())` without any
/// network access (offline pre-seed path). Otherwise it streams `rt.url` to a process-unique temp
/// file (hashing as bytes arrive — tarballs run up to ~200 MB, never buffered whole), verifies
/// the tarball's sha256, then streams each listed payload out of the (gzipped) tar archive to a
/// process-unique `.tmp` sibling of its final path, verifies its sha256, and renames it into
/// place. On any hash mismatch (tarball or payload) *or* I/O error during extraction, all partial
/// output this call produced is removed and a hard error is returned naming the URL and the
/// expected/actual hashes (see [`extract_payloads`]'s doc comment for the cleanup guarantee).
///
/// Every scratch/temp path this function creates embeds the current process id plus a
/// monotonically increasing counter (see [`unique_tmp_path`]), so two processes downloading the
/// same flavor concurrently (e.g. a CLI run and a daemon on a fresh machine) never share a temp
/// file: each writes, verifies, and atomically renames only its own bytes. A completed rename
/// from either process is equally valid — both downloaded and verified the same pinned content —
/// so concurrent completions are benign; only a shared, truncatable temp path would be a hazard.
pub(crate) fn ensure_downloaded(
    rt: &'static RemoteRuntime,
    dest_dir: &Path,
) -> Result<(), EmbedError> {
    if payloads_valid(rt.payloads, dest_dir) {
        return Ok(());
    }

    fs::create_dir_all(dest_dir).map_err(EmbedError::Io)?;

    tracing::info!(
        url = rt.url,
        approx_mb = rt.approx_download_mb,
        "downloading ONNX Runtime (one-time, cached)…"
    );

    let tarball_tmp = unique_tmp_path(&dest_dir.join(format!("{}.download.tgz", rt.cache_subdir)));
    let result = (|| -> Result<(), EmbedError> {
        let actual_tarball_sha = download_tarball_streaming(rt.url, &tarball_tmp)?;
        verify_and_extract(
            &tarball_tmp,
            &actual_tarball_sha,
            rt.tarball_sha256,
            rt.url,
            rt.payloads,
            dest_dir,
        )
    })();

    // Always clean up the downloaded tarball, success or failure — it's a scratch file, never
    // part of the cache contract. Process-unique, so this never races another process's tarball.
    let _ = fs::remove_file(&tarball_tmp);
    result?;

    // Every payload was just individually hash-verified during extraction — record that in the
    // marker so the next process start can skip re-hashing (see `payloads_valid`'s doc comment).
    write_verified_marker(rt.payloads, dest_dir);

    tracing::info!(url = rt.url, "ONNX Runtime download complete");
    Ok(())
}

/// True iff every one of `payloads` already exists under `dest_dir` with a matching sha256 —
/// the offline pre-seed / already-cached fast path.
///
/// # Verification-marker fast path (issue F4: avoiding a full re-hash on every process start)
///
/// A full sha256 of every payload — up to ~340 MB for the CUDA flavor (incl. a 315 MB
/// `libonnxruntime_providers_cuda.so`) — is too expensive to redo on *every* `localdb` command;
/// the old embedded-era code only ever fully hashed a single ~22 MB library per start, which does
/// not scale to that. So this first consults [`fast_path_valid`]: if a verification marker
/// (written by [`write_verified_marker`] the last time every payload was fully hash-verified)
/// exists, matches the *current* pinned table sha for every payload, and every payload's on-disk
/// size still matches the marker's recorded size, validation succeeds without reading file
/// contents at all.
///
/// This trades content-corruption detection for speed: a payload whose bytes are corrupted
/// without changing its length would incorrectly pass the fast path (documented tradeoff, see the
/// dedicated unit test below). A pin bump (the table's expected sha256 changes) is *not* silently
/// accepted, though — the marker's recorded sha no longer matches the live table entry, so
/// [`fast_path_valid`] returns `false` and this falls through to the full hash check below, same
/// as if no marker existed at all.
///
/// Whenever the fast path doesn't apply, this falls back to the original full-hash check; on
/// success it writes a fresh marker (so the *next* start can take the fast path), and on failure
/// it removes any existing marker (so a stale marker never lingers to lie about a directory that
/// is now known-invalid).
fn payloads_valid(payloads: &[(&str, &str)], dest_dir: &Path) -> bool {
    if fast_path_valid(payloads, dest_dir) {
        return true;
    }

    let all_valid = payloads.iter().all(|(path_in_tar, expected_sha)| {
        let dest = payload_dest(dest_dir, path_in_tar);
        ModelCache::sha256_file(&dest)
            .map(|actual| actual == *expected_sha)
            .unwrap_or(false)
    });

    if all_valid {
        write_verified_marker(payloads, dest_dir);
    } else {
        // Best-effort: don't let a stale/wrong marker survive next to files we now know are
        // invalid — its mere presence must never lie about validity.
        let _ = fs::remove_file(marker_path(dest_dir));
    }

    all_valid
}

/// A payload's on-disk destination is `dest_dir` joined with just the file name portion of
/// its in-tar path — the CPU flavor's file name and content are byte-identical to what
/// `ort_runtime.rs` extracts today, so existing user caches at this path remain valid.
fn payload_dest(dest_dir: &Path, path_in_tar: &str) -> PathBuf {
    let file_name = Path::new(path_in_tar)
        .file_name()
        .expect("payload path_in_tar always has a file name");
    dest_dir.join(file_name)
}

/// File name of the marker [`write_verified_marker`] writes after every payload has been
/// confirmed present and correct by a full sha256 hash — see [`payloads_valid`]'s doc comment.
const VERIFIED_MARKER_FILE: &str = ".payloads-verified.json";

/// One payload's recorded state in the verification marker: just its file name (meaningless
/// outside the cache dir the marker itself lives in), byte size, and the sha256 it was verified
/// against at write time.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerifiedPayload {
    file_name: String,
    size: u64,
    sha256: String,
}

/// The verification marker's on-disk shape: one [`VerifiedPayload`] per payload that was hash
/// -verified the last time it was written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct VerifiedMarker {
    payloads: Vec<VerifiedPayload>,
}

fn marker_path(dest_dir: &Path) -> PathBuf {
    dest_dir.join(VERIFIED_MARKER_FILE)
}

/// Best-effort write of the verification marker, once `payloads` have just been confirmed (by a
/// real sha256 hash, either via a fresh extraction or a full pre-seed hash check) present and
/// correct under `dest_dir`. Never fatal on failure — the marker is purely a fast-path
/// optimization for [`payloads_valid`], never load-bearing for correctness — so any I/O or
/// serialization error here is silently swallowed.
fn write_verified_marker(payloads: &[(&str, &str)], dest_dir: &Path) {
    let recorded: Vec<VerifiedPayload> = payloads
        .iter()
        .filter_map(|(path_in_tar, sha)| {
            let dest = payload_dest(dest_dir, path_in_tar);
            let size = fs::metadata(&dest).ok()?.len();
            let file_name = dest.file_name()?.to_str()?.to_string();
            Some(VerifiedPayload {
                file_name,
                size,
                sha256: sha.to_string(),
            })
        })
        .collect();

    if recorded.len() != payloads.len() {
        // Couldn't stat/name every payload we supposedly just verified — don't write a marker
        // that would (incorrectly) fast-path a future call.
        return;
    }

    let marker = VerifiedMarker { payloads: recorded };
    if let Ok(json) = serde_json::to_vec_pretty(&marker) {
        let _ = fs::write(marker_path(dest_dir), json);
    }
}

/// Read back a previously-written verification marker, if present and parseable. `None` on any
/// I/O or parse error — every caller treats that identically to "no marker", i.e. falls back to
/// the full hash check.
fn read_verified_marker(dest_dir: &Path) -> Option<VerifiedMarker> {
    let bytes = fs::read(marker_path(dest_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// [`payloads_valid`]'s fast path: true iff a verification marker exists, parses, has exactly one
/// entry per payload, and every entry's recorded sha256 matches the *current* pinned table entry
/// (so a pin bump — a stale marker recording an old sha — correctly falls through to a full
/// re-hash) and its recorded size matches the file's current on-disk size (a cheap `stat`, not a
/// hash). See [`payloads_valid`]'s doc comment for the accepted tradeoff: content corruption that
/// preserves file size is not caught here.
fn fast_path_valid(payloads: &[(&str, &str)], dest_dir: &Path) -> bool {
    let Some(marker) = read_verified_marker(dest_dir) else {
        return false;
    };
    if marker.payloads.len() != payloads.len() {
        return false;
    }

    payloads.iter().all(|(path_in_tar, expected_sha)| {
        let dest = payload_dest(dest_dir, path_in_tar);
        let Some(file_name) = dest.file_name().and_then(|f| f.to_str()) else {
            return false;
        };
        let Some(recorded) = marker.payloads.iter().find(|p| p.file_name == file_name) else {
            return false;
        };
        if recorded.sha256 != *expected_sha {
            return false;
        }
        fs::metadata(&dest)
            .map(|meta| meta.len() == recorded.size)
            .unwrap_or(false)
    })
}

/// Per-process-unique tag (`<pid>-<monotonic counter>`), appended to every scratch/temp file this
/// module creates — see [`unique_tmp_path`] and [`ensure_downloaded`]'s doc comment for why this
/// matters (two processes downloading concurrently must never share a temp path).
fn unique_tmp_tag() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{pid}-{n}")
}

/// A process-unique temp sibling of `base`: same directory and file name, with a
/// `.<pid>-<counter>.tmp` suffix appended (see [`unique_tmp_tag`]). Two calls — whether in the
/// same process or two different ones — never collide.
fn unique_tmp_path(base: &Path) -> PathBuf {
    let tag = unique_tmp_tag();
    let mut file_name = base.file_name().map(|f| f.to_owned()).unwrap_or_default();
    file_name.push(format!(".{tag}.tmp"));
    base.with_file_name(file_name)
}

/// Verify `tarball_path`'s already-computed sha256 (`actual_tarball_sha`) against
/// `expected_tarball_sha`, then extract+verify each of `payloads` into `dest_dir`.
///
/// This is the shared verify/extract path: [`ensure_downloaded`] calls it after streaming a
/// real download (passing the hash computed during that stream, to avoid re-reading a
/// potentially ~200 MB file), and unit tests call it directly against local fixture tarballs
/// (no network required).
fn verify_and_extract(
    tarball_path: &Path,
    actual_tarball_sha: &str,
    expected_tarball_sha: &str,
    url: &str,
    payloads: &[(&str, &str)],
    dest_dir: &Path,
) -> Result<(), EmbedError> {
    if actual_tarball_sha != expected_tarball_sha {
        return Err(EmbedError::Internal(format!(
            "downloaded ONNX Runtime tarball from {url} but its sha256 ({actual_tarball_sha}) \
             does not match the pinned value ({expected_tarball_sha}). Refusing to use an \
             unverified binary. This may mean the pinned constant is stale, or the download \
             was corrupted/tampered with — retry, and if it persists, verify the release asset \
             manually."
        )));
    }

    let file = fs::File::open(tarball_path).map_err(EmbedError::Io)?;
    extract_payloads(file, payloads, dest_dir)
}

/// Stream-decompress+untar `reader`, extracting only the entries listed in `payloads` to
/// `dest_dir` (each written to a process-unique `.tmp` sibling of its final path, verified by
/// sha256, then renamed into place). On *any* failure — a payload hash mismatch, a payload
/// missing from the archive, or an I/O error on create/copy/rename (disk full, permissions,
/// etc.) — every file this call wrote is removed before returning the error, including the
/// in-flight tmp file of the payload that failed: `dest_dir` is left exactly as it was found.
fn extract_payloads(
    reader: impl Read,
    payloads: &[(&str, &str)],
    dest_dir: &Path,
) -> Result<(), EmbedError> {
    let decoder = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);

    let mut remaining: HashMap<String, &str> = payloads
        .iter()
        .map(|(path, sha)| (path.to_string(), *sha))
        .collect();
    let mut written: Vec<PathBuf> = Vec::new();

    let entries = archive
        .entries()
        .map_err(|e| EmbedError::Internal(format!("failed to read tar entries: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| EmbedError::Internal(format!("failed to read tar entry: {e}")))?;
        let entry_path = entry
            .path()
            .map_err(|e| EmbedError::Internal(format!("failed to read tar entry path: {e}")))?
            .to_string_lossy()
            .into_owned();

        let Some(expected_sha) = remaining.remove(&entry_path) else {
            continue;
        };

        let dest = payload_dest(dest_dir, &entry_path);
        let tmp = unique_tmp_path(&dest);

        if let Err(e) = write_and_verify_payload(&mut entry, &tmp, &dest, &entry_path, expected_sha)
        {
            // Route every early exit — a hash mismatch *or* an I/O error on create/copy/rename —
            // through the same cleanup: remove this attempt's tmp file plus every payload this
            // call already wrote, leaving `dest_dir` exactly as it was found (see this function's
            // doc comment).
            let _ = fs::remove_file(&tmp);
            cleanup(&written);
            return Err(e);
        }
        written.push(dest);
    }

    if !remaining.is_empty() {
        cleanup(&written);
        let missing: Vec<&str> = remaining.keys().map(String::as_str).collect();
        return Err(EmbedError::Internal(format!(
            "tarball did not contain expected payload path(s): {missing:?}"
        )));
    }

    Ok(())
}

/// Best-effort removal of files this extraction attempt wrote, on failure.
fn cleanup(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

/// Write one tar `entry`'s bytes to `tmp`, verify its sha256 against `expected_sha`, and
/// atomically rename it into `dest` — the full per-payload extraction step, factored out of
/// [`extract_payloads`]'s loop so *every* early exit (an I/O error on create/copy/rename, not
/// just a hash mismatch) is a plain `Err` the caller handles identically (see F3: previously, I/O
/// errors here propagated via `?` before the mismatch-path's cleanup ran, leaving orphan `.tmp`
/// files contrary to this module's documented cleanup guarantee).
fn write_and_verify_payload(
    entry: &mut impl Read,
    tmp: &Path,
    dest: &Path,
    entry_path: &str,
    expected_sha: &str,
) -> Result<(), EmbedError> {
    let actual_sha = {
        let out = fs::File::create(tmp).map_err(EmbedError::Io)?;
        let mut hashing = HashingWriter::new(out);
        io::copy(entry, &mut hashing).map_err(EmbedError::Io)?;
        hashing.finalize_hex()
    };

    if actual_sha != expected_sha {
        return Err(EmbedError::Internal(format!(
            "extracted payload {entry_path} sha256 mismatch: expected {expected_sha}, got \
             {actual_sha}. This may mean the pinned constant is stale, or the download was \
             corrupted/tampered with — retry, and if it persists, verify the release asset \
             manually."
        )));
    }

    fs::rename(tmp, dest).map_err(EmbedError::Io)
}

/// Builds the `ureq` agent used for ONNX Runtime tarball downloads: an explicit read timeout so
/// a stalled connection can't hang forever while `ort_runtime`'s process-wide init mutex is held
/// (see `ort_runtime.rs`'s module docs — a hung download there blocks every other thread's
/// `ensure_ort_initialized` call too), plus an explicit connect timeout for the same reason.
///
/// Deliberately generous and *not* an overall/deadline timeout: `timeout_read` only bounds the
/// gap between individual reads, so a slow-but-steady connection streaming the ~196 MB CUDA
/// tarball over several minutes is unaffected — only a connection that stalls completely (no
/// bytes for the whole window) times out.
fn download_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(60))
        .build()
}

/// Stream-download `url` to `dest`, computing its sha256 as bytes arrive so the tarball is
/// never buffered whole in memory (CUDA tarballs run to ~200 MB), returning the hex digest.
fn download_tarball_streaming(url: &str, dest: &Path) -> Result<String, EmbedError> {
    // ureq follows redirects by default (GitHub release assets 302 to
    // objects.githubusercontent.com) and uses rustls for TLS (see Cargo.toml).
    let response = download_agent()
        .get(url)
        .call()
        .map_err(|e| EmbedError::Internal(format!("failed to download {url}: {e}")))?;
    let mut body = response.into_reader();
    let file = fs::File::create(dest).map_err(EmbedError::Io)?;
    let mut hashing = HashingWriter::new(file);
    io::copy(&mut body, &mut hashing).map_err(EmbedError::Io)?;
    Ok(hashing.finalize_hex())
}

/// `io::Write` wrapper that tees written bytes into a running SHA-256 hash, so a download or
/// extraction can be verified without ever buffering the whole (up to ~300 MB) file in memory.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finalize_hex(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// F5: the download agent must set an explicit read (and connect) timeout — the default
    /// `ureq` agent has no read timeout at all, which would let a stalled connection hang
    /// forever while `ort_runtime`'s process-wide init mutex is held.
    #[test]
    fn download_agent_has_explicit_timeouts() {
        let agent = download_agent();
        let debug = format!("{agent:?}");
        assert!(
            debug.contains("timeout_read: Some"),
            "agent should set an explicit read timeout, not ureq's default (none): {debug}"
        );
        assert!(
            debug.contains("timeout_connect: Some"),
            "agent should set an explicit connect timeout: {debug}"
        );
    }

    /// Build a gzipped tarball containing `entries` (`(path_in_tar, contents)`) at
    /// `dir/fixture.tgz` and return its path.
    fn build_fixture_tarball(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let tarball_path = dir.join("fixture.tgz");
        let file = fs::File::create(&tarball_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        tarball_path
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    fn no_tmp_files_remain(dir: &Path) -> bool {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| e.path().extension().is_none_or(|ext| ext != "tmp"))
    }

    #[test]
    fn payloads_extracted_atomically_with_matching_sha() {
        let src = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let payload_bytes = b"fake onnxruntime payload bytes";
        let payload_sha = sha256_hex(payload_bytes);
        let tarball_path =
            build_fixture_tarball(src.path(), &[("pkg/lib/libfake.so", payload_bytes)]);
        let tarball_sha = ModelCache::sha256_file(&tarball_path).unwrap();

        let payloads = [("pkg/lib/libfake.so", payload_sha.as_str())];
        verify_and_extract(
            &tarball_path,
            &tarball_sha,
            &tarball_sha,
            "https://example.invalid/x.tgz",
            &payloads,
            dest.path(),
        )
        .unwrap();

        let dest_file = dest.path().join("libfake.so");
        assert!(dest_file.is_file());
        assert_eq!(ModelCache::sha256_file(&dest_file).unwrap(), payload_sha);
        assert!(
            no_tmp_files_remain(dest.path()),
            "no .tmp files should remain after successful extraction"
        );
    }

    #[test]
    fn tarball_sha_mismatch_aborts_and_cleans_tmp() {
        let src = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let tarball_path = build_fixture_tarball(src.path(), &[("pkg/lib/libfake.so", b"data")]);
        let actual_sha = ModelCache::sha256_file(&tarball_path).unwrap();
        let wrong_expected = "0".repeat(64);

        let payloads = [("pkg/lib/libfake.so", "irrelevant-not-checked")];
        let err = verify_and_extract(
            &tarball_path,
            &actual_sha,
            &wrong_expected,
            "https://example.invalid/x.tgz",
            &payloads,
            dest.path(),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains(&actual_sha) && msg.contains(&wrong_expected),
            "error should mention both expected and actual hash: {msg}"
        );
        assert!(
            fs::read_dir(dest.path()).unwrap().next().is_none(),
            "dest_dir should be untouched on tarball sha mismatch"
        );
    }

    #[test]
    fn payload_sha_mismatch_aborts_and_cleans() {
        let src = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let payload_bytes = b"real payload bytes";
        let tarball_path =
            build_fixture_tarball(src.path(), &[("pkg/lib/libfake.so", payload_bytes)]);
        let tarball_sha = ModelCache::sha256_file(&tarball_path).unwrap();
        let wrong_payload_sha = "1".repeat(64);

        let payloads = [("pkg/lib/libfake.so", wrong_payload_sha.as_str())];
        let err = verify_and_extract(
            &tarball_path,
            &tarball_sha,
            &tarball_sha,
            "https://example.invalid/x.tgz",
            &payloads,
            dest.path(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("libfake.so"));
        assert!(
            fs::read_dir(dest.path()).unwrap().next().is_none(),
            "dest_dir should be cleaned up on payload sha mismatch"
        );
    }

    /// F3: an I/O error mid-extract (not just a hash mismatch) must still clean up every tmp
    /// file this call created, and roll back any payload it already renamed into place earlier
    /// in the same call. Forced by pre-creating the second payload's destination as an existing
    /// non-empty directory, so `fs::rename` onto it fails with a real `io::Error`.
    #[test]
    fn io_error_during_extract_cleans_up_tmp_and_rolls_back() {
        let src = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let good_bytes: &[u8] = b"first payload, extracted fine";
        let blocked_bytes: &[u8] = b"second payload, rename blocked";
        let good_sha = sha256_hex(good_bytes);
        let blocked_sha = sha256_hex(blocked_bytes);

        let tarball_path = build_fixture_tarball(
            src.path(),
            &[
                ("pkg/lib/libgood.so", good_bytes),
                ("pkg/lib/libblocked.so", blocked_bytes),
            ],
        );
        let tarball_sha = ModelCache::sha256_file(&tarball_path).unwrap();

        // Pre-create the second payload's destination as an existing, non-empty directory:
        // renaming a regular file onto it fails with a real io::Error (EISDIR/ENOTDIR), not a
        // hash mismatch.
        let blocked_dest = dest.path().join("libblocked.so");
        fs::create_dir(&blocked_dest).unwrap();
        fs::write(blocked_dest.join("occupant"), b"pre-existing").unwrap();

        let payloads = [
            ("pkg/lib/libgood.so", good_sha.as_str()),
            ("pkg/lib/libblocked.so", blocked_sha.as_str()),
        ];
        let err = verify_and_extract(
            &tarball_path,
            &tarball_sha,
            &tarball_sha,
            "https://example.invalid/x.tgz",
            &payloads,
            dest.path(),
        )
        .unwrap_err();

        assert!(
            matches!(err, EmbedError::Io(_)),
            "rename onto an existing directory should surface as a real io::Error: {err:?}"
        );
        assert!(
            no_tmp_files_remain(dest.path()),
            "no .tmp scratch files should remain after an io error mid-extract"
        );
        assert!(
            !dest.path().join("libgood.so").exists(),
            "a payload already renamed into place earlier in this call must be rolled back too"
        );
    }

    #[test]
    fn preseeded_valid_files_skip_download() {
        let dest = TempDir::new().unwrap();
        let payload_bytes: &[u8] = b"preseeded payload bytes";
        // sha256("preseeded payload bytes")
        const PAYLOAD_SHA: &str =
            "cc424f044ba01f0660fee7ce94a0eaad9440f3e52a68b437dcc1003aba10f283";
        debug_assert_eq!(sha256_hex(payload_bytes), PAYLOAD_SHA);
        fs::write(dest.path().join("libfake.so"), payload_bytes).unwrap();

        static RT: RemoteRuntime = RemoteRuntime {
            url: "https://invalid.invalid/x.tgz",
            tarball_sha256: "unused",
            payloads: &[("pkg/lib/libfake.so", PAYLOAD_SHA)],
            cache_subdir: "test",
            approx_download_mb: 1,
        };

        // If this actually tried the network, it would fail (or hang) resolving
        // invalid.invalid — success here proves the preseed fast path was taken.
        ensure_downloaded(&RT, dest.path()).unwrap();
    }

    #[test]
    fn corrupted_preseeded_payload_detected() {
        let src = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let correct_bytes = b"correct payload bytes";
        let correct_sha = sha256_hex(correct_bytes);
        fs::write(dest.path().join("libfake.so"), b"wrong bytes").unwrap();

        let payloads = [("pkg/lib/libfake.so", correct_sha.as_str())];
        assert!(
            !payloads_valid(&payloads, dest.path()),
            "corrupted preseeded payload should fail validation"
        );

        // Simulate the re-acquisition ensure_downloaded would perform, via the local fixture
        // reader/file-path entry point (no network involved).
        let tarball_path =
            build_fixture_tarball(src.path(), &[("pkg/lib/libfake.so", correct_bytes)]);
        let tarball_sha = ModelCache::sha256_file(&tarball_path).unwrap();
        verify_and_extract(
            &tarball_path,
            &tarball_sha,
            &tarball_sha,
            "https://example.invalid/x.tgz",
            &payloads,
            dest.path(),
        )
        .unwrap();

        assert!(payloads_valid(&payloads, dest.path()));
        assert_eq!(
            fs::read(dest.path().join("libfake.so")).unwrap(),
            correct_bytes
        );
    }

    #[test]
    fn missing_payload_in_tarball_errors() {
        let src = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let tarball_path =
            build_fixture_tarball(src.path(), &[("pkg/lib/libpresent.so", b"present")]);
        let tarball_sha = ModelCache::sha256_file(&tarball_path).unwrap();

        let payloads = [
            ("pkg/lib/libpresent.so", sha256_hex(b"present")),
            ("pkg/lib/libmissing.so", "0".repeat(64)),
        ];
        let payloads_ref: Vec<(&str, &str)> =
            payloads.iter().map(|(p, s)| (*p, s.as_str())).collect();

        let err = verify_and_extract(
            &tarball_path,
            &tarball_sha,
            &tarball_sha,
            "https://example.invalid/x.tgz",
            &payloads_ref,
            dest.path(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("libmissing.so"));
        assert!(
            fs::read_dir(dest.path()).unwrap().next().is_none(),
            "no partial payloads should remain when a listed payload is missing"
        );
    }

    // --- F2: process-unique tmp naming ------------------------------------------------------

    #[test]
    fn unique_tmp_path_embeds_pid_and_never_collides_across_calls() {
        let base = Path::new("/tmp/localdb-ort-test/libfake.so");
        let a = unique_tmp_path(base);
        let b = unique_tmp_path(base);

        assert_ne!(
            a, b,
            "sequential calls for the same base must never collide"
        );

        let pid = std::process::id().to_string();
        assert!(
            a.to_string_lossy().contains(&pid) && b.to_string_lossy().contains(&pid),
            "tmp path should embed this process's pid: {a:?} / {b:?}"
        );
        assert_eq!(
            a.parent(),
            base.parent(),
            "tmp path must stay in the same directory"
        );
        assert!(a.extension().is_some_and(|ext| ext == "tmp"));
        assert!(b.extension().is_some_and(|ext| ext == "tmp"));
    }

    // --- F4: verification-marker fast path ----------------------------------------------------

    #[test]
    fn missing_marker_full_hash_passes_and_writes_marker() {
        let dest = TempDir::new().unwrap();
        let payload_bytes: &[u8] = b"payload bytes for the marker-writing test";
        let sha = sha256_hex(payload_bytes);
        fs::write(dest.path().join("libfake.so"), payload_bytes).unwrap();

        let payloads = [("pkg/lib/libfake.so", sha.as_str())];
        assert!(
            !marker_path(dest.path()).exists(),
            "no marker should exist before the first check"
        );
        assert!(
            payloads_valid(&payloads, dest.path()),
            "full hash check should pass with no marker present"
        );
        assert!(
            marker_path(dest.path()).exists(),
            "payloads_valid should write the marker after a successful full-hash verification"
        );

        let marker = read_verified_marker(dest.path()).unwrap();
        assert_eq!(marker.payloads.len(), 1);
        assert_eq!(marker.payloads[0].file_name, "libfake.so");
        assert_eq!(marker.payloads[0].sha256, sha);
        assert_eq!(marker.payloads[0].size, payload_bytes.len() as u64);
    }

    #[test]
    fn valid_marker_with_same_size_corruption_still_passes_documented_tradeoff() {
        let dest = TempDir::new().unwrap();
        let original: &[u8] = b"original payload bytes, fixed length!!";
        let sha = sha256_hex(original);
        let path = dest.path().join("libfake.so");
        fs::write(&path, original).unwrap();

        let payloads = [("pkg/lib/libfake.so", sha.as_str())];
        assert!(
            payloads_valid(&payloads, dest.path()),
            "initial full hash should pass"
        );
        assert!(marker_path(dest.path()).exists());

        // Corrupt the file's content but keep its exact byte length.
        let corrupted: Vec<u8> = original.iter().map(|b| b ^ 0xFF).collect();
        assert_eq!(corrupted.len(), original.len());
        fs::write(&path, &corrupted).unwrap();

        assert!(
            payloads_valid(&payloads, dest.path()),
            "documented tradeoff: a valid marker plus a matching file size skips the full \
             content hash, so same-size content corruption is not caught by the fast path"
        );
    }

    #[test]
    fn size_mismatch_fails_validation_even_with_valid_marker() {
        let dest = TempDir::new().unwrap();
        let original: &[u8] = b"original content for the size-mismatch test";
        let sha = sha256_hex(original);
        let path = dest.path().join("libfake.so");
        fs::write(&path, original).unwrap();

        let payloads = [("pkg/lib/libfake.so", sha.as_str())];
        assert!(payloads_valid(&payloads, dest.path()));
        assert!(marker_path(dest.path()).exists());

        // Truncate: same marker, different size.
        fs::write(&path, b"short").unwrap();

        assert!(
            !payloads_valid(&payloads, dest.path()),
            "a size mismatch must fail both the fast path (size check) and the full-hash fallback"
        );
    }

    #[test]
    fn stale_marker_sha_falls_back_to_full_hash() {
        let dest = TempDir::new().unwrap();
        let payload_bytes: &[u8] = b"payload content unaffected by a pin bump";
        let current_sha = sha256_hex(payload_bytes);
        let path = dest.path().join("libfake.so");
        fs::write(&path, payload_bytes).unwrap();

        // Hand-write a marker recording a *different* sha256 than the current pinned table
        // entry — simulating a version pin bump that changed the expected hash without the
        // cached file itself changing.
        let stale_marker = VerifiedMarker {
            payloads: vec![VerifiedPayload {
                file_name: "libfake.so".to_string(),
                size: payload_bytes.len() as u64,
                sha256: "0".repeat(64),
            }],
        };
        fs::write(
            marker_path(dest.path()),
            serde_json::to_vec(&stale_marker).unwrap(),
        )
        .unwrap();

        let payloads = [("pkg/lib/libfake.so", current_sha.as_str())];
        assert!(
            !fast_path_valid(&payloads, dest.path()),
            "a marker recording a stale sha (vs. the current pinned table) must not fast-path"
        );
        assert!(
            payloads_valid(&payloads, dest.path()),
            "the full-hash fallback should still pass since the file's real content matches the \
             current pinned sha"
        );
    }

    #[test]
    fn invalid_payload_removes_stale_marker() {
        let dest = TempDir::new().unwrap();
        let correct: &[u8] = b"correct payload content";
        let correct_sha = sha256_hex(correct);
        let path = dest.path().join("libfake.so");
        fs::write(&path, correct).unwrap();

        let payloads = [("pkg/lib/libfake.so", correct_sha.as_str())];
        assert!(payloads_valid(&payloads, dest.path()));
        assert!(marker_path(dest.path()).exists());

        // Corrupt with a *different* size, so both the fast path and the full hash reject it.
        fs::write(&path, b"wrong content, different length entirely").unwrap();
        assert!(!payloads_valid(&payloads, dest.path()));
        assert!(
            !marker_path(dest.path()).exists(),
            "a now-invalid marker must not linger to lie about a directory known to be bad"
        );
    }

    #[test]
    fn unsupported_target_flavor_errors() {
        let err = cpu_flavor_for("windows", "x86_64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("windows/x86_64"));
        assert!(msg.contains("linux/x86_64"));
        assert!(msg.contains("local-onnx"));
        assert!(msg.contains("ORT_DYLIB_PATH"));

        let err2 = cpu_flavor_for("macos", "x86_64").unwrap_err();
        assert!(err2.to_string().contains("macos/x86_64"));
    }

    /// Real network test — skipped unless `LOCALDB_TEST_ORT_DOWNLOAD=1`, since it downloads
    /// an actual multi-MB release asset from GitHub.
    #[test]
    fn real_download_cpu_flavor() {
        if std::env::var("LOCALDB_TEST_ORT_DOWNLOAD").as_deref() != Ok("1") {
            eprintln!("skipping real_download_cpu_flavor (set LOCALDB_TEST_ORT_DOWNLOAD=1 to run)");
            return;
        }

        let dest = TempDir::new().unwrap();
        let rt = cpu_flavor_for_target().unwrap();

        ensure_downloaded(rt, dest.path()).unwrap();
        for (path_in_tar, expected_sha) in rt.payloads {
            let file = payload_dest(dest.path(), path_in_tar);
            assert!(file.is_file(), "{} should exist", file.display());
            assert_eq!(&ModelCache::sha256_file(&file).unwrap(), expected_sha);
        }

        let mtimes_before: Vec<_> = rt
            .payloads
            .iter()
            .map(|(p, _)| {
                fs::metadata(payload_dest(dest.path(), p))
                    .unwrap()
                    .modified()
                    .unwrap()
            })
            .collect();

        // Second call should be a no-op fast path (no re-download, no re-write).
        ensure_downloaded(rt, dest.path()).unwrap();

        let mtimes_after: Vec<_> = rt
            .payloads
            .iter()
            .map(|(p, _)| {
                fs::metadata(payload_dest(dest.path(), p))
                    .unwrap()
                    .modified()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            mtimes_before, mtimes_after,
            "second ensure_downloaded call should not rewrite already-valid payloads"
        );
    }

    /// Real network test for the CUDA flavor — Linux/x86_64 only (CI runs this on GPU-less
    /// ubuntu runners; it only downloads+verifies the tarball, never touches `ort` init).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn real_download_cuda_flavor() {
        if std::env::var("LOCALDB_TEST_ORT_DOWNLOAD").as_deref() != Ok("1") {
            eprintln!(
                "skipping real_download_cuda_flavor (set LOCALDB_TEST_ORT_DOWNLOAD=1 to run)"
            );
            return;
        }

        let dest = TempDir::new().unwrap();
        ensure_downloaded(&CUDA_LINUX_X64, dest.path()).unwrap();
        for (path_in_tar, expected_sha) in CUDA_LINUX_X64.payloads {
            let file = payload_dest(dest.path(), path_in_tar);
            assert!(file.is_file(), "{} should exist", file.display());
            assert_eq!(&ModelCache::sha256_file(&file).unwrap(), expected_sha);
        }
    }
}
