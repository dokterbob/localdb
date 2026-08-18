# Release engineering

This document captures how the release pipeline works, what each artifact contains, and how to cut a
new release.

## Overview

The pipeline has two halves:

1. **release-plz** (`.github/workflows/release-plz.yml`, config `release-plz.toml`) maintains a
   rolling **release PR** on every push to `main`: it bumps `[workspace.package].version` (all
   crates inherit it — never hand-bump), rewrites the internal `=X.Y.Z` dependency pins, and updates
   `CHANGELOG.md` via `cliff.toml` (Common Changelog style). **Merging that PR cuts the release**:
   the same workflow then pushes the bare `vX.Y.Z` tag.
2. **dist / cargo-dist** (`.github/workflows/release.yml`, config `dist-workspace.toml`) fires on
   that tag: builds the three target tarballs plus the shell installer, uploads checksums and GitHub
   build-provenance attestations, creates the GitHub Release (body from the version's `CHANGELOG.md`
   section), and runs our custom jobs.

```
push to main ─→ release-plz-pr  (maintains rolling release PR)
merge release PR ─→ release-plz-release  (pushes tag vX.Y.Z)
tag vX.Y.Z ─→ release.yml (dist):
    plan ─→ build-local-artifacts (3 runners) ─→ build-global-artifacts
         ─→ custom-release-checks   (release-checks.yml — artifact gate)
         ─→ host                    (uploads artifacts, creates GitHub Release)
         ─→ custom-homebrew-tap-publish  (needs host + checks — pushes tap formula)
         ─→ announce ─→ custom-smoke-test (smoke-test.yml — post-announce, informational)
```

`release.yml` is **generated** by `dist generate` from `dist-workspace.toml` — never hand-edit it;
edit the config and regenerate (`dist generate --check` guards drift). The custom jobs live in
hand-maintained `workflow_call` workflows:

- `release-checks.yml` — dynamic-dependency allowlist (`ldd`/`otool -L`), GLIBC ≤ 2.35 floor on the
  binary **and** the embedded ONNX Runtime (extracted at runtime by a real tiny index run), and a
  `--version` smoke. Gates the tap publish. (dist 0.32 offers no pre-`host` hook, so the GitHub
  Release itself is created regardless — a failed check means: fix, delete the release/tag, re-tag.)
- `homebrew-tap-publish.yml` — renders `homebrew/localdb.rb.erb` via `homebrew/render.rb` from
  dist's final `dist-manifest.json` (URLs/sha256s verbatim) and pushes `Formula/localdb.rb` to
  `dokterbob/homebrew-localdb`. Single writer of the tap. Needs the `HOMEBREW_TAP_TOKEN` secret
  (fine-grained PAT, `contents: write` on the tap).
- `smoke-test.yml` — `smoke_test.sh` (install → init → index → search) on ubuntu-22.04 (the
  glibc-2.35 floor, ≈ Mint 21.x), ubuntu-latest and macos-latest. Post-announce, informational.

## Version and changelog policy

- `[workspace.package].version` is **release-plz-owned**. All 10 crates use
  `version.workspace = true`; internal path deps carry a `version = "X.Y.Z"` requirement
  (`cargo package` demands one), which release-plz keeps in sync on each bump.
- **Package names are namespaced** (`localdb-extract`, `localdb-embed`, …) while each crate's
  `[lib] name` stays short, so imports are unprefixed (`use extract::…`) but nothing in the
  workspace can ever be resolved against an unrelated crates.io package — the short names
  (`extract`, `fetch`, `embed`, …) are all taken by strangers there, and release-plz's `git_only`
  change detection runs `cargo package`, which resolves internal deps through an overlay registry
  that only shadows crates.io at the _same_ name and version. The `localdb-*` names are currently
  unclaimed on crates.io; the plan is to claim them once the project matures. Until then a squatter
  publishing a higher-versioned `localdb-*` crate could confuse that overlay resolution — if that
  ever becomes a live concern before the names are claimed, tightening the internal requirements to
  exact `=X.Y.Z` pins closes it.
- `release-plz.toml` uses the single-tag scheme: every package shares
  `git_tag_name = "v{{ version }}"` (that template is how `git_only` mode finds each package's last
  released version), but only `localdb` creates the tag.
- `CHANGELOG.md` follows [Common Changelog](https://common-changelog.org), rendered by `cliff.toml`
  (shared by release-plz and the `git-cliff` CLI). Commits are grouped by first-word heuristics
  (Add/Fix/Remove/…); docs/chore/ci/test commits are skipped. The release PR is hand-editable —
  curate the generated section before merging.
- Version bumps are patch-level by default (the repo does not use conventional commits, so
  release-plz cannot infer minor/major). Edit the version in the release PR for a bigger bump.

## Release targets

| Platform            | Target triple               | Runner             | Notes                  |
| ------------------- | --------------------------- | ------------------ | ---------------------- |
| macOS Apple Silicon | `aarch64-apple-darwin`      | `macos-14`         | CoreML built in        |
| Linux x86_64        | `x86_64-unknown-linux-gnu`  | `ubuntu-22.04`     | glibc-2.35 floor       |
| Linux arm64         | `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | native build, no cross |

Runner pinning lives in `[dist.github-custom-runners]` in `dist-workspace.toml`. The ubuntu-22.04
pins are the glibc-floor mechanism (issue #133): our own Rust code inherits the build machine's
glibc floor. Pinning also keeps dist off its default zigbuild cross path for aarch64.

Artifacts are built with the `dist` cargo profile (`[profile.dist]`: release + `strip = true`).

## Embedding backends per artifact

| Artifact                    | Backend                                                                  |
| --------------------------- | ------------------------------------------------------------------------ |
| `aarch64-apple-darwin`      | CoreML (ANE/GPU) built in — auto-selected at runtime; falls back to ONNX |
| `x86_64-unknown-linux-gnu`  | ONNX CPU only                                                            |
| `aarch64-unknown-linux-gnu` | ONNX CPU only                                                            |

**How CoreML gets into the macOS binary** — `cli/Cargo.toml` declares a
`[target.'cfg(target_os = "macos")'.dependencies]` block that depends on `embed` with
`features = ["local-coreml"]`. Cargo unions this with the base `local-onnx` feature, so on macOS
`embed` builds with both. No `--features` flag is needed anywhere.

Models are downloaded from HuggingFace at runtime on first use (~706 MB for the default model) and
cached under `paths.models`. Nothing is bundled in the binary.

## Native deps and static-linking guarantees

**ONNX Runtime is embedded, never system-provided** (issue #133): `embed/build.rs` downloads and
sha256-verifies Microsoft's official build at compile time and embeds it; the binary extracts it to
the user cache dir on first use (`embed::ort_runtime`). The Homebrew formula deliberately has **no**
`depends_on "onnxruntime"` — brew's onnxruntime bumps every few weeks while `ort` v2 pins 1.24.x,
and Ubuntu 24.04 has no apt package, so a system-lib approach would fork the build per channel and
accept untested version skew. Embedded keeps one artifact and one tested combo everywhere.

`release-checks.yml` enforces the guarantees on the built tarballs:

- **Linux (both arches, natively)**: `ldd` allowlist (platform baseline only), `objdump -T` GLIBC ≤
  2.35 on the binary and the runtime-extracted `libonnxruntime`.
- **macOS**: `otool -L` allowlist — only `/usr/lib/`, `/System/Library/`, `@rpath`, `@loader_path`
  (CoreML/Foundation frameworks live under `/System/Library/` and pass).
- `--version` must report a commit SHA (vergen; `unknown` would mean a git-less pipeline build).

## Install channels

- **Homebrew tap**: `brew install dokterbob/localdb/localdb` — prebuilt-binary formula with shell
  completions (`generate_completions_from_executable`, backed by `localdb completions <shell>`) and
  an opt-in `brew services start localdb` daemon (`service do`). A from-source formula is off the
  table while `embed/build.rs` downloads ONNX Runtime at build time (Homebrew builds are
  network-free), and homebrew-core is out of reach at current notability.
- **Shell installer**: `curl ... | sh` one-liner generated by dist, linked from each GitHub Release.
- **Tarballs**: three per release, each containing `localdb`, `README.md`, `LICENSE`, plus
  `sha256.sum` checksums and GitHub build-provenance attestations
  (`gh attestation verify <file> --repo dokterbob/localdb`).

## MSRV

The workspace MSRV is **Rust 1.88** on every platform, declared as `rust-version` in the root
`Cargo.toml` (floor set by `image` 0.25 via the `pdf_oxide` PDF parser). CI and the release pipeline
use current stable.

## How to cut a release

1. Merge the rolling **release PR** that release-plz maintains (curate its changelog section and, if
   the default patch bump is wrong, edit the version in the PR first).
2. That's it. The merge triggers the tag push; the tag triggers dist. Monitor the `Release` run in
   the Actions tab: 3 tarballs + shell installer + checksums + attestations on the GitHub Release,
   tap formula updated, smoke test green.

**Do not** hand-bump the version or hand-push `vX.Y.Z` tags outside this flow (a hand-pushed tag
does work — dist only needs the tag — but the changelog and version pins won't have been updated).

## Operational notes

- release-plz's `git_only` change detection runs `cargo package --workspace` (with verification
  builds) in a temp worktree on every `release-plz` run — this is a full workspace build; the
  workflow warms it with `Swatinem/rust-cache` and a fixed `CARGO_TARGET_DIR`.
- `RELEASE_PLZ_TOKEN` must be a PAT: tags pushed with the default `GITHUB_TOKEN` do not trigger
  `release.yml`.
- Legacy `v0.1.0preN` tags predate this pipeline; both `cliff.toml`'s `tag_pattern` and
  release-plz's tag template ignore them.

## Known gaps / future work

- **CUDA**: today the ONNX sessions register no execution providers (CPU-only). A `local-cuda`
  feature and CUDA release artifact are tracked separately.
- **launchd / systemd units outside brew**: `brew services` covers Homebrew installs; bare-tarball
  installs still have no unit files. See `specs/06-roadmap.md §4`.
- **Windows**: no target yet.
