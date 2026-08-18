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
//! This script is a no-op unless the `local-onnx` feature is enabled and the target OS is
//! Linux or macOS (Windows/other targets get no embedded runtime and `local-onnx` simply
//! isn't buildable there yet).
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

/// One target's downloadable asset: tarball name, its pinned sha256, and the path of the
/// shared library payload inside the tarball.
struct Asset {
    archive: &'static str,
    archive_sha256: &'static str,
    payload_in_tar: &'static str,
}

const LINUX_X64: Asset = Asset {
    archive: "onnxruntime-linux-x64-1.24.4.tgz",
    archive_sha256: "3a211fbea252c1e66290658f1b735b772056149f28321e71c308942cdb54b747",
    payload_in_tar: "onnxruntime-linux-x64-1.24.4/lib/libonnxruntime.so.1.24.4",
};

const LINUX_AARCH64: Asset = Asset {
    archive: "onnxruntime-linux-aarch64-1.24.4.tgz",
    archive_sha256: "866109a9248d057671a039b9d725be4bd86888e3754140e6701ec621be9d4d7e",
    payload_in_tar: "onnxruntime-linux-aarch64-1.24.4/lib/libonnxruntime.so.1.24.4",
};

const OSX_ARM64: Asset = Asset {
    archive: "onnxruntime-osx-arm64-1.24.4.tgz",
    archive_sha256: "93787795f47e1eee369182e43ed51b9e5da0878ab0346aecf4258979b8bba989",
    payload_in_tar: "onnxruntime-osx-arm64-1.24.4/lib/libonnxruntime.1.24.4.dylib",
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
    if target_os != "linux" && target_os != "macos" {
        // No official Microsoft ONNX Runtime asset shipped for this target (yet). Building
        // `local-onnx` for e.g. Windows isn't supported by this build script.
        return;
    }

    // Never use the host OS/arch here — Linux aarch64 release builds are cross-compiled
    // from an x86_64 host, so only the *target* cfg vars are meaningful.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let asset = match (target_os.as_str(), target_arch.as_str()) {
        ("linux", "x86_64") => &LINUX_X64,
        ("linux", "aarch64") => &LINUX_AARCH64,
        ("macos", "aarch64") => &OSX_ARM64,
        (os, arch) => {
            panic!(
                "localdb's `local-onnx` feature has no embedded ONNX Runtime build for \
                 target {os}/{arch}. Supported: linux/x86_64, linux/aarch64, macos/aarch64. \
                 Build without `--features local-onnx`, or set LOCALDB_ORT_LIB to the path \
                 of a local ONNX Runtime shared library to override this check."
            );
        }
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));

    let lib_filename = Path::new(asset.payload_in_tar)
        .file_name()
        .expect("payload_in_tar has a file name")
        .to_str()
        .expect("payload file name is valid UTF-8");
    let lib_dest = out_dir.join(lib_filename);

    if !lib_dest.is_file() {
        let tarball_path = ensure_tarball(&out_dir, asset);
        extract_payload(&tarball_path, asset.payload_in_tar, &lib_dest);
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

/// Download `asset`'s tarball into `out_dir` (skipping the download if it's already present
/// with a matching sha256), verify its checksum against the pinned constant, and return its
/// path. Fails the build (`panic!`) on checksum mismatch — never silently ship an
/// unverified binary.
fn ensure_tarball(out_dir: &Path, asset: &Asset) -> PathBuf {
    let tarball_path = out_dir.join(asset.archive);

    if tarball_path.is_file() && sha256_file(&tarball_path) == asset.archive_sha256 {
        return tarball_path;
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
    fs::rename(&tmp_path, &tarball_path).unwrap_or_else(|e| {
        panic!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            tarball_path.display()
        )
    });
    tarball_path
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

/// Extract a single `payload_path` entry from the gzip tarball at `tarball_path` to
/// `dest`, atomically (write to a `.tmp` sibling, then rename).
fn extract_payload(tarball_path: &Path, payload_path: &str, dest: &Path) {
    let file = fs::File::open(tarball_path)
        .unwrap_or_else(|e| panic!("failed to open {}: {e}", tarball_path.display()));
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    let entries = archive
        .entries()
        .unwrap_or_else(|e| panic!("failed to read entries of {}: {e}", tarball_path.display()));

    let mut found = false;
    for entry in entries {
        let mut entry = entry.unwrap_or_else(|e| panic!("failed to read tar entry: {e}"));
        let entry_path = entry
            .path()
            .unwrap_or_else(|e| panic!("failed to read tar entry path: {e}"))
            .to_path_buf();
        if entry_path.to_string_lossy() == payload_path {
            let tmp_dest = dest.with_extension("tmp");
            let mut out = fs::File::create(&tmp_dest)
                .unwrap_or_else(|e| panic!("failed to create {}: {e}", tmp_dest.display()));
            std::io::copy(&mut entry, &mut out).unwrap_or_else(|e| {
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
            found = true;
            break;
        }
    }

    if !found {
        panic!(
            "tarball {} did not contain expected payload path {payload_path}",
            tarball_path.display()
        );
    }
}

fn sha256_file(path: &Path) -> String {
    let data = fs::read(path)
        .unwrap_or_else(|e| panic!("failed to read {} for checksum: {e}", path.display()));
    let mut hasher = Sha256::new();
    hasher.update(&data);
    hex::encode(hasher.finalize())
}
