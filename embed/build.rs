//! Downloads and verifies Microsoft's official ONNX Runtime shared library, then embeds it
//! into the `embed` crate for the `local-onnx` feature.
//!
//! # Why this exists
//!
//! `ort`'s `download-binaries` feature makes `ort-sys` statically link pyke.io's prebuilt
//! ONNX Runtime archive into our executable. That archive is built with GCC 14 on Ubuntu
//! 24.04 and references `__isoc23_strtol*` symbols, which gives the *release binary itself*
//! a GLIBC >= 2.38 floor — it refuses to start on glibc-2.35 distros (Linux Mint 21.x,
//! Ubuntu 22.04). It is also ABI-incompatible with GCC-11 libstdc++ when built on
//! ubuntu-22.04.
//!
//! Instead, `embed`'s `ort` dependency (see `Cargo.toml`) uses `load-dynamic` — `dlopen`,
//! no ONNX Runtime ABI is linked into our executable at all. This build script downloads
//! Microsoft's official NuGet package `Microsoft.ML.OnnxRuntime`, which bundles native
//! builds for every supported platform in one `.nupkg` (itself a zip archive, signed by
//! Microsoft and immutable once published), verifies its sha256 against a pinned value,
//! and extracts the one shared library this build's target platform needs into `OUT_DIR`.
//! `src/ort_runtime.rs` embeds that file via `include_bytes!(env!("LOCALDB_ORT_LIB_PATH"))`,
//! writes it out to the user's cache dir on first use, and calls `ort::init_from`.
//!
//! Verified floors of the pinned 1.24.4 builds (via `objdump -T`): max `GLIBC_2.27`,
//! `GLIBCXX_3.4.22`, `CXXABI_1.3.11` — well under Ubuntu 22.04's baseline (`GLIBC_2.35`).
//! Their only dlopen-time dependencies are baseline system libraries plus `libstdc++.so.6`.
//! The macOS build's `LC_BUILD_VERSION` declares a minimum of macOS 14.0.
//!
//! This script is a no-op unless the `local-onnx` feature is enabled and the target OS is
//! Linux or macOS (Windows/other targets get no embedded runtime and `local-onnx` simply
//! isn't buildable there yet).

use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// ONNX Runtime version embedded for `local-onnx`. Newest release within `ort
/// 2.0.0-rc.12`'s supported 1.17-1.24 range whose Linux builds satisfy Ubuntu 22.04's
/// glibc/libstdc++/libgcc baselines (verified via `objdump -T`).
const ORT_VERSION: &str = "1.24.4";

/// NuGet package ID, lowercase as required by the flatcontainer API below.
const NUGET_PACKAGE_ID: &str = "microsoft.ml.onnxruntime";

/// sha256 of the pinned `.nupkg` for `ORT_VERSION`. NuGet package versions are immutable
/// once published, so this never needs to change without `ORT_VERSION` changing too.
const NUGET_PACKAGE_SHA256: &str =
    "4b978d5065b85e7004b6c6f60ca494bd978fbe6836cbf0a0b52d82b61ab99638";

/// One target's payload: the path of its shared library inside the NuGet package's zip.
struct Asset {
    payload_in_zip: &'static str,
}

const LINUX_X64: Asset = Asset {
    payload_in_zip: "runtimes/linux-x64/native/libonnxruntime.so",
};

const LINUX_AARCH64: Asset = Asset {
    payload_in_zip: "runtimes/linux-arm64/native/libonnxruntime.so",
};

const OSX_ARM64: Asset = Asset {
    payload_in_zip: "runtimes/osx-arm64/native/libonnxruntime.dylib",
};

fn main() {
    // We emit explicit rerun-if directives below, which disables cargo's default "rerun on
    // any file change in the package" heuristic — so re-add build.rs itself explicitly.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LOCALDB_ORT_LIB");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_LOCAL_ONNX");

    if env::var("CARGO_FEATURE_LOCAL_ONNX").is_err() {
        // local-onnx disabled: nothing to embed. (Other features, e.g. local-coreml, never
        // touch ort/this build script's outputs.)
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
    // The output file name carries the version even though the in-archive name does not:
    // `ort_runtime.rs` names the runtime-extracted cache file after it, so a version bump
    // never collides with a previously extracted library of the same name.
    let (asset, output_filename) = match (target_os.as_str(), target_arch.as_str()) {
        ("linux", "x86_64") => (&LINUX_X64, format!("libonnxruntime.so.{ORT_VERSION}")),
        ("linux", "aarch64") => (&LINUX_AARCH64, format!("libonnxruntime.so.{ORT_VERSION}")),
        ("macos", "aarch64") => (&OSX_ARM64, format!("libonnxruntime.{ORT_VERSION}.dylib")),
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

    // Escape hatch for offline/distro builds: use a caller-provided ONNX Runtime library
    // directly, skipping the download+verify path entirely.
    if let Ok(local_lib) = env::var("LOCALDB_ORT_LIB") {
        let local_path = PathBuf::from(&local_lib);
        if !local_path.is_file() {
            panic!("LOCALDB_ORT_LIB={local_lib} does not point to an existing file");
        }
        let sha256 = sha256_file(&local_path);
        emit_outputs(&local_path, &sha256);
        return;
    }

    let lib_dest = out_dir.join(&output_filename);

    if !lib_dest.is_file() {
        let package_path = ensure_nuget_package(&out_dir);
        extract_payload(&package_path, asset.payload_in_zip, &lib_dest);
        // The package bundles every platform's libraries (~125 MB); only the extracted
        // library is needed from here on, and its presence skips this branch on reruns.
        let _ = fs::remove_file(&package_path);
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
}

fn nuget_package_filename() -> String {
    format!("{NUGET_PACKAGE_ID}.{ORT_VERSION}.nupkg")
}

fn nuget_package_url(filename: &str) -> String {
    format!("https://api.nuget.org/v3-flatcontainer/{NUGET_PACKAGE_ID}/{ORT_VERSION}/{filename}")
}

/// Download the pinned NuGet package into `out_dir` (skipping the download if it's already
/// present with a matching sha256), verify its checksum, and return its path. Fails the
/// build (`panic!`) on checksum mismatch — never silently ship an unverified binary.
fn ensure_nuget_package(out_dir: &Path) -> PathBuf {
    let filename = nuget_package_filename();
    let package_path = out_dir.join(&filename);

    if package_path.is_file() && sha256_file(&package_path) == NUGET_PACKAGE_SHA256 {
        return package_path;
    }

    let url = nuget_package_url(&filename);
    eprintln!("embed/build.rs: downloading {url}");
    let tmp_path = out_dir.join(format!("{filename}.tmp"));
    let actual = download_to_file(&url, &tmp_path);

    if actual != NUGET_PACKAGE_SHA256 {
        let _ = fs::remove_file(&tmp_path);
        panic!(
            "downloaded {url} but its sha256 ({actual}) does not match the pinned value \
             ({NUGET_PACKAGE_SHA256}). Refusing to embed an unverified ONNX Runtime binary. \
             This may mean the pinned constant in embed/build.rs is stale, or the download \
             was corrupted/tampered with — retry, and if it persists, verify the NuGet \
             package manually (https://www.nuget.org/packages/{NUGET_PACKAGE_ID}/{ORT_VERSION})."
        );
    }

    fs::rename(&tmp_path, &package_path).unwrap_or_else(|e| {
        panic!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            package_path.display()
        )
    });
    package_path
}

/// Stream `url`'s response body to `dest`, computing its sha256 as it goes rather than
/// buffering the ~125 MB body in memory, and return the hex-encoded digest.
fn download_to_file(url: &str, dest: &Path) -> String {
    // ureq follows redirects by default (NuGet's flatcontainer API redirects to a CDN),
    // honors HTTPS_PROXY/ALL_PROXY from the environment, and uses rustls for TLS (see
    // Cargo.toml). Certificates are checked against the OS trust store through
    // rustls-platform-verifier, as reqwest does at runtime: ureq's default is its bundled
    // webpki roots, which reject a TLS-intercepting proxy's CA that the host itself trusts.
    // `into_reader` rather than `read_to_vec`, which caps bodies at 10 MB.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .unwrap_or_else(|e| panic!("failed to download {url}: {e}"));

    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(dest)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", dest.display()));

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader
            .read(&mut buf)
            .unwrap_or_else(|e| panic!("failed to read response body from {url}: {e}"));
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", dest.display()));
        hasher.update(&buf[..n]);
    }
    hex::encode(hasher.finalize())
}

/// Extract a single `payload_path` entry from the NuGet package's zip archive at
/// `package_path` to `dest`, atomically (write to a `.tmp` sibling, then rename).
fn extract_payload(package_path: &Path, payload_path: &str, dest: &Path) {
    let file = fs::File::open(package_path)
        .unwrap_or_else(|e| panic!("failed to open {}: {e}", package_path.display()));
    let mut archive = zip::ZipArchive::new(file).unwrap_or_else(|e| {
        panic!(
            "failed to read {} as a zip archive: {e}",
            package_path.display()
        )
    });

    let mut entry = archive.by_name(payload_path).unwrap_or_else(|e| {
        panic!(
            "NuGet package {} did not contain expected payload path {payload_path}: {e}",
            package_path.display()
        )
    });

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
}

fn sha256_file(path: &Path) -> String {
    let data = fs::read(path)
        .unwrap_or_else(|e| panic!("failed to read {} for checksum: {e}", path.display()));
    let mut hasher = Sha256::new();
    hasher.update(&data);
    hex::encode(hasher.finalize())
}
