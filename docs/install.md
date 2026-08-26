# Installing localdb

**License:** AGPL-3.0-or-later

## Supported platforms

The release workflow produces binaries for:

| Platform            | Target triple               | Embedding backend                        |
| ------------------- | --------------------------- | ---------------------------------------- |
| macOS Apple Silicon | `aarch64-apple-darwin`      | CoreML (ANE/GPU) built in, ONNX fallback |
| Linux x86_64        | `x86_64-unknown-linux-gnu`  | ONNX CPU                                 |
| Linux arm64         | `aarch64-unknown-linux-gnu` | ONNX CPU                                 |

The macOS binary includes CoreML acceleration automatically — no `--features` flag or config change
is required.

No external dependencies (OpenSSL, etc.) are required. Release binaries are dynamically linked: on
Linux, CI verifies an `ldd` allowlist plus a GLIBC symbol-version ceiling of 2.35, checked on both
the binary and the runtime-extracted ONNX Runtime; on macOS, CI verifies an `otool -L` allowlist
(only `/usr/lib/`, `/System/Library/`, `@rpath`, `@loader_path`). See
[release-engineering.md](release-engineering.md) for pipeline details.

## Install with Homebrew (macOS and Linux)

```bash
brew install dokterbob/localdb/localdb
```

The formula installs a prebuilt binary for your platform plus shell completions (bash/zsh/fish). The
HTTP daemon can optionally run under `brew services` (`launchd` on macOS, `systemd` on Linux):

```bash
brew services start localdb   # runs `localdb serve`, restarts it on failure
brew services stop localdb
```

Every command works daemonless too — the service is opt-in.

## Install with the shell installer

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dokterbob/localdb/releases/latest/download/localdb-installer.sh | sh
```

Downloads the right tarball for your platform, installs into `$CARGO_HOME/bin` (falling back to
`~/.cargo/bin`) and offers to update your `PATH`.

## Install from a pre-built tarball

Once a release is tagged, download the tarball for your platform from the
[releases page](https://github.com/dokterbob/localdb/releases/latest) and extract the binary:

```bash
# Example: macOS Apple Silicon
PLATFORM=aarch64-apple-darwin
curl -L "https://github.com/dokterbob/localdb/releases/latest/download/localdb-${PLATFORM}.tar.xz" \
  | tar -xJ -C /usr/local/bin --strip-components=1 "localdb-${PLATFORM}/localdb"
localdb --version
```

Adjust `PLATFORM` to match your system from the table above.

## Shell completions

Homebrew installs completions automatically. For other install paths, `localdb completions <shell>`
prints the script (bash, zsh, fish, elvish, powershell) — e.g.:

```bash
# zsh (put anywhere on your $fpath)
localdb completions zsh > ~/.zfunc/_localdb
# bash
localdb completions bash >> ~/.bash_completion
```

## A note on embedding models

The default embedder (`pplx-embed-context-v1-0.6b`) is downloaded from the public HuggingFace repo
`perplexity-ai/pplx-embed-context-v1-0.6b` (~706 MB) on the first indexing or search operation
(including `source add`'s auto-index). No API key or license click-through is required. The model is
cached under `paths.models` for subsequent runs.

To fetch it ahead of time instead of on the first `index`/`search`, run
`localdb init --download-model` (see [cli.md](cli.md#localdb-init)).

For details on the embedding pipeline and alternative model options, see
[architecture.md](architecture.md) and
[specs/04-search-pipeline.md](https://github.com/dokterbob/localdb/blob/main/specs/04-search-pipeline.md).

## Build from source

localdb requires **Rust 1.88 or later** on every platform (the `pdf_oxide` PDF parser pulls in
`image` 0.25, which needs 1.88; the macOS CoreML path's edition-2024 `hf-hub` 1.0 floor of 1.85 is
subsumed). The easiest way to install and manage Rust is [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Clone the repository and use `cargo install --path`:

```bash
git clone https://github.com/dokterbob/localdb.git
cd localdb
cargo install --path localdb
```

This places the `localdb` binary in `~/.cargo/bin/`. Make sure that directory is on your `PATH`
(rustup adds it automatically).

Verify the install:

```bash
localdb --version
```

You can also install directly from the git repository without cloning:

```bash
cargo install --git https://github.com/dokterbob/localdb localdb
```

If you're planning to contribute rather than just build a local copy, see
[CONTRIBUTING.md](https://github.com/dokterbob/localdb/blob/main/CONTRIBUTING.md) for the
development setup, coverage gates, and spec-first workflow.

## Next step

Once installed, follow the [Quick Start guide](quickstart.md) to index your first files and run a
search.
