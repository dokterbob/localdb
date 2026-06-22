# Release engineering

This document captures how the release pipeline works, what each artifact contains, and how to
cut a new release. It reflects the state established during the T12 work.

## Overview

Releases are tag-triggered. Pushing a tag matching `v[0-9]+.[0-9]+.[0-9]*` (e.g. `v0.1.0` or
`v0.1.0-rc1`) runs `.github/workflows/release.yml`, which has three jobs:

```
build-release (matrix: 3 targets)
  → publish-release (softprops/action-gh-release@v2)
  → smoke-test (Linux x86_64)
```

## Release targets

| Platform | Target triple | Runner | Cross? |
|---|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | `macos-latest` | No |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `ubuntu-latest` | No |
| Linux arm64 | `aarch64-unknown-linux-gnu` | `ubuntu-latest` | Yes (`gcc-aarch64-linux-gnu`) |

## Embedding backends per artifact

| Artifact | Backend |
|---|---|
| `aarch64-apple-darwin` | CoreML (ANE/GPU) built in — auto-selected at runtime; falls back to ONNX |
| `x86_64-unknown-linux-gnu` | ONNX CPU only |
| `aarch64-unknown-linux-gnu` | ONNX CPU only |

**How CoreML gets into the macOS binary** — `cli/Cargo.toml` declares a
`[target.'cfg(target_os = "macos")'.dependencies]` block that depends on `embed` with
`features = ["local-coreml"]`. Cargo unions this with the base `local-onnx` feature, so on
macOS `embed` builds with both. The Linux build entries are ignored on macOS and vice versa.
No `--features` flag is needed anywhere — `cargo build -p localdb`, `cargo install --path
localdb`, and the release workflow all pick up CoreML automatically on macOS.

CoreML lives entirely in `embed`; it is gated `cfg(target_os = "macos")` in `embed/Cargo.toml`,
so `objc2`, `block2`, and related crates are never compiled on Linux.

Models are downloaded from HuggingFace at runtime on first use (~706 MB) and cached under
`paths.models`. Nothing is bundled in the binary.

## Native deps and static-linking guarantees

**onnxruntime** is downloaded at build time by fastembed's `ort-download-binaries-rustls-tls`
feature — no system library, no `build.rs` override needed. The resulting binary has no
unexpected external shared-library deps.

The `release.yml` `Verify no unexpected dynamic dependencies` step enforces this:

- **Linux native** (`x86_64`): `ldd` output is filtered to assert only the platform baseline
  (`linux-vdso`, `libgcc_s`, `libc`, `libm`, `libdl`, `libpthread`, `ld-linux`) is linked.
  Cross-compiled `aarch64` is skipped (can't run `ldd` on a foreign-arch binary).
- **macOS**: `otool -L` output asserts only `/usr/lib/`, `/System/Library/`, `@rpath`, or
  `@loader_path` appears. `CoreML.framework`, `Foundation.framework`, and `libobjc.A.dylib`
  all live under `/System/Library/` and pass this check.

## MSRV

| Platform | Minimum Rust version | Reason |
|---|---|---|
| Linux | 1.82 | workspace MSRV |
| macOS | 1.85 | edition-2024 `hf-hub` 1.0 pulled in by CoreML path |

CI uses `dtolnay/rust-toolchain@stable` and the `macos-14` `coreml` job already uses ≥1.85,
so CI is unaffected. Only the source-install instructions note the split MSRV.

## Tarball naming

```
localdb-<GITHUB_REF_NAME>-<target>.tar.gz
```

Examples: `localdb-v0.1.0-aarch64-apple-darwin.tar.gz`,
`localdb-v0.1.0-x86_64-unknown-linux-gnu.tar.gz`.

Each tarball contains `localdb`, `README.md`, and `LICENSE`.

## How to cut a release

1. Bump `version` in `[workspace.package]` in `Cargo.toml` and run
   `cargo build --workspace` to update `Cargo.lock`.
2. Commit: `Bump version to X.Y.Z`.
3. Tag and push:
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
4. The `release.yml` workflow fires automatically. Monitor it in the GitHub Actions tab.
5. Once complete, verify the GitHub Release has three tarballs attached and that the
   smoke-test job passed.

**Pre-release dry-run:** push a tag like `v0.1.0-rc1` — the trigger pattern
`v[0-9]+.[0-9]+.[0-9]*` matches it. Delete the test tag and release afterward:
```bash
git tag -d v0.1.0-rc1
git push origin --delete v0.1.0-rc1
# Delete the GitHub Release via the web UI or: gh release delete v0.1.0-rc1 --yes
```

## Known gaps / future work

- **CUDA**: today the ONNX sessions register no execution providers (CPU-only). Adding a
  `local-cuda` feature in `embed`, registering `CUDAExecutionProvider`, and publishing a
  Linux x86_64 CUDA release artifact is tracked as a separate issue — see the
  "Add and test a CUDA-accelerated build" GitHub issue.
- **Homebrew / launchd / systemd**: deferred to Phase ≥2 per `specs/06-roadmap.md §4`.
- **Linux arm64 smoke test**: the cross-compiled binary is not currently smoke-tested in CI
  (cannot run a foreign-arch binary on the x86_64 runner without QEMU).
