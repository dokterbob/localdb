//! Runtime (first-use) download of Microsoft's official ONNX Runtime from a sha256-pinned
//! "flavor table" — CPU flavors per target, plus a CUDA flavor for linux/x86_64.
//!
//! # Why download at runtime, not embed at build time (issue #76)
//!
//! `embed/build.rs` currently downloads the ONNX Runtime *at build time* and bakes it into
//! the binary via `include_bytes!` (see `ort_runtime.rs`). That means every build embeds a
//! single CPU library regardless of what the machine that eventually runs the binary actually
//! needs — roughly a third of that artifact size is dead weight on any given machine: macOS
//! defaults to CoreML and never touches the embedded ONNX Runtime at all, and CUDA machines
//! need a GPU-capable library the CPU build can't provide. A flavor table downloaded on first
//! use instead lets one release binary serve CPU-only Linux/macOS, Linux+CUDA today, and
//! Linux+ROCm (or other accelerators) later, without bloating every binary with libraries most
//! installs will never load.
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
//! not touch `ort` initialization (see `ort_runtime.rs`) or embedder construction
//! (`factory.rs`) — those are wired up in later chunks.

// Nothing outside this module's own tests consumes it yet — `ort_runtime.rs`'s flavor-based
// init and `factory.rs`'s CUDA selection are wired up in later chunks (issue #76). Remove
// this allow once they land.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{error::EmbedError, model_cache::ModelCache};

/// ONNX Runtime version this flavor table is pinned to. Kept in sync with `build.rs`'s own
/// copy of the same constant (that copy is removed once `build.rs` itself is deleted).
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

/// Linux x86_64 CPU build. Same tarball/payload `build.rs` currently embeds; the extracted
/// file name and bytes are identical, so existing user caches under this file name remain
/// valid once this module supersedes `build.rs`.
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
/// network access (offline pre-seed path). Otherwise it streams `rt.url` to a temp file
/// (hashing as bytes arrive — tarballs run up to ~200 MB, never buffered whole), verifies the
/// tarball's sha256, then streams each listed payload out of the (gzipped) tar archive to a
/// `.tmp` sibling of its final path, verifies its sha256, and renames it into place. On any
/// hash mismatch (tarball or payload), all partial output is removed and a hard error is
/// returned naming the URL and the expected/actual hashes.
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

    let tarball_tmp = dest_dir.join(format!("{}.download.tgz.tmp", rt.cache_subdir));
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
    // part of the cache contract.
    let _ = fs::remove_file(&tarball_tmp);
    result?;

    tracing::info!(url = rt.url, "ONNX Runtime download complete");
    Ok(())
}

/// True iff every one of `payloads` already exists under `dest_dir` with a matching sha256 —
/// the offline pre-seed / already-cached fast path.
fn payloads_valid(payloads: &[(&str, &str)], dest_dir: &Path) -> bool {
    payloads.iter().all(|(path_in_tar, expected_sha)| {
        let dest = payload_dest(dest_dir, path_in_tar);
        ModelCache::sha256_file(&dest)
            .map(|actual| actual == *expected_sha)
            .unwrap_or(false)
    })
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
/// `dest_dir` (each written to a `.tmp` sibling of its final path, verified by sha256, then
/// renamed into place). On any payload hash mismatch or a payload missing from the archive,
/// every file this call wrote is removed before returning the error — `dest_dir` is left
/// exactly as it was found.
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
        let tmp = dest.with_extension("tmp");
        let actual_sha = {
            let out = fs::File::create(&tmp).map_err(EmbedError::Io)?;
            let mut hashing = HashingWriter::new(out);
            io::copy(&mut entry, &mut hashing).map_err(EmbedError::Io)?;
            hashing.finalize_hex()
        };

        if actual_sha != expected_sha {
            let _ = fs::remove_file(&tmp);
            cleanup(&written);
            return Err(EmbedError::Internal(format!(
                "extracted payload {entry_path} sha256 mismatch: expected {expected_sha}, got \
                 {actual_sha}. This may mean the pinned constant is stale, or the download was \
                 corrupted/tampered with — retry, and if it persists, verify the release asset \
                 manually."
            )));
        }

        fs::rename(&tmp, &dest).map_err(EmbedError::Io)?;
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

/// Stream-download `url` to `dest`, computing its sha256 as bytes arrive so the tarball is
/// never buffered whole in memory (CUDA tarballs run to ~200 MB), returning the hex digest.
fn download_tarball_streaming(url: &str, dest: &Path) -> Result<String, EmbedError> {
    // ureq follows redirects by default (GitHub release assets 302 to
    // objects.githubusercontent.com) and uses rustls for TLS (see Cargo.toml).
    let response = ureq::get(url)
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
