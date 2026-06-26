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

---

## Known gaps {#known-gaps}

This section documents verified divergences between the specs and the v0.1.0 implementation. They are listed honestly so contributors know where work remains. Each item names the responsible code area.

**1. HTTP daemon `POST /v1/jobs` is a no-op.**
The daemon's job-submission endpoint accepts the request and reports the job state machine (`pending → done`) but does not run the ingestion pipeline; `chunks_written` stays `0`. Daemon-side reads (`/v1/search`, `/v1/documents/{id}`, `/v1/status`) DO see CLI-indexed data because the daemon now opens the same unified database as the CLI. To actually index, run `localdb index` from the CLI (which still works while the daemon runs — concurrent writers serialise via SQLite WAL).

**2. YAML-declared stores cannot be indexed.** ([#12](https://github.com/dokterbob/localdb/issues/12))
Stores declared in `config.yaml` under the `stores:` key appear in `localdb store list` as `(yaml)`, but `localdb index --store <name>` returns `error: store not found: <name>` (exit 3). The `run_index` function in `cli/src/lib.rs` resolves stores only from the unified database. Today's working path is to create stores at runtime with `localdb store add <name>` and add sources with `localdb source add`. YAML store declarations are config-only for now.

**3. `source add` does not validate path existence.** ([#14](https://github.com/dokterbob/localdb/issues/14))
`localdb source add /does/not/exist --store notes` succeeds (exit 0) even when the path does not exist on disk. Validation is deferred to index time. The source spec validation in `core/src/config/` or the CLI source-add handler is the place to add an existence check.

**4. macOS default paths use a verbose bundle ID.** ([#15](https://github.com/dokterbob/localdb/issues/15))
The default config, data, and model-cache locations on macOS all live under the bundle ID `com.localdb.localdb.localdb` (e.g. data at `~/Library/Application Support/com.localdb.localdb.localdb/data`). The triple-repeat comes from `ProjectDirs::from("com.localdb", "localdb", "localdb")` in `core/src/config/platform.rs`. Specs/03 shows shorter `localdb/` paths. Cosmetic; override with `paths.*` in config for cleaner locations.

**5. The CoreML context bundle ships only the L512 sequence-length bucket.**
The CoreML backend (`local-coreml` feature; see [Platform notes](#platform-notes)) reads its bucket manifest from HF repo `dokterbob/pplx-embed-coreml`. Today only the `context/L512-int8` bucket is published. The larger context buckets (`L ∈ {1024, 2048, 4096}`) are picked up automatically from the manifest once published, so no code change is needed. This XET-deduped download that shares the ~1.15 GB encoder weights across buckets relies on the `hf-hub` 1.0 pre-release.

**6. Sources added before the include-allowlist change keep empty `include` globs.**
As of the `only-index-supported-files` branch, `cli` automatically sets `DEFAULT_PATH_INCLUDES` (an extension-based allowlist) on new directory sources that have no explicit `include` globs. Sources that were added before this change already have an empty `include` list recorded in the unified database and will continue to index all files they enumerate until they are removed and re-added with `localdb source add`. There is no automatic migration, and this change is intentionally not folded into `policy_version`. The per-file chunk preset is determined deterministically from the filename/MIME type at index time, so re-indexing existing content with the new code produces correct results without a policy-hash change.

---

## Deferred design decisions {#design-decisions}

Several items surfaced during the v0.1.0 issue sweep require cross-cutting design decisions before code can be written. They are documented (with options and recommendations) in [docs/design-decisions.md](design-decisions.md):

- **A7**: `policy_version` does not hash resolved per-source chunking parameters.
- **A8 / B4**: Pagination offset computed but never applied; `total_candidates` is pre-dedup.
- **B2**: Cross-store deduplication semantics (collapse vs. distinct citations).
- **B3**: Rerank seam re-attaches store metadata by index position (safe today, unsafe with real reranker).
- **E1**: Structured MCP tool results (spec-decided, implementation deferred to v0.2.0).
- **A9-charset**: Allowed character set for store names beyond traversal-safety.
