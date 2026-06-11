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
`Block`, `Chunk`, `IndexJob`, `Citation`), the two key traits (`RetrievalStore` and
`Embedder`), content-addressed ID derivation (blake3), the RRF fusion engine, indexing
orchestration, and the error taxonomy. Contains no I/O framework; everything async-capable
lives in other crates. This is the crate everything else imports.

### `extract`

Format detection and extraction. Accepts raw bytes and returns a normalized text string plus
a `Vec<Block>` (heading sections, paragraph groups, code fences). Supported in v1: Markdown
(pulldown-cmark), plain text, HTML (readability-style), and text-layer PDF. Unsupported files
are counted as skipped in `IndexJob` stats, not treated as errors. See
[specs/04-search-pipeline.md](../specs/04-search-pipeline.md) §2.

### `embed`

`Embedder` implementations. Declares providers for local ONNX inference
(`OnnxEmbedder`, feature-gated `local-onnx`), OpenAI-compatible flat HTTP endpoints
(`OpenAiEmbedder`), Perplexity contextualized embeddings (`PerplexityEmbedder`), and Voyage
(`VoyageEmbedder`). All implement the document-aware `Embedder` trait from `core`, which
groups chunks by document so contextualized/late-chunking models can use the surrounding
document as context. **Note:** none of these providers are wired into the running binary yet
— see [Known gaps §1](#known-gaps). See [specs/04-search-pipeline.md](../specs/04-search-pipeline.md) §4.

### `store-lancedb`

The `RetrievalStore` trait implementation backed by LanceDB embedded. One LanceDB database
per logical store, stored under `{data_dir}/stores/{store_name}/`, with a single `chunks`
table. BM25 full-text search uses LanceDB's built-in FTS index (tantivy underneath); dense
search uses IVF-PQ or exact KNN (auto-selected by row count). RRF fusion is intentionally
_not_ done here — the trait returns raw ranked lists and `core` fuses them. See
[specs/01-architecture.md](../specs/01-architecture.md) §2.

### `cli`

Command implementations. A thin layer on `core` and the daemon client; no business logic.
Each command handler acquires config and runtime state, probes the daemon socket, then either
delegates to the HTTP API (thin-client mode) or opens the store in-process (embedded mode).
Also holds the `FakeEmbedder` wiring that is active today — see [Known gaps §1](#known-gaps).

### `server`

The axum-based HTTP API daemon. Exposes the `/v1` REST surface, manages the daemon unix
socket for discovery, holds the write lock, runs the file-watcher (`notify`), the URL
refresh scheduler, and the background job queue. This is an **experimental** preview;
see [Known gaps §2](#known-gaps). See [specs/05-surfaces.md](../specs/05-surfaces.md) §3.

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
 │  extract  →  normalized text + Blocks                   │
 │       │                                                 │
 │       ▼                                                 │
 │  chunker  →  Chunks  (heading-aware, ~400-token prose)  │
 │       │                                                 │
 │       ▼                                                 │
 │  Embedder  →  dense vectors  [currently: FakeEmbedder]  │
 │       │                                                 │
 │       ▼                                                 │
 │  store-lancedb  →  chunks.lance  (BM25 index + vectors) │
 └─────────────────────────────────────────────────────────┘

 ┌─────────────────────────────────────────────────────────┐
 │                     READ PATH                           │
 │                                                         │
 │  query string                                           │
 │       │                                                 │
 │       ├──────────────────────────────────┐              │
 │       ▼                                  ▼              │
 │  BM25 search (tantivy/LanceDB)    dense search (KNN)    │
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
in-process (mmapped LanceDB; embeddings come from the placeholder `FakeEmbedder` in v0.1.0 —
see [Known gaps §1](#known-gaps)). No configuration is needed for the common case. See [specs/01-architecture.md](../specs/01-architecture.md) §3.

**Important:** the thin-client routing is not reachable today because CLI commands open the
runtime-state database before completing the probe — see [Known gaps §3](#known-gaps).

---

## On-disk layout

The config file and the data directory are independent paths (`--config` /
`LOCALDB_CONFIG` choose the former; `paths.data` the latter). After
`localdb init` and `localdb index`:

```
<config_dir>/
  config.yaml                  # YAML config (version: 1)

<data_dir>/
  cli-sources.redb             # redb: source records created via CLI
  runtime-state.redb           # redb: runtime stores, daemon state
  stores/
    <store_name>/
      chunks.lance             # LanceDB table: chunks + BM25 FTS + vectors
  daemon.sock                  # unix socket (present only while daemon runs)
```

The default `data_dir` on macOS is `~/Library/Application Support/com.localdb.localdb.localdb/data`
(the bundle ID is intentionally verbose — see [Known gaps §7](#known-gaps)).
Override with `paths.data` in `config.yaml` or point to a custom config with `--config`.

The `models/` directory (configured via `paths.models`) is created only when a real embedder
downloads a model. With the current `FakeEmbedder`, no model files are written.

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

## Known gaps {#known-gaps}

This section documents verified divergences between the specs and the v0.1.0 implementation.
They are listed honestly so contributors know where work remains. Each item names the
responsible code area.

**1. Embeddings are hash-based placeholders (`FakeEmbedder`).** ([#8](https://github.com/dokterbob/localdb/issues/8), [#16](https://github.com/dokterbob/localdb/issues/16))
The `embed` crate contains real provider implementations (ONNX, OpenAI-compatible,
Perplexity, Voyage), but `cli/src/lib.rs` wires `FakeEmbedder::new(128)` for both index
and search. No model download occurs; the `init` message "embedding models will be downloaded
on first index" does not apply yet. Dense scores in search results are a constant placeholder
(`dense: 1.0`); ranking is driven entirely by BM25. The `embed` crate and `RetrievalStore`
trait are ready — the wiring in the CLI command handlers is the remaining work.

**2. The HTTP daemon uses an in-memory `FakeStore`.** ([#9](https://github.com/dokterbob/localdb/issues/9))
`server/src/handlers.rs` holds one shared in-memory store behind an `Arc`. All API routes
return correct JSON shapes, but `POST /v1/search` returns empty citations and job indexing
operates on the in-memory store regardless of what the CLI has indexed into LanceDB on disk.
The daemon is an experimental preview; do not rely on it for search correctness today.
Plumbing the `LanceDbStore` into `AppState` is the remaining work.

**3. CLI commands are blocked while the daemon runs.** ([#10](https://github.com/dokterbob/localdb/issues/10), [#11](https://github.com/dokterbob/localdb/issues/11))
Every CLI command opens `runtime-state.redb` before completing the daemon probe, so running
any CLI command while `localdb serve` is active returns `error: internal error ... Database
already open. Cannot acquire lock.` (exit 1). The spec's thin-client routing path
(`cli/src/lib.rs`, daemon probe) exists in the code but is unreachable. Additionally, the
probe falls back to `http://127.0.0.1:7700` (cannot read the URL from the real unix socket),
so it assumes the default port. A stale `daemon.sock` left by a killed daemon causes CLI to
report "daemon running" and `search` to exit with 5 "daemon is unreachable"; fix by removing
the socket file manually.

**4. YAML-declared stores cannot be indexed.** ([#12](https://github.com/dokterbob/localdb/issues/12))
Stores declared in `config.yaml` under the `stores:` key appear in `localdb store list` as
`(yaml)`, but `localdb index --store <name>` returns `error: store not found: <name>` (exit
3). The `run_index` function in `cli/src/lib.rs` (~line 1108) resolves stores only from the
runtime-state database. The working path today is to create stores at runtime with
`localdb store add <name>` and add sources with `localdb source add`. YAML store
declarations are config-only for now.

**5. `search --store <unknown>` exits 0 instead of 3.** ([#13](https://github.com/dokterbob/localdb/issues/13))
When `--store` names an unknown store, `localdb search` prints "No indexed stores found.
Run `localdb index` first." and exits 0 with `{"citations":[]}`, rather than returning exit
code 3 as `store remove` and other not-found cases do. This inconsistency lives in the
search command handler in `cli/src/lib.rs`.

**6. `source add` does not validate path existence.** ([#14](https://github.com/dokterbob/localdb/issues/14))
`localdb source add /does/not/exist --store notes` succeeds (exit 0) even when the path does
not exist on disk. Validation is deferred to index time. The source spec validation in
`core/src/config/` or the CLI source-add handler is the place to add an existence check.

**7. macOS default paths use a verbose bundle ID.** ([#15](https://github.com/dokterbob/localdb/issues/15))
The default config, data, and model-cache locations on macOS all live under the bundle ID
`com.localdb.localdb.localdb` (e.g. data at
`~/Library/Application Support/com.localdb.localdb.localdb/data`). The triple-repeat comes
from `ProjectDirs::from("com.localdb", "localdb", "localdb")` in
`core/src/config/platform.rs`; specs/03 shows shorter `localdb/` paths. Cosmetic; override
with `paths.*` in config for cleaner locations.
