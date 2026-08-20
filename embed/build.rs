//! Downloads and verifies Microsoft's official ONNX Runtime shared library, then embeds it
//! into the `embed` crate for the `local-onnx` feature.
//!
//! # Why this exists (issue #133)
//!
//! `ort`'s `download-binaries` feature makes `ort-sys` statically link pyke.io's prebuilt
//! ONNX Runtime archive into our executable. That archive is built with GCC 14 on Ubuntu
//! 24.04 and references `__isoc23_strtol*` symbols, which gives the *release binary itself*
//! a GLIBC >= 2.38 floor — it refuses to start on glibc-2.35 distros (Linux Mint 21.x,
//! Ubuntu 22.04). It is also ABI-incompatible with GCC-11 libstdc++ when built on
//! ubuntu-22.04 (pykeio/ort#523, unresolved upstream as of writing).
//!
//! Instead, `embed`'s `ort` dependency (see `Cargo.toml`) uses `load-dynamic` — `dlopen`,
//! no ONNX Runtime ABI is linked into our executable at all. This build script downloads
//! *Microsoft's official* ONNX Runtime release for the target platform, verifies its
//! sha256 against a pinned value, and extracts the shared library into `OUT_DIR`.
//! `src/ort_runtime.rs` embeds that file via `include_bytes!(env!("LOCALDB_ORT_LIB_PATH"))`,
//! writes it out to the user's cache dir on first use, and calls `ort::init_from`.
//!
//! Verified floors of the pinned Linux 1.24.4 builds (via `objdump -T`): max `GLIBC_2.27`,
//! `GLIBCXX_3.4.22`, `CXXABI_1.3.11` — well under Ubuntu 22.04's baseline (`GLIBC_2.35`).
//! Their only dlopen-time dependencies are baseline system libraries plus `libstdc++.so.6`.
//! The macOS build's `LC_BUILD_VERSION` declares a minimum of macOS 14.0.
//!
//! This script is a no-op unless the `local-onnx` feature is enabled and the target OS is one
//! Microsoft ships an official release for — Linux, macOS, or Windows. Other targets get no
//! embedded runtime, and `local-onnx` is not usable there.
//!
//! # The `ort_embedded` cfg
//!
//! Whenever this script embeds a runtime it emits `cargo:rustc-cfg=ort_embedded`, and it is
//! the only thing that ever does. Everything that must not compile without an embedded
//! runtime — `ort_runtime::imp`, `factory`'s local-ONNX constructors — gates on that one
//! cfg rather than restating `all(feature = "local-onnx", any(target_os = …))` in each
//! place. Restating it invites skew, and skew here is silent: the two ends of it are a
//! `provider: local` that dies deep inside a half-initialized ORT call and one that never
//! compiles at all.

use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// ONNX Runtime version embedded for `local-onnx`. Newest release within `ort
/// 2.0.0-rc.12`'s supported 1.17-1.24 range whose Linux builds satisfy Ubuntu 22.04's
/// glibc/libstdc++/libgcc baselines (verified 2026-07-02 via `objdump -T`).
const ORT_VERSION: &str = "1.24.4";

const RELEASE_BASE_URL: &str = "https://github.com/microsoft/onnxruntime/releases/download/v1.24.4";

/// How a target's asset is packaged. Microsoft ships gzip tarballs for Linux and macOS and
/// a zip for Windows.
enum Archive {
    Tgz,
    Zip,
}

/// One target's downloadable asset: archive name, its pinned sha256, how it is packaged, and
/// the path of the shared library payload inside it.
///
/// `payload_in_archive` names exactly one entry, and it is a compile-time constant — that is
/// what keeps the Windows asset's 382 MB `lib/onnxruntime.pdb` out of the binary. Nothing
/// here iterates or globs, so no archive entry other than the named one can ever be embedded.
struct Asset {
    archive: &'static str,
    archive_sha256: &'static str,
    payload_in_archive: &'static str,
    archive_kind: Archive,
}

const LINUX_X64: Asset = Asset {
    archive: "onnxruntime-linux-x64-1.24.4.tgz",
    archive_sha256: "3a211fbea252c1e66290658f1b735b772056149f28321e71c308942cdb54b747",
    payload_in_archive: "onnxruntime-linux-x64-1.24.4/lib/libonnxruntime.so.1.24.4",
    archive_kind: Archive::Tgz,
};

const LINUX_AARCH64: Asset = Asset {
    archive: "onnxruntime-linux-aarch64-1.24.4.tgz",
    archive_sha256: "866109a9248d057671a039b9d725be4bd86888e3754140e6701ec621be9d4d7e",
    payload_in_archive: "onnxruntime-linux-aarch64-1.24.4/lib/libonnxruntime.so.1.24.4",
    archive_kind: Archive::Tgz,
};

const OSX_ARM64: Asset = Asset {
    archive: "onnxruntime-osx-arm64-1.24.4.tgz",
    archive_sha256: "93787795f47e1eee369182e43ed51b9e5da0878ab0346aecf4258979b8bba989",
    payload_in_archive: "onnxruntime-osx-arm64-1.24.4/lib/libonnxruntime.1.24.4.dylib",
    archive_kind: Archive::Tgz,
};

/// `onnxruntime.dll` alone, deliberately: the archive also carries
/// `lib/onnxruntime_providers_shared.dll`, but that is the loader shim for *shared* execution
/// providers (CUDA, TensorRT). localdb registers no execution providers and runs CPU-only, so
/// nothing ever asks for it. It is not embedded until something proves it is needed — every
/// byte here is `.rodata` in the shipped binary.
const WINDOWS_X64: Asset = Asset {
    archive: "onnxruntime-win-x64-1.24.4.zip",
    archive_sha256: "d2319fddfb6ea4db99ccc4b60c85c517bcd855721f5daa6a06d40d7cb2ee2357",
    payload_in_archive: "onnxruntime-win-x64-1.24.4/lib/onnxruntime.dll",
    archive_kind: Archive::Zip,
};

fn main() {
    // We emit explicit rerun-if directives below, which disables cargo's default "rerun on
    // any file change in the package" heuristic — so re-add build.rs itself explicitly.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LOCALDB_ORT_LIB");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_LOCAL_ONNX");
    // Declare the cfg unconditionally, including on the paths below that never set it —
    // otherwise `#[cfg(ort_embedded)]` trips the `unexpected_cfgs` lint, which CI denies.
    println!("cargo:rustc-check-cfg=cfg(ort_embedded)");

    if env::var("CARGO_FEATURE_LOCAL_ONNX").is_err() {
        // local-onnx disabled: nothing to embed. (Other features, e.g. local-coreml, never
        // touch ort/this build script's outputs.)
        return;
    }

    // Escape hatch for offline/distro builds: use a caller-provided ONNX Runtime library
    // directly, skipping the download+verify path entirely. Checked before the target gate
    // below, because a target we ship no asset for is exactly when someone needs it.
    if let Ok(local_lib) = env::var("LOCALDB_ORT_LIB") {
        let local_path = PathBuf::from(&local_lib);
        if !local_path.is_file() {
            panic!("LOCALDB_ORT_LIB={local_lib} does not point to an existing file");
        }
        let sha256 = sha256_file(&local_path);
        emit_outputs(&local_path, &sha256);
        return;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !matches!(target_os.as_str(), "linux" | "macos" | "windows") {
        // No official Microsoft ONNX Runtime asset shipped for this target (yet), so there is
        // nothing to embed and `ort_embedded` stays unset. `factory.rs` turns that into a
        // clean "no local ONNX backend on this platform" error rather than a failed dlopen.
        return;
    }

    // Never use the host OS/arch here — Linux aarch64 release builds are cross-compiled
    // from an x86_64 host, so only the *target* cfg vars are meaningful.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let asset = match (target_os.as_str(), target_arch.as_str()) {
        ("linux", "x86_64") => &LINUX_X64,
        ("linux", "aarch64") => &LINUX_AARCH64,
        ("macos", "aarch64") => &OSX_ARM64,
        ("windows", "x86_64") => &WINDOWS_X64,
        (os, arch) => {
            panic!(
                "localdb's `local-onnx` feature has no embedded ONNX Runtime build for \
                 target {os}/{arch}. Supported: linux/x86_64, linux/aarch64, macos/aarch64, \
                 windows/x86_64. Build without `--features local-onnx`, or set \
                 LOCALDB_ORT_LIB to the path of a local ONNX Runtime shared library to \
                 override this check."
            );
        }
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));

    let lib_filename = Path::new(asset.payload_in_archive)
        .file_name()
        .expect("payload_in_archive has a file name")
        .to_str()
        .expect("payload file name is valid UTF-8");
    let lib_dest = out_dir.join(lib_filename);

    if !lib_dest.is_file() {
        let archive_path = ensure_archive(&out_dir, asset);
        extract_payload(&archive_path, asset, &lib_dest);
    }

    let sha256 = sha256_file(&lib_dest);
    emit_outputs(&lib_dest, &sha256);
}

fn emit_outputs(lib_path: &Path, sha256: &str) {
    let abs_path = fs::canonicalize(lib_path).unwrap_or_else(|e| {
        panic!(
            "failed to canonicalize embedded ONNX Runtime lib path {}: {e}",
            lib_path.display()
        )
    });
    println!(
        "cargo:rustc-env=LOCALDB_ORT_LIB_PATH={}",
        abs_path.display()
    );
    println!("cargo:rustc-env=LOCALDB_ORT_LIB_SHA256={sha256}");
    println!("cargo:rustc-env=LOCALDB_ORT_VERSION={ORT_VERSION}");
    // The single source of truth for "this build has an embedded ONNX Runtime". Emitted
    // here and nowhere else, so it cannot disagree with the env vars above.
    println!("cargo:rustc-cfg=ort_embedded");
}

/// Download `asset`'s archive into `out_dir` (skipping the download if it's already present
/// with a matching sha256), verify its checksum against the pinned constant, and return its
/// path. Fails the build (`panic!`) on checksum mismatch — never silently ship an
/// unverified binary.
fn ensure_archive(out_dir: &Path, asset: &Asset) -> PathBuf {
    let archive_path = out_dir.join(asset.archive);

    if archive_path.is_file() && sha256_file(&archive_path) == asset.archive_sha256 {
        return archive_path;
    }

    let url = format!("{RELEASE_BASE_URL}/{}", asset.archive);
    eprintln!("embed/build.rs: downloading {url}");
    let bytes = download(&url);

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual = hex::encode(hasher.finalize());
    if actual != asset.archive_sha256 {
        panic!(
            "downloaded {url} but its sha256 ({actual}) does not match the pinned value \
             ({}). Refusing to embed an unverified ONNX Runtime binary. This may mean the \
             pinned constant in embed/build.rs is stale, or the download was corrupted/\
             tampered with — retry, and if it persists, verify the release asset manually.",
            asset.archive_sha256
        );
    }

    let tmp_path = out_dir.join(format!("{}.tmp", asset.archive));
    fs::write(&tmp_path, &bytes)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", tmp_path.display()));
    fs::rename(&tmp_path, &archive_path).unwrap_or_else(|e| {
        panic!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            archive_path.display()
        )
    });
    archive_path
}

fn download(url: &str) -> Vec<u8> {
    // ureq follows redirects by default (GitHub release assets 302 to
    // objects.githubusercontent.com) and uses rustls for TLS (see Cargo.toml).
    let response = ureq::get(url)
        .call()
        .unwrap_or_else(|e| panic!("failed to download {url}: {e}"));
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .unwrap_or_else(|e| panic!("failed to read response body from {url}: {e}"));
    bytes
}

/// Extract `asset`'s single named payload entry from the archive at `archive_path` to
/// `dest`, atomically (write to a `.tmp` sibling, then rename).
///
/// Only `asset.payload_in_archive` is ever read; both branches locate that one entry by exact
/// name and ignore everything else in the archive.
fn extract_payload(archive_path: &Path, asset: &Asset, dest: &Path) {
    let payload_path = asset.payload_in_archive;
    let file = fs::File::open(archive_path)
        .unwrap_or_else(|e| panic!("failed to open {}: {e}", archive_path.display()));

    match asset.archive_kind {
        Archive::Tgz => {
            // A tar has no index: the only way to find an entry is to stream through until
            // its name matches.
            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
            let entries = archive.entries().unwrap_or_else(|e| {
                panic!("failed to read entries of {}: {e}", archive_path.display())
            });
            for entry in entries {
                let mut entry = entry.unwrap_or_else(|e| panic!("failed to read tar entry: {e}"));
                let entry_path = entry
                    .path()
                    .unwrap_or_else(|e| panic!("failed to read tar entry path: {e}"))
                    .to_path_buf();
                if entry_path.to_string_lossy() == payload_path {
                    write_atomically(&mut entry, dest, payload_path);
                    return;
                }
            }
            panic!(
                "archive {} did not contain expected payload path {payload_path}",
                archive_path.display()
            );
        }
        Archive::Zip => {
            // A zip has a central directory, so the payload is addressed by name directly —
            // no entry other than `payload_path` is even decompressed. That matters here:
            // the Windows archive carries a 382 MB `onnxruntime.pdb` alongside the 14 MB DLL.
            let mut archive = zip::ZipArchive::new(file).unwrap_or_else(|e| {
                panic!("failed to read zip archive {}: {e}", archive_path.display())
            });
            let mut entry = archive.by_name(payload_path).unwrap_or_else(|e| {
                panic!(
                    "archive {} did not contain expected payload path {payload_path}: {e}",
                    archive_path.display()
                )
            });
            write_atomically(&mut entry, dest, payload_path);
        }
    }
}

/// Stream `reader` into `dest` via a `.tmp` sibling, then rename — so a failed or interrupted
/// extraction never leaves a truncated library that the `lib_dest.is_file()` check would
/// later mistake for a complete one.
fn write_atomically(reader: &mut impl Read, dest: &Path, payload_path: &str) {
    let tmp_dest = dest.with_extension("tmp");
    let mut out = fs::File::create(&tmp_dest)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", tmp_dest.display()));
    std::io::copy(reader, &mut out).unwrap_or_else(|e| {
        panic!(
            "failed to extract {payload_path} to {}: {e}",
            tmp_dest.display()
        )
    });
    drop(out);
    fs::rename(&tmp_dest, dest).unwrap_or_else(|e| {
        panic!(
            "failed to rename {} -> {}: {e}",
            tmp_dest.display(),
            dest.display()
        )
    });
}

fn sha256_file(path: &Path) -> String {
    let data = fs::read(path)
        .unwrap_or_else(|e| panic!("failed to read {} for checksum: {e}", path.display()));
    let mut hasher = Sha256::new();
    hasher.update(&data);
    hex::encode(hasher.finalize())
}
