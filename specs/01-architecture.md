# Spec 01 — Architecture & Crate Boundaries

> Status: accepted draft, 2026-06-10. Placeholder product name: `localdb` (naming is out of scope).

## 1. Repository & workspace layout

**Decision:** one monorepo, one Cargo workspace, a small number of crates, **one binary**.
Split into separate repos only when external reuse demand actually appears.

**Rationale:** layered reuse is a goal (product principle 7), but multi-repo costs release
coordination and contributor onboarding before there is a single user. Crate boundaries inside a
workspace give the same layering at near-zero cost.
**Rejected:** multi-repo from the start; separate binaries per surface (operational sprawl, three
things to version and install instead of one).

| Crate | Contents |
|---|---|
| `core` | Domain model (stores, sources, documents, blocks, chunks, citations, index jobs), search orchestration, indexing policy, the `RetrievalStore` trait, the `Embedder` trait, error taxonomy. No I/O frameworks. |
| `extract` | Format detection and extraction → normalized document (Markdown, plain text, HTML, text-layer PDF in v1). |
| `store-libsql` | libsql implementation of `RetrievalStore` (DiskANN vectors + FTS5). |
| `embed` | `Embedder` implementations: local ONNX (fastembed-class), OpenAI-compatible HTTP provider, contextualized-embedding providers. Model download/cache management. |
| `server` | HTTP API (axum or similar), daemon runtime: file watching, URL refresh scheduling, job queue, unix socket. |
| `mcp` | MCP server over stdio, thin layer on `core` (or on the daemon client, see §3). |
| `cli` | Command implementations, thin layer on `core` / daemon client. |
| `localdb` (bin) | Single binary with subcommands: `serve`, `mcp`, `init`, `index`, `search`, `status`, `store`, `source`. See [05-surfaces.md](05-surfaces.md). |

**Invariant:** all surfaces (CLI, HTTP, MCP) sit on the same `core`; no retrieval, indexing, or
domain logic is implemented in a surface crate — one shared core beats duplicated logic.

## 2. Surface ordering & storage default

1. **CLI + MCP ship first; web UI follows in a second iteration.** Rationale: the primary early
   users are technical and agent users; CLI+MCP exercise the entire core without any frontend
   build, and the embedded-first process model (§3) makes them usable with zero daemon setup.
   **Rejected:** web-UI-first — front-loads a frontend build and a daemon before the core has
   proven users.
2. **libsql embedded is the local default, behind a trait.** Storage goes behind the
   `RetrievalStore` trait in `core`; the default implementation is **libsql** (Turso's SQLite
   fork, in-process, MIT-licensed) — a single engine providing DiskANN vector search, FTS5 for
   BM25, and relational metadata in one file. Qdrant server becomes the remote-mode adapter on the
   roadmap; **Qdrant Edge** (in-process, pre-GA ~0.6.x as of early 2026) is a watch-item
   ([06-roadmap.md](06-roadmap.md) §3). Hybrid fusion (RRF) is done in our code above the trait,
   not delegated ([04-search-pipeline.md](04-search-pipeline.md) §5). **Rejected:** Qdrant as
   local default — Qdrant has no embedded mode (server-only), which would force a daemon-always
   model and contradict §3.

## 3. Process model: embedded-first, daemon-optional

**Decision:** CLI and MCP link `core` directly and open the store **in-process** (libsql database
\+ ONNX models). No daemon is required for any MVP function. A daemon (`localdb serve`) is
optional; when one is running, CLI and MCP become thin clients of its HTTP API.

- **Discovery:** a unix socket at a well-known path in the data dir
  ([03-config.md](03-config.md) §4). Socket present and responsive → route through daemon;
  otherwise → embedded mode. No configuration needed for the common case.
- **Concurrency model:** SQLite WAL and `busy_timeout=5000` is the sole concurrency primitive. No advisory file lock. Multi-process is the first-class topology. Multiple stdio MCP servers, a CLI session running `localdb index`, and an optional `localdb serve` daemon may all share one data directory as peers. The daemon is no longer special. SQLite admits one writer at a time. Concurrent writers serialise via `busy_timeout`. An exhausted busy-timeout maps to `Error::RuntimeStateLocked` (exit 4).
- **Daemon-exclusive capabilities:** continuous file watching, scheduled URL refresh, the HTTP
  API and (later) web UI, background job queue. Embedded mode does one-shot equivalents
  (`localdb index` = scan now; no watching).

**Rationale:** `localdb search foo` must work seconds after install with nothing running — this is
the local-first promise. **Rejected:** daemon-always — heavier install, worse first-run; pure-embedded with no daemon — loses watching, refresh, and the web
surface for home-server mode.

## 4. Stores vs. backends

Two concepts, deliberately separated:

- A **store** (logical, `core`): a named knowledge base with identity (stable ID), a
  `visibility` field (`private` | `shared` — enum exists in MVP, only `private` is functional),
  ACL hooks (empty in MVP), its own sources, and its own indexing policy
  ([03-config.md](03-config.md) §2). **Multiple stores per instance from day one** — e.g. files
  vs. bookmarks vs. (later) email. Stores are the unit of sharing and federation
  ([VISION.md](../VISION.md)).
- A **backend** (physical): an implementation of `RetrievalStore` that holds a store's index.
  MVP: `libsql` (embedded, single engine with DiskANN vectors and FTS5). Roadmap: `qdrant`
  (remote server), possibly Qdrant Edge. A store declares its backend in config; default is
  `libsql`.

`RetrievalStore` (trait sketch — normative surface, not final signatures): upsert chunks (dense
vector + text for BM25 + metadata), delete by document, dense search, BM25 search, metadata
filtering, store-level stats. Fusion happens above the trait in `core`.

## 5. Federation-readiness constraints (design constraints only)

MVP implements none of the federation behavior in [VISION.md](../VISION.md), but every MVP
component must respect:

1. **Stable, content-addressed IDs** for documents and chunks ([02-domain-model.md](02-domain-model.md) §3) — IDs must be meaningful outside the node that minted them.
2. **Provenance on every chunk** (origin store, source, content hash, fetch time).
3. **Per-store visibility** modeled as an enum, never a boolean bolted on later.
4. **No assumption of a single store** anywhere in core, surfaces, or config.

## 6. Runtime & concurrency

**Decision:** async on **tokio** for all I/O and orchestration — but not literally everything.

- **Async:** the daemon (HTTP API, file watching, schedulers, job queue), the `RetrievalStore`
  and `Embedder` traits (their backends are inherently async: libsql's Rust API is async, hosted
  providers are HTTP), URL fetching, and surface plumbing.
- **Not async:** CPU-bound work — ONNX inference, extraction/parsing, chunking, blake3 hashing —
  runs on a blocking/rayon pool via `spawn_blocking`-style handoff, never on the async executor.
  Pure domain logic in `core` (ID derivation, fusion, policy hashing, config resolution) stays
  sync and runtime-agnostic; only the orchestration around it is async.
- **Embedded mode:** one-shot CLI/MCP commands spin up a tokio runtime per invocation; the cost
  is negligible against model load and index I/O.

**Rationale:** the daemon needs real concurrency (watchers + jobs + HTTP) regardless, and the
storage/embedding dependencies are async-native — one execution model everywhere beats a sync
core wrapped in adapter shims. **Rejected:** fully synchronous core with hand-rolled threads
(fights the storage backend's async API, reinvents the daemon's scheduling); async-everything including CPU
work (starves the executor during indexing).

## 7. Development practices: TDD and coverage gates

**Decision:** test-driven development is the **default mode** for all crates: write the failing
test first, then the implementation. Coverage gates, enforced in CI (e.g. `cargo llvm-cov`):

- **≥ 80%** line coverage for critical functions — search orchestration, fusion, chunking,
  extraction normalization, config resolution, ID derivation.
- **≥ 90%** for anything that **modifies data** — store upserts/deletes, index job execution,
  document/chunk writes, config/state mutation, migrations.

Trait-based seams (`RetrievalStore`, `Embedder`) exist partly to make this practical. Core logic is tested against a real tmpdir SqliteBackend. Adapter crates are tested against the real backend (libsql tmpdir, ONNX tiny model) in integration tests. Every ticket in [PLAN.md](../PLAN.md) carries test expectations. A ticket is not done below its gate.
