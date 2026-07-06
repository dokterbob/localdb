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
| `aarch64-apple-darwin` | CoreML (ANE/GPU) built in — auto-selected at runtime; falls back to ONNX (CPU) |
| `x86_64-unknown-linux-gnu` | ONNX (CPU or, on an NVIDIA host, CUDA) — see below |
| `aarch64-unknown-linux-gnu` | ONNX CPU only |

**How CoreML gets into the macOS binary** — `cli/Cargo.toml` declares a
`[target.'cfg(target_os = "macos")'.dependencies]` block that depends on `embed` with
`features = ["local-coreml"]`. Cargo unions this with the base `local-onnx` feature, so on
macOS `embed` builds with both. The Linux build entries are ignored on macOS and vice versa.
No `--features` flag is needed anywhere — `cargo build -p localdb`, `cargo install --path
localdb`, and the release workflow all pick up CoreML automatically on macOS.

CoreML lives entirely in `embed`; it is gated `cfg(target_os = "macos")` in `embed/Cargo.toml`,
so `objc2`, `block2`, and related crates are never compiled on Linux.

**CUDA needs no separate artifact (issue #76).** The single `x86_64-unknown-linux-gnu` tarball
serves both CPU-only and NVIDIA-GPU machines: `embedding.provider: local` auto-detects an NVIDIA
driver + CUDA 12.x runtime + cuDNN 9 stack at run time and, if found, downloads the
CUDA-flavored ONNX Runtime and prefers the CUDA execution provider (silent CPU fallback on
failure); `local-cuda` forces it (hard error, exit 5, if the stack is unavailable); `local-onnx`
opts out of CUDA entirely. There is no `-cuda` release target, no separate CUDA tarball, and no
`--features` flag — see [specs/03-config.md](../specs/03-config.md) §7 and
[architecture.md](architecture.md)'s "ONNX Runtime loading" section for the flavor table and
detection ladder.

Models are downloaded from HuggingFace at runtime on first use (~706 MB) and cached under
`paths.models`. Nothing is bundled in the binary.

## Native deps and static-linking guarantees

**ONNX Runtime is no longer embedded in the binary at all (issue #76).** Previously
(pre-#76) it was downloaded at *build* time and baked into the `embed` crate via
`include_bytes!`. `embed/build.rs` is deleted; instead `embed::ort_download` downloads
Microsoft's official ONNX Runtime release (sha256-pinned, per-target "flavor table") the first
time it's needed at *run* time, and `embed::ort_runtime` `dlopen`s it (`ort`'s `load-dynamic`
feature — the executable itself links no ONNX Runtime ABI). Because the runtime is no longer
embedded, **release artifacts shrink by roughly the size the previous embedded library added
per platform (~12–20 MB, see `architecture.md`'s prior measurement)** — the CPU flavor now
downloads at first use instead of shipping in every tarball. `ort`'s `download-binaries` /
`api-*` default features, and `ort/cuda`, are still never enabled — see
[architecture.md](architecture.md#platform-notes) for why. The resulting binary has no
unexpected external shared-library deps.

The `release.yml` `Verify no unexpected dynamic dependencies` step enforces this:

- **Linux native** (`x86_64`): `ldd` output is filtered to assert only the platform baseline
  (`linux-vdso`, `libgcc_s`, `libc`, `libm`, `libdl`, `libpthread`, `ld-linux`) is linked.
  Cross-compiled `aarch64` is skipped (can't run `ldd` on a foreign-arch binary).
- **macOS**: `otool -L` output asserts only `/usr/lib/`, `/System/Library/`, `@rpath`, or
  `@loader_path` appears. `CoreML.framework`, `Foundation.framework`, and `libobjc.A.dylib`
  all live under `/System/Library/` and pass this check.

**glibc-floor guarantee moved to CI (issue #76).** Before #76, `release.yml`'s "Verify glibc
floor" step `objdump`-checked the *embedded* ONNX Runtime `.so` (baked into the binary at build
time) alongside the `localdb` binary itself, to guarantee both stayed within Ubuntu 22.04's
`GLIBC_2.35` floor (issue #133). Since the runtime is now downloaded at run time instead of
embedded, that half of the check moved to `.github/workflows/ci.yml`'s `ort-download` job:
it downloads and `objdump`-checks the real CPU and CUDA flavors on a GPU-less `ubuntu-22.04`
runner, on every PR, rather than only at release time. `release.yml`'s own "Verify glibc floor"
step still runs, scoped to just the `localdb` binary — our own Rust code still inherits the
*build machine's* glibc floor independent of the ONNX Runtime mechanism, which is why Linux
builds stay pinned to `ubuntu-22.04` here too.

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

## Verifying CUDA

CI's `ort-download` job proves the CUDA-flavored ONNX Runtime downloads, verifies, and fails
*cleanly* without a GPU — it cannot exercise a real execution-provider registration or actual
inference, since GitHub-hosted runners have no NVIDIA hardware. `scripts/cuda-verify.sh` is a
one-shot manual verification kit for someone with an NVIDIA machine to run against a published
release tarball:

```bash
./scripts/cuda-verify.sh <release-tarball-url>
# e.g. https://github.com/dokterbob/localdb/releases/download/vX.Y.Z/localdb-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz

./scripts/cuda-verify.sh --dry-run   # no GPU/network required; exercises only the
                                      # environment-evidence step, for testing the script itself
```

No repo checkout is required for a real run — only the tarball URL. The script downloads and
extracts the tarball into an isolated scratch workdir (redirecting `XDG_CACHE_HOME` /
`XDG_DATA_HOME` / `XDG_CONFIG_HOME` / `XDG_STATE_HOME` / `HOME` so it never touches the
operator's real caches or config), then:

1. Collects environment evidence (`nvidia-smi`, `ldconfig -p` for `libcudnn.so.9` /
   `libcudart.so.12`, `uname -a`, glibc version).
2. Runs `localdb index` + 3 `localdb search` queries against 10 fixture documents with
   `embedding.provider: local-cuda` (forced CUDA), asserting exit 0 and that the CUDA-flavored
   ONNX Runtime download/commit was logged.
3. Repeats the same fixtures/queries with `provider: local-onnx` (CPU reference).
4. **Parity check**: compares the top-5 `chunk_id`s per query between the CUDA and CPU runs —
   requires the top-1 result identical and ≥4/5 overlap — validating that the two execution
   providers are index-interchangeable on real hardware (the claim in
   [specs/03-config.md](../specs/03-config.md) §7).
5. Runs once more with `provider: local` (automatic mode), asserting the factory's log line
   shows it chose CUDA on its own.
6. Writes `./cuda-verify-report.txt` (PASS/FAIL/SKIP per step plus evidence) in the current
   working directory, and prints a final hardware/date/PASS-FAIL summary.

Expect roughly **~900 MB of downloads** on the GPU box: the release tarball, the
`pplx-embed-context-v1-0.6b` embedding model (~706 MB, one-time, cached), and the CUDA-flavored
ONNX Runtime (~196 MB, one-time, cached). Paste the resulting `cuda-verify-report.txt` back for
review; once a run passes, update the known-gap entry below (and
[architecture.md](architecture.md#known-gaps)'s matching entry) with "verified on `<hardware>`,
`<date>`".

## Known gaps / future work

- **CUDA execution has not been verified on real NVIDIA hardware.** The CUDA execution
  provider, `local-cuda`, and `local` mode's automatic CUDA preference are implemented
  (issue #76: `embed/src/cuda_ep.rs`, `embed/src/ort_download.rs`, `embed/src/factory.rs`) and
  covered by CI on GPU-less runners, but no one has yet run `scripts/cuda-verify.sh` (see
  above) against a real GPU. Treat GPU execution and CPU/CUDA output parity as unverified until
  that run happens and this entry is updated with the result.
- **Homebrew / launchd / systemd**: deferred to Phase ≥2 per `specs/06-roadmap.md §4`.
- **Linux arm64 smoke test**: the cross-compiled binary is not currently smoke-tested in CI
  (cannot run a foreign-arch binary on the x86_64 runner without QEMU).
