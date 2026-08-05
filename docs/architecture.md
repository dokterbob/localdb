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

Schema changes go through an explicit migrations runner (`store-libsql/src/migrations/`): a
frozen baseline DDL snapshot (`baseline.rs`, `PRAGMA user_version = 4`) plus a linear, numbered
chain of `Migration` entries applied one transaction at a time. A `schema_migrations` table is
the source of truth for version, with `PRAGMA user_version` kept as a cheap, non-authoritative
marker. **Opening a store never migrates it, in either direction** — a version mismatch on open
is refused with an actionable hint, on every surface (CLI, HTTP daemon, MCP alike). The only way
to change a store's schema version is `localdb db migrate` / `localdb db downgrade [--to N]`
(CLI-only; `db status` is read-only and never refuses). See
[docs/migrations.md](migrations.md) for the full user-facing and authoring guide, and
[specs/02-domain-model.md](../specs/02-domain-model.md) §9 /
[specs/05-surfaces.md](../specs/05-surfaces.md) §2.1 for the design.

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

MCP server built on the official `rmcp` SDK (full macro-native `#[tool_router]`/
`#[tool_handler]`), speaking the same `Citation` shape that every other surface uses.
Exposes four read-only tools — `search`, `get_document`, `get_chunks`, `list_stores`.
Served over two transports: stdio (`localdb mcp`, embedded-in-process or, if a daemon
is already running, proxied to its `/mcp` route — see `mcp/src/proxy.rs`) and HTTP
(`/mcp`, mounted on `server`'s axum router alongside `/v1` — see `mcp/src/http.rs` and
`server/src/mcp_bridge.rs`). The `--allow-write` flag is parsed for forward
compatibility but write tools are rejected in v1 on both transports.
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
 │                        READ PATH                        │
 │                                                         │
 │  query string                                           │
 │       │                                                 │
 │       ▼                                                 │
 │  fan out per store: BM25 (FTS5) + dense (KNN)           │
 │       │                                                 │
 │       ▼                                                 │
 │  pool each leg across all stores (global rank per leg)  │
 │       │                                                 │
 │       ▼                                                 │
 │  single RRF fusion (k=60; key = store_id+chunk_id)      │
 │       │                                                 │
 │       ▼                                                 │
 │  top-N Citations (fused + per-leg scores)               │
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
populate `<cache_dir>/localdb/ort/<version>/` on first use with the embedded ONNX Runtime
shared library — a separate, sibling directory to `models/`, not configurable via `paths.*`.
See [Platform notes: ONNX Runtime loading](#platform-notes).

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

**ONNX Runtime loading (`local-onnx`, all platforms).** `embed`'s `ort` dependency uses the
`load-dynamic` feature — the `localdb` executable links no ONNX Runtime ABI at all and instead
`dlopen`s a shared library at a path chosen at runtime. `embed/build.rs` downloads *Microsoft's
official* ONNX Runtime release (pinned to 1.24.4) for the build's target platform
(`linux-x64`, `linux-aarch64`, `osx-arm64`), verifies it against a pinned sha256, and embeds it
into the `embed` crate via `include_bytes!`. On first construction of any local-ONNX embedder,
`embed::ort_runtime::ensure_ort_initialized` extracts that embedded library to
`<cache_dir>/localdb/ort/<version>/` (mirroring the model-cache convention; idempotent —
skipped if an up-to-date copy is already there) and calls `ort::init_from` on it before any
other `ort` API is touched. Embedding the runtime grows the `localdb` binary by roughly
12–20 MB depending on platform (measured: +11.8 MiB on macOS arm64, where it also replaced
the previous statically-linked archive); the compressed release tarballs grow less.

Two overrides exist for power users / distro packagers who want to supply their own ONNX
Runtime instead of the embedded one:
- `ORT_DYLIB_PATH` (runtime env var): `ensure_ort_initialized` honours this directly and
  `dlopen`s that path instead of extracting the embedded copy.
- `LOCALDB_ORT_LIB` (build-time env var, read by `embed/build.rs`): points the *build* at a
  local ONNX Runtime library to embed instead of downloading one (offline/distro builds).

Both overrides require ONNX Runtime **≥ 1.24** — `fastembed`'s own (unconditional) `ort`
dependency declaration requests the `api-24` feature regardless of which `fastembed` features
we enable, so despite `embed`'s own `ort` dependency line specifying no `api-*` feature, Cargo
feature unification still enables `api-24` for the whole build. This is why the pinned embedded
version is exactly 1.24.4, not an older 1.17–1.23 release.

Why this exists: `ort`'s `download-binaries` feature (the previous approach) statically links
pyke.io's prebuilt ONNX Runtime archive into `ort-sys`. That archive is built with GCC 14 on
Ubuntu 24.04 and references `__isoc23_strtol*` symbols, giving the *release binary itself* a
`GLIBC_2.38` floor — it refused to start on glibc-2.35 distros (Linux Mint 21.x, Ubuntu 22.04).
It was also ABI-incompatible with GCC-11 libstdc++ when built on ubuntu-22.04. See
[issue #133](https://github.com/dokterbob/localdb/issues/133) and
[pykeio/ort#523](https://github.com/pykeio/ort/issues/523) (unresolved upstream). The Microsoft
official Linux builds we embed instead float at `GLIBC_2.27` / `GLIBCXX_3.4.22` / `CXXABI_1.3.11`
(verified via `objdump -T`), comfortably under Ubuntu 22.04's `GLIBC_2.35` baseline; the
embedded macOS dylib declares a minimum of macOS 14.0 (`LC_BUILD_VERSION`). Because our own
Rust code still inherits the *build machine's* glibc floor independent of this mechanism, the
release and CI workflows also pin Linux builds to `ubuntu-22.04` (not `ubuntu-latest`) and
verify both the `localdb` binary and the embedded `.so` stay at or below `GLIBC_2.35`.

---

## Known gaps {#known-gaps}

This section documents verified divergences between the specs and the v0.1.0 implementation. They are listed honestly so contributors know where work remains. Each item names the responsible code area.

**Recently closed, not (re)listed below:** `--store` used to be honored only by `search`/`mcp` — every other command silently operated on an arbitrary store instead of respecting the flag's absence consistently (#178, #118). `--store` is now resolved and validated the same way everywhere, with a per-command default documented in [specs/05-surfaces.md §2.2](../specs/05-surfaces.md#22-store-scope): all stores for `search`/`status`/`store list`/`index`, the store named `default` for `source`/`add`, and rejected outright for `db status`/`migrate`/`downgrade`. Separately, MCP `get_document`/`get_chunks` now accept an optional `store` argument (id or name) to disambiguate a document id that exists in more than one store (#144; see [docs/mcp.md](mcp.md#get_document)). Gaps #6 and #7 below (the `/mcp` HTTP store-list snapshot and daemon-proxied `localdb mcp --store`) are related but distinct and remain open.

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

**6. `/mcp` (HTTP) doesn't see stores added after daemon startup.**
`server::mcp_bridge::build_available_stores` snapshots the daemon's store list once,
at `start_daemon` time — a store added later via `POST /v1/stores` is invisible over
MCP until the daemon restarts. Root cause: `rmcp`'s Streamable HTTP service factory is
synchronous, so there's no hook to redo the async `AppState` lookup per session
without an ugly blocking bridge. See [docs/mcp.md](mcp.md#remote-http-connecting-from-another-machine).

**7. `--store` is not honored when `localdb mcp` proxies to a running daemon.**
The daemon's `/mcp` route has no concept of a per-stdio-session store filter, so
proxied stdio mode always exposes the daemon's full store set regardless of
`--store`; a non-fatal warning is printed to stderr. Building client-side
re-filtering for this was rejected as not worth the complexity in v1. See
[docs/mcp.md](mcp.md#daemon-proxied-stdio) and [specs/05-surfaces.md](../specs/05-surfaces.md) §4.2.

---

## Deferred design decisions {#design-decisions}

Several items surfaced during the v0.1.0 issue sweep require cross-cutting design decisions before code can be written. They are documented (with options and recommendations) in [docs/design-decisions.md](design-decisions.md):

- **A7**: `policy_version` does not hash resolved per-source chunking parameters.
- **A8 / B4**: Pagination offset computed but never applied; `total_candidates` is pre-dedup.
- **B2**: Cross-store deduplication semantics (collapse vs. distinct citations).
- **B3**: Rerank seam re-attaches store metadata by index position (safe today, unsafe with real reranker).
- **E1**: Structured MCP tool results (spec-decided, implementation deferred to v0.2.0).
- **A9-charset**: Allowed character set for store names beyond traversal-safety.
