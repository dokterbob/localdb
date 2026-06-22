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
| Linux x86_64 | `x86_64-unknown-linux-gnu` | ONNX CPU |
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

## Next step

Once installed, follow the [Quick Start guide](quickstart.md) to index your first files and
run a search.
