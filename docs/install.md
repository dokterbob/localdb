# Installing localdb

**Version:** 0.1.0 — **License:** AGPL-3.0-or-later

## Prerequisites

localdb requires **Rust 1.82 or later** (Linux) or **Rust 1.85 or later** (macOS, because
CoreML is built automatically and pulls edition-2024 `hf-hub` 1.0). The easiest way to
install and manage Rust is
[rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

No external dependencies (OpenSSL, etc.) are required — the binary is statically linked on
Linux and links only system libraries on macOS.

## Supported platforms

The release workflow produces binaries for:

| Platform | Target triple | Embedding backend |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | CoreML (ANE/GPU) built in, ONNX fallback |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | ONNX CPU, or CUDA on an NVIDIA host (auto-detected, see below) |
| Linux arm64 | `aarch64-unknown-linux-gnu` | ONNX CPU |

The macOS binary includes CoreML acceleration automatically — no `--features` flag or config
change is required. See [release-engineering.md](release-engineering.md) for pipeline details.

## Install from a pre-built tarball

> **Note:** No GitHub release has been tagged yet. Tarballs will be published once a release is
> tagged. Until then, use the `cargo install --path localdb` path described below.

Once a release is tagged, download the tarball for your platform from the
[releases page](https://github.com/dokterbob/localdb/releases/latest) and extract the binary:

```bash
# Example: macOS Apple Silicon
VERSION=0.1.0
PLATFORM=aarch64-apple-darwin
curl -L "https://github.com/dokterbob/localdb/releases/download/v${VERSION}/localdb-v${VERSION}-${PLATFORM}.tar.gz" \
  | tar -xz -C /usr/local/bin --strip-components=1 "localdb-v${VERSION}-${PLATFORM}/localdb"
localdb --version
```

Adjust `PLATFORM` to match your system from the table above.

## Install from source (working path today)

Clone the repository and use `cargo install --path`:

```bash
git clone https://github.com/dokterbob/localdb.git
cd localdb
cargo install --path localdb
```

This places the `localdb` binary in `~/.cargo/bin/`. Make sure that directory is on your
`PATH` (rustup adds it automatically).

Verify the install:

```bash
localdb --version
# localdb 0.1.0
```

You can also install directly from the git repository without cloning:

```bash
cargo install --git https://github.com/dokterbob/localdb localdb
```

## A note on embedding models

`localdb init` prints:

```
Note: embedding models will be downloaded on first index.
```

**This message is accurate.** The default embedder (`pplx-embed-context-v1-0.6b`) is
downloaded from the public HuggingFace repo `perplexity-ai/pplx-embed-context-v1-0.6b`
(~706 MB) the first time `localdb index` or `localdb search` runs. No API key or license
click-through is required. The model is cached under `paths.models` for subsequent runs.

For details on the embedding pipeline and alternative model options, see
[architecture.md](architecture.md) and
[../specs/04-search-pipeline.md](../specs/04-search-pipeline.md).

## First-run downloads

The first `localdb index` or `localdb search` fetches everything the configured embedder needs
and caches it — nothing is bundled in the binary:

| What | Size | When |
|---|---|---|
| Embedding model (`pplx-embed-context-v1-0.6b`, default) | ~706 MB | Always, on first use, any provider. |
| ONNX Runtime, CPU flavor | ~8 MB (Linux x86_64) / ~7 MB (Linux arm64) / ~30 MB (macOS arm64) | `local-onnx`, or `local`/`local-cuda` falling back to CPU. |
| ONNX Runtime, CUDA flavor | ~196 MB | `local-cuda`, or `local` when it detects a usable NVIDIA stack. |

All three are one-time downloads, sha256-verified, and cached indefinitely across upgrades of
`localdb` itself (only a version bump of the pinned model or ONNX Runtime triggers a
re-download). The ONNX Runtime is *not* embedded in the `localdb` binary — see
[architecture.md](architecture.md#platform-notes) for why.

## CUDA (NVIDIA GPU)

No special release artifact is needed for CUDA — the same Linux x86_64 tarball auto-detects an
NVIDIA GPU and downloads the CUDA-enabled ONNX Runtime on demand. Three provider settings
control how eagerly it's used ([specs/03-config.md](../specs/03-config.md) §7):

| `embedding.provider` | Behavior |
|---|---|
| `local` (default) | Automatic: detects the CUDA stack; if present, downloads the CUDA flavor and prefers it, with silent CPU fallback if GPU registration fails. Otherwise plain CPU. |
| `local-cuda` | Forces CUDA. **Hard error, exit code 5**, with an actionable message, if the stack is incomplete or registration fails — no CPU fallback. |
| `local-onnx` | CPU only, always — the opt-out for metered connections or machines that should never attempt the ~196 MB CUDA download. |

**Requirements:** an NVIDIA driver **R525 or newer**, the **CUDA 12.x runtime**
(`libcudart.so.12`), and **cuDNN 9** (`libcudnn.so.9`). cuDNN is the piece most often missing —
it ships as a separate package from both the NVIDIA driver and the CUDA toolkit metapackages,
so a machine with `nvidia-smi` working and `nvcc`/CUDA installed can still fail the stack check
until cuDNN 9 is installed on top. Linux x86_64 only — CUDA is not offered on Linux arm64 or
macOS.

If `local-cuda`'s hard error fires, its message names exactly which piece is missing (driver,
CUDA runtime, or cuDNN) and how to fix it, or suggests falling back to `provider: local`
(automatic, with CPU fallback) or `local-onnx` (CPU only).

## Offline / air-gapped installs

Every download above is a plain HTTPS fetch verified by sha256 against a pinned value — if the
expected file is already present at the expected cache path with a matching hash, no network
call is made at all. This makes pre-seeding an air-gapped machine straightforward: copy the
files below into place (from a machine with network access, or from the release CI cache)
before running `localdb index`.

ONNX Runtime cache paths (pinned to version `1.24.4`):

| OS | CPU flavor directory | CUDA flavor directory |
|---|---|---|
| Linux | `~/.cache/localdb/ort/1.24.4/` (or `$XDG_CACHE_HOME/localdb/ort/1.24.4/`) | `~/.cache/localdb/ort/1.24.4-cuda/` |
| macOS | `~/Library/Caches/localdb/ort/1.24.4/` | n/a (CUDA is Linux x86_64 only) |

File names inside each directory (see `embed/src/ort_download.rs` for the pinned sha256 of
each):

- Linux x86_64 CPU: `libonnxruntime.so.1.24.4`
- Linux arm64 CPU: `libonnxruntime.so.1.24.4`
- macOS arm64 CPU: `libonnxruntime.1.24.4.dylib`
- Linux x86_64 CUDA (all three, in the `-cuda` directory): `libonnxruntime.so.1.24.4`,
  `libonnxruntime_providers_shared.so`, `libonnxruntime_providers_cuda.so`

The embedding model cache under `paths.models` (default: platform cache dir + `localdb/models`)
can be pre-seeded the same way from a HuggingFace mirror or a prior download.

Alternatively, skip the flavor table entirely and point at an already-installed ONNX Runtime
(**>= 1.24** required):

- `ORT_DYLIB_PATH=/path/to/libonnxruntime.so localdb index …` — environment variable, highest
  precedence, no config change needed.
- `embedding.ort_library: /path/to/libonnxruntime.so` in `config.yaml` — persistent,
  packager-friendly equivalent; `ORT_DYLIB_PATH` still wins if both are set.

## Next step

Once installed, follow the [Quick Start guide](quickstart.md) to index your first files and
run a search.
