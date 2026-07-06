# localdb — Contributor Architecture Guide

> Version 0.1.0 · AGPL-3.0-or-later · github.com/dokterbob/localdb

This document orients new contributors: crate boundaries, data flow, process model, on-disk
layout, and a frank account of what is not yet wired. For design rationale and decisions
behind each choice, follow the links into the `specs/` tree — that is the authority; this
document is the behavior layer on top of it.

---

## Crate map

The workspace is a single Cargo workspace with one binary (`localdb`) built from eight crates.
No retrieval, indexing, or domain logic lives in a surface crate — all surfaces share one core
(see [specs/01-architecture.md](../specs/01-architecture.md) §1).

### `core`

The domain model and shared logic. Defines every entity (`Store`, `Source`, `Document`,
`Chunk`, `IndexJob`, `Citation`), the two key traits (`RetrievalStore` and
`Embedder`), content-addressed ID derivation (blake3), the RRF fusion engine, indexing
orchestration, and the error taxonomy. Contains no I/O framework; everything async-capable
lives in other crates. This is the crate everything else imports.

### `extract`

Format detection and extraction. Accepts raw bytes and returns a normalized Markdown string
plus `DocumentMetadata` extracted from frontmatter (Dublin Core fields). Supported in v1:
Markdown (pulldown-cmark), plain text, HTML (readability-style), and text-layer PDF. Binary
files and non-UTF-8 content are declined gracefully. Unsupported or unreadable files are
counted as skipped/errored in `IndexJob` stats, never fatal. See
[specs/04-search-pipeline.md](../specs/04-search-pipeline.md) §2.

### `embed`

`Embedder` implementations. Declares providers for local ONNX inference
(`OnnxEmbedder`, feature-gated `local-onnx`), local CoreML inference on Apple Silicon
(feature-gated `local-coreml`, macOS-only), OpenAI-compatible flat HTTP endpoints
(`OpenAiEmbedder`), Perplexity contextualized embeddings (`PerplexityEmbedder`), and Voyage
(`VoyageEmbedder`). All implement the document-aware `Embedder` trait from `core`, which
groups chunks by document so contextualized/late-chunking models can use the surrounding
document as context. The CLI wires the embedder via `embed::create_embedder` from the config
policy; the default is `local` / `pplx-embed-context-v1-0.6b`. The `local` provider auto-selects
the CoreML (ANE/GPU) backend on Apple Silicon macOS when built with `--features local-coreml`,
falling back to ONNX (CPU) otherwise; `local-coreml` / `local-onnx` force a backend. The two
backends emit index-interchangeable vectors. See
[specs/04-search-pipeline.md](../specs/04-search-pipeline.md) §4 and the
[Platform notes](#platform-notes) below.

### `store-libsql`

The `RetrievalStore` trait implementation backed by libsql (DiskANN vectors + FTS5 BM25). A single unified database file at `<data_dir>/localdb.db` holds everything. BM25 full-text search uses SQLite's FTS5 virtual table. Dense search uses the DiskANN vector index (`libsql_vector_idx`). RRF fusion is done in `core`. See [specs/01-architecture.md](../specs/01-architecture.md) §2.

### `cli`

Command implementations. A thin layer on `core` and the daemon client; no business logic.
Each command handler acquires config and runtime state, probes the daemon socket, then either
delegates to the HTTP API (thin-client mode) or opens the store in-process (embedded mode).
Calls `embed::create_embedder` from the config policy to obtain the embedder for `index` and
`search`; `FakeEmbedder` is used only in unit tests.

### `server`

The axum-based HTTP API daemon. Exposes the `/v1` REST surface, manages the daemon unix
socket for discovery, runs the file-watcher (`notify`), the URL refresh scheduler, and the
background job queue. Opens the same unified database (`<data_dir>/localdb.db`) as the CLI;
CLI-indexed data is visible. Multi-process is the first-class concurrency model — the daemon
is one writer among peers (CLI sessions, multiple stdio MCP servers); concurrent writers
serialise via SQLite WAL + `busy_timeout=5000`. Ingestion via `POST /v1/jobs` is currently a
no-op — see [Known gaps §1](#known-gaps). See [specs/05-surfaces.md](../specs/05-surfaces.md) §3.

### `mcp`

Stdio MCP server (JSON-RPC 2.0, newline-delimited). Exposes three read-only tools —
`search`, `get_document`, `list_stores` — and speaks the same `Citation` shape that every
other surface uses. Fully functional in embedded mode (opens stores in-process). The
`--allow-write` flag is parsed for forward compatibility but write tools are rejected in v1.
See [specs/05-surfaces.md](../specs/05-surfaces.md) §4.

### `localdb` (binary)

The single-binary entry point. Parses the top-level subcommand tree with clap and delegates
to the appropriate crate. No logic of its own. Subcommands: `init`, `serve`, `mcp`, `status`,
`store`, `source`, `index`, `search`.

---

## Data flow

```
 ┌─────────────────────────────────────────────────────────┐
 │                     WRITE PATH                          │
 │                                                         │
 │  path / URL source                                      │
 │       │                                                 │
 │       ▼                                                 │
 │  extract  →  normalized Markdown + DocumentMetadata      │
 │       │                                                 │
 │       ▼                                                 │
 │  chunker  →  Chunks  (heading-aware, ~400-token prose)  │
 │       │                                                 │
 │       ▼                                                 │
 │  Embedder  →  dense vectors  [default: local; CoreML/ONNX]│
 │       │                                                 │
 │       ▼                                                 │
 │  store-libsql  →  localdb.db  (BM25 index + vectors)    │
 └─────────────────────────────────────────────────────────┘

 ┌─────────────────────────────────────────────────────────┐
 │                     READ PATH                           │
 │                                                         │
 │  query string                                           │
 │       │                                                 │
 │       ├──────────────────────────────────┐              │
 │       ▼                                  ▼              │
 │  BM25 search (FTS5)               dense search (KNN)    │
 │       │                                  │              │
 │       └──────────────┬───────────────────┘              │
 │                      ▼                                  │
 │               RRF fusion (k=60, in core)                │
 │                      │                                  │
 │                      ▼                                  │
 │         top-N Citations  (fused + per-leg scores)       │
 └─────────────────────────────────────────────────────────┘
```

Content-addressed IDs (`blake3`) flow through every step: documents get
`blake3(uri ‖ content_hash)` and chunks get `blake3(document_id ‖ chunk_text ‖ span)`,
making re-indexing idempotent. See [specs/02-domain-model.md](../specs/02-domain-model.md) §3.

The `Citation` is the canonical output shape used by every surface — CLI, HTTP, and MCP all
return the same structure. See [specs/02-domain-model.md](../specs/02-domain-model.md) §6.

---

## Process model

```
  localdb search / localdb mcp
         │
         ▼
  probe <data_dir>/daemon.sock
         │
    ┌────┴────────────────┐
    │ socket present       │ socket absent
    │ and responsive       │ (or missing)
    ▼                      ▼
  thin client          embedded mode
  (HTTP to daemon)     open store in-process
```

On every invocation, CLI and MCP probe a unix socket at `<data_dir>/daemon.sock`. If a
daemon is running and responsive, the command routes over HTTP. If not, the store is opened
in-process (libsql database; embeddings come from the configured embedder, defaulting to the
local ONNX model). No configuration is needed for the common case. See [specs/01-architecture.md](../specs/01-architecture.md) §3.

---

## On-disk layout

The config file and the data directory are independent paths (`--config` /
`LOCALDB_CONFIG` choose the former; `paths.data` the latter). After
`localdb init` and `localdb index`:

```
<config_dir>/
  config.yaml                  # YAML config (version: 1)

<data_dir>/
  localdb.db                   # SQLite (WAL): unified database
  localdb.db-wal               # WAL sidecar (libsql managed)
  localdb.db-shm               # shared-memory sidecar (libsql managed)
  daemon.sock                  # unix socket (present only while daemon runs)
```

The default `data_dir` on macOS is `~/Library/Application Support/com.localdb.localdb.localdb/data`
(the bundle ID is intentionally verbose — see [Known gaps §4](#known-gaps)).
Override with `paths.data` in `config.yaml` or point to a custom config with `--config`.

The `models/` directory (configured via `paths.models`) is populated on first `localdb index`
or `localdb search` when the default `local` embedder downloads `pplx-embed-context-v1-0.6b`
(~706 MB ONNX) from HuggingFace. On Apple Silicon macOS built with `--features local-coreml`,
the CoreML bundle is additionally fetched from `dokterbob/pplx-embed-coreml` (XET-deduped via
`hf-hub` 1.0). Subsequent runs use the cached model.

`--features local-onnx` builds (the default on Linux; the ONNX fallback on macOS) additionally
populate `<cache_dir>/localdb/ort/<version>/` (or `<version>-cuda/` for the CUDA flavor) on
first use with a downloaded ONNX Runtime shared library — a separate, sibling directory to
`models/`, not configurable via `paths.*`. See [Platform notes: ONNX Runtime loading](#platform-notes).

---

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | OK |
| 1 | Internal error |
| 2 | Invalid usage or config (clap errors, config parse failures) |
| 3 | Not found (unknown store, unknown source) |
| 4 | Conflict / already running (duplicate store, second daemon) |
| 5 | Unavailable (daemon unreachable, model missing) |

---

## Platform notes {#platform-notes}

**CoreML embedding backend (macOS / Apple Silicon).** The default `pplx-embed-context-v1-0.6b`
model can run on Apple's ANE/GPU via a CoreML backend in `embed`, behind the opt-in
`local-coreml` cargo feature (macOS-only; every code path is
`#[cfg(all(target_os = "macos", feature = "local-coreml"))]`). Build it with
`cargo build -p localdb --features local-coreml`. Because the feature pulls edition-2024
dependencies (`hf-hub` 1.0), it requires **Rust ≥ 1.85**; the workspace `rust-version` is `1.85`.
Default builds (feature off) are unaffected and remain ONNX-only — Linux and CI default builds
never touch any CoreML code.

The default `local` provider auto-selects CoreML on Apple Silicon when the feature is built and
the bundle loads, otherwise falls back to ONNX (CPU). `local-coreml` forces CoreML (hard error if
unavailable); `local-onnx` forces ONNX. CoreML and ONNX vectors are index-interchangeable
(same `model_id`, 1024-dim, `Binary`; measured ~0.995–0.9995 cosine parity, ~98–99% per-dimension
sign agreement), so switching backends needs no reindex. See [specs/03-config.md](../specs/03-config.md) §7 and
[specs/04-search-pipeline.md](../specs/04-search-pipeline.md) §4.

**ONNX Runtime loading (`local-onnx`, all platforms; issue #76).** `embed`'s `ort` dependency
uses the `load-dynamic` feature — the `localdb` executable links no ONNX Runtime ABI at all and
instead `dlopen`s a shared library at a path chosen at runtime. Historically (pre-#76) that
library was downloaded *at build time* and baked into the binary via `include_bytes!`
(`embed/build.rs`). `embed/build.rs` is now **deleted**: instead, `embed::ort_download` holds a
sha256-pinned "flavor table" of Microsoft's official ONNX Runtime release tarballs, and
`embed::ort_runtime::ensure_ort_initialized` downloads (or reuses an already-verified cached
copy of) the process's chosen flavor the first time any local-ONNX embedder is constructed,
before calling `ort::init_from` on it — before any other `ort` API is touched. One release
binary now serves every target the flavor table knows about, instead of a fixed CPU library
baked in regardless of what the running machine needs.

The flavor table (all pinned to ONNX Runtime 1.24.4) has four entries:

| Flavor | Target | Approx. download | Cache subdirectory |
|---|---|---|---|
| CPU | `linux-x64` | ~8 MB | `<cache_dir>/localdb/ort/1.24.4/` |
| CPU | `linux-aarch64` | ~7 MB | `<cache_dir>/localdb/ort/1.24.4/` |
| CPU | `osx-arm64` | ~30 MB | `<cache_dir>/localdb/ort/1.24.4/` |
| CUDA | `linux-x64` only | ~196 MB | `<cache_dir>/localdb/ort/1.24.4-cuda/` |

The CUDA flavor bundles three libraries (the core runtime, the shared provider bridge, and
`libonnxruntime_providers_cuda.so`) and deliberately excludes the TensorRT provider. CPU and
CUDA flavors live in sibling `ort/<version>/` and `ort/<version>-cuda/` cache directories so
they never collide, and a process commits to exactly one flavor for its lifetime (`ort`'s
`init_from`/`.commit()` can only meaningfully run once) — see `embed/src/ort_runtime.rs`'s
module docs for the "once-only" state machine, including why a failed CUDA *download* must not
block a subsequent CPU fallback attempt in the same process (automatic `local` mode's behavior).
The CPU flavor's cache directory and file names are byte-identical to the old build-time-embedded
extraction path, so **existing user caches from before issue #76 remain valid pre-seeds** — no
migration, no re-download.

**Override precedence**, honoured regardless of flavor, in priority order:
1. `ORT_DYLIB_PATH` (runtime env var) — existing power-user / system-package escape hatch.
2. `embedding.ort_library` (config key, see [specs/03-config.md](../specs/03-config.md) §1) —
   the packager-facing equivalent, threaded through by the embedder factory.
3. Runtime download of the requested flavor's pinned payloads (the default).

A caller supplying either override is assumed to know what they are doing (e.g. pointing at a
distro package or a GPU build the flavor table doesn't know about) — overrides bypass
flavor-specific download logic entirely. Both overrides require ONNX Runtime **≥ 1.24**:
`fastembed`'s own (unconditional) `ort` dependency declaration requests the `api-24` feature
regardless of which `fastembed` features we enable, so Cargo feature unification enables
`api-24` for the whole build even though `embed`'s own `ort` dependency line specifies no
`api-*` feature. This is why the pinned flavor-table version is exactly 1.24.4, not an older
1.17–1.23 release.

**Offline / air-gapped installs.** Since `ensure_downloaded` treats "payloads already present
with matching sha256" as success without touching the network, pre-seeding a machine is just
placing the expected files at the expected cache path before first use (exact paths and file
names: [docs/install.md](install.md)). This is the same mechanism that makes existing pre-#76
CPU caches valid without migration.

**Why no silent system-ORT scanning.** `embed` never probes `/usr/lib`, `ldconfig`, or similar
for a pre-installed ONNX Runtime and quietly adopts it: the flavor table requires ONNX Runtime
**≥ 1.24** (via the `api-24` feature unification above), and a system package's exact version,
build flavor (CPU vs. CUDA-enabled), and ABI are unknowable without asking the user — silently
picking the wrong one would misattribute a load failure or, worse, a wrong-execution-provider
success to the wrong cause. `ORT_DYLIB_PATH` / `embedding.ort_library` exist precisely so an
operator who *does* have a suitable system library can point at it explicitly instead.

**Why not `ort`'s `download-binaries` feature.** That feature (and any `api-*` default feature)
statically links pyke.io's prebuilt ONNX Runtime archive into `ort-sys`. That archive is built
with GCC 14 on Ubuntu 24.04 and references `__isoc23_strtol*` symbols, giving the *release binary
itself* a `GLIBC_2.38` floor — it refused to start on glibc-2.35 distros (Linux Mint 21.x, Ubuntu
22.04). It was also ABI-incompatible with GCC-11 libstdc++ when built on ubuntu-22.04. See
[issue #133](https://github.com/dokterbob/localdb/issues/133) and
[pykeio/ort#523](https://github.com/pykeio/ort/issues/523) (unresolved upstream) — this
constraint predates and is independent of issue #76's move to runtime download, and applies
identically to `ort/cuda` (the CUDA support here dlopens Microsoft's official CUDA execution
provider library ourselves; it never links `ort`'s own CUDA bindings). The Microsoft official
Linux builds this flavor table downloads instead float at `GLIBC_2.27` / `GLIBCXX_3.4.22` /
`CXXABI_1.3.11` (verified via `objdump -T`), comfortably under Ubuntu 22.04's `GLIBC_2.35`
baseline; the macOS dylib declares a minimum of macOS 14.0 (`LC_BUILD_VERSION`). Because our own
Rust code still inherits the *build machine's* glibc floor independent of this mechanism, the
release workflow still pins Linux builds to `ubuntu-22.04` (not `ubuntu-latest`); the
glibc/libstdc++ floor check against the *downloaded* ONNX Runtime libraries (formerly an
objdump step in the release workflow against the embedded `.so`) now lives in `ci.yml`'s
`ort-download` job, which runs on every PR against real downloaded artifacts on a GPU-less
`ubuntu-22.04` runner.

**Migration note.** `LOCALDB_ORT_LIB` (the build-time env var `embed/build.rs` used to read, to
embed a local ONNX Runtime library instead of downloading one for an offline/distro build) no
longer exists — `embed/build.rs` itself is deleted. The equivalent today is either
`ORT_DYLIB_PATH` (env, for a single run) or `embedding.ort_library` (config, persistent) pointed
at an already-installed library, or pre-seeding the download cache as described above.

**CUDA execution provider (Linux x86_64, issue #76).** `embed::cuda_ep` implements a cheap,
file-level detection ladder before ever attempting a CUDA download: (1) an NVIDIA driver
(`/proc/driver/nvidia/version`, `/dev/nvidiactl`, or `ldconfig -p` listing `libcuda.so.1`), then
(2) the CUDA 12.x runtime (`libcudart.so.12` via `ldconfig -p`), then (3) cuDNN 9
(`libcudnn.so.9` via `ldconfig -p` — the piece most commonly missing, since it ships separately
from both the driver and the CUDA toolkit metapackages). A stack that passes all three rungs
still goes through a ground-truth `ort` execution-provider registration probe
(`embed::cuda_ep::probe_cuda`) before being trusted. Three provider values consume this:
`local-onnx` never attempts CUDA; `local` (automatic) prefers CUDA when the ladder passes but
silently falls back to CPU on any failure; `local-cuda` requires CUDA and hard-errors (exit
code 5, `ProviderUnavailable`) with no fallback if the stack is incomplete or registration
fails — checked *before* any download is attempted. See
[specs/03-config.md](../specs/03-config.md) §7 for the full provider table.

---

## Known gaps {#known-gaps}

This section documents verified divergences between the specs and the v0.1.0 implementation. They are listed honestly so contributors know where work remains. Each item names the responsible code area.

**1. HTTP daemon `POST /v1/jobs` is a no-op.**
The daemon's job-submission endpoint accepts the request and reports the job state machine (`pending → done`) but does not run the ingestion pipeline; `chunks_written` stays `0`. Daemon-side reads (`/v1/search`, `/v1/documents/{id}`, `/v1/status`) DO see CLI-indexed data because the daemon now opens the same unified database as the CLI. To actually index, run `localdb index` from the CLI (which still works while the daemon runs — concurrent writers serialise via SQLite WAL).

**Gap #2. `source add` does not validate path existence.** ([#14](https://github.com/dokterbob/localdb/issues/14))
**Resolved as of 2026-06-28:** `cli/src/lib.rs` now validates path existence in `run_source_add_async` via `normalize_path_source`.
`localdb source add /does/not/exist --store notes` succeeds (exit 0) even when the path does not exist on disk. Validation is deferred to index time. The source spec validation in `core/src/config/` or the CLI source-add handler is the place to add an existence check.

**Gap #3. macOS default paths use a verbose bundle ID.** ([#15](https://github.com/dokterbob/localdb/issues/15))
**Resolved as of 2026-06-28:** `core/src/config/platform.rs` now uses `ProjectDirs::from("", "", "localdb")` for clean default paths.
The default config, data, and model-cache locations on macOS all live under the bundle ID `com.localdb.localdb.localdb` (e.g. data at `~/Library/Application Support/com.localdb.localdb.localdb/data`). The triple-repeat comes from `ProjectDirs::from("com.localdb", "localdb", "localdb")` in `core/src/config/platform.rs`. Specs/03 shows shorter `localdb/` paths. Cosmetic; override with `paths.*` in config for cleaner locations.

**4. The CoreML context bundle ships only the L512 sequence-length bucket.**
The CoreML backend (`local-coreml` feature; see [Platform notes](#platform-notes)) reads its bucket manifest from HF repo `dokterbob/pplx-embed-coreml`. Today only the `context/L512-int8` bucket is published. The larger context buckets (`L ∈ {1024, 2048, 4096}`) are picked up automatically from the manifest once published, so no code change is needed. This XET-deduped download that shares the ~1.15 GB encoder weights across buckets relies on the `hf-hub` 1.0 pre-release.

**5. Sources added before the include-allowlist change keep empty `include` globs.**
As of the `only-index-supported-files` branch, `cli` automatically sets `DEFAULT_PATH_INCLUDES` (an extension-based allowlist) on new directory sources that have no explicit `include` globs. Sources that were added before this change already have an empty `include` list recorded in the unified database and will continue to index all files they enumerate until they are removed and re-added with `localdb source add`. There is no automatic migration, and this change is intentionally not folded into `policy_version`. The per-file chunk preset is determined deterministically from the filename/MIME type at index time, so re-indexing existing content with the new code produces correct results without a policy-hash change.

**6. CUDA execution provider is untested on real NVIDIA hardware.** ([issue #76](https://github.com/dokterbob/localdb/issues/76))
The `local-cuda` provider, `local` mode's automatic CUDA preference, and the CPU/CUDA index
parity claim in [specs/03-config.md](../specs/03-config.md) §7 are all exercised in CI only on
GPU-less runners (`ci.yml`'s `ort-download` job asserts the CUDA-flavored ONNX Runtime
downloads, verifies, and fails *cleanly* without a GPU — it cannot exercise a real EP
registration or actual inference). `scripts/cuda-verify.sh` is a one-shot manual verification
kit for someone with NVIDIA hardware to run against a release tarball; until that run happens
and this note is updated with the result, treat GPU execution and CPU/CUDA output parity as
unverified, not merely untested-in-CI.

---

## Deferred design decisions {#design-decisions}

Several items surfaced during the v0.1.0 issue sweep require cross-cutting design decisions before code can be written. They are documented (with options and recommendations) in [docs/design-decisions.md](design-decisions.md):

- **A7**: `policy_version` does not hash resolved per-source chunking parameters.
- **A8 / B4**: Pagination offset computed but never applied; `total_candidates` is pre-dedup.
- **B2**: Cross-store deduplication semantics (collapse vs. distinct citations).
- **B3**: Rerank seam re-attaches store metadata by index position (safe today, unsafe with real reranker).
- **E1**: Structured MCP tool results (spec-decided, implementation deferred to v0.2.0).
- **A9-charset**: Allowed character set for store names beyond traversal-safety.
