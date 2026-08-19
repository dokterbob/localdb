# Spec 01 — Architecture & Crate Boundaries

> Status: accepted draft, 2026-06-10. Placeholder product name: `localdb` (naming is out of scope).

## 1. Repository & workspace layout

**Decision:** one monorepo, one Cargo workspace, a small number of crates, **one binary**. Split
into separate repos only when external reuse demand actually appears.

**Rationale:** layered reuse is a goal (product principle 7), but multi-repo costs release
coordination and contributor onboarding before there is a single user. Crate boundaries inside a
workspace give the same layering at near-zero cost. **Rejected:** multi-repo from the start;
separate binaries per surface (operational sprawl, three things to version and install instead of
one).

| Crate           | Contents                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `core`          | Domain model (stores, sources, resources, blocks, chunks, citations, index jobs), the `Ingestor` trait contract and `IngestorKind` enum, the ingestion pipeline (`run_source_ingestion`, `index_resource`), search orchestration, indexing policy, the `RetrievalStore` trait, the `Embedder` trait, error taxonomy. **No I/O**: `core` drives ingestion only through a caller-supplied `&dyn Ingestor`; it never opens a file, socket, or HTTP client itself. |
| `extract`       | Format detection and extraction → blocks (Markdown, plain text, HTML, text-layer PDF in v1). Implementation detail of the file ingestor.                                                                                                                                                                                                                                                                                                                       |
| `ingest`        | Concrete `Ingestor` implementations: `FileIngestor`, `UrlIngestor`, and future connectors (Atom/RSS, Notion, Telegram, Signal, HackMD, email, transcription). Depends on `core` + `extract` + `fetch`. Owns all acquisition I/O — filesystem reads, credential prompts — but its outgoing HTTP goes through `fetch`, not a bare client of its own.                                                                                                             |
| `fetch`         | Owns the shared outgoing-HTTP layer (issue #207): retry via `backon` (429/408/5xx/timeout, honoring `Retry-After`), per-host pacing via `governor` (keyed on the destination host, loopback/LAN exempt), and the SSRF destination guard for third-party-discovered URLs. `ingest`'s `UrlIngestor` and `embed`'s hosted providers both build their `reqwest::Client` through it rather than constructing one directly.                                          |
| `store-libsql`  | libsql implementation of `RetrievalStore` (DiskANN vectors + FTS5).                                                                                                                                                                                                                                                                                                                                                                                            |
| `embed`         | `Embedder` implementations: local ONNX (fastembed-class), OpenAI-compatible HTTP provider, contextualized-embedding providers. Model download/cache management. Hosted providers consume `fetch`'s retry/`Retry-After` handling (reactive only — no proactive per-host pacing against paid APIs).                                                                                                                                                              |
| `server`        | HTTP API (axum or similar), daemon runtime: file watching, URL refresh scheduling, job queue, daemon sentinel.                                                                                                                                                                                                                                                                                                                                                 |
| `mcp`           | MCP server over stdio, thin layer on `core` (or on the daemon client, see §3).                                                                                                                                                                                                                                                                                                                                                                                 |
| `cli`           | Command implementations, thin layer on `core` / daemon client.                                                                                                                                                                                                                                                                                                                                                                                                 |
| `localdb` (bin) | Single binary with subcommands: `serve`, `mcp`, `init`, `index`, `search`, `status`, `store`, `source`. See [05-surfaces.md](05-surfaces.md).                                                                                                                                                                                                                                                                                                                  |

**Invariant:** all surfaces (CLI, HTTP, MCP) sit on the same `core`; no retrieval, indexing, or
domain logic is implemented in a surface crate — one shared core beats duplicated logic.

**Ingestor crate boundary:** the `Ingestor` trait and the ingestion pipeline that drives it
(`run_source_ingestion`, `index_resource`) live in `core` — the contract and the orchestration,
never the I/O. Concrete ingestor implementations (`FileIngestor`, `UrlIngestor`, and future
connectors), terminal interaction, credential prompts, HTTP/API clients, and source-specific setup
live in the `ingest` crate. The CLI constructs the concrete ingestor for a given `SourceSpec` and
passes it into `core::run_source_ingestion` as `&dyn Ingestor`; this restores the "no I/O in `core`"
invariant, which the prior `core::ingestors::*` modules violated by calling `std::fs::read` directly
(issue #117). **Rejected:** leaving concrete ingestors in `core` or scattering them across `cli` —
either reintroduces I/O into the domain layer or duplicates ingestor construction per surface.

`core` owns normalizing locators for identity and delete-sweep decisions: every `Uri` reaching the
pipeline (via `on_resource`'s `Resource.uri` or `on_skipped`'s `uri` parameter) is canonical by
construction, and an ingestor is never required to normalize one itself to stay correct — it only
has to produce a valid `Uri` in the first place.

**Ingestion pipeline shape:** the CLI builds one concrete ingestor per `SourceSpec`, then calls
`core::run_source_ingestion(source, &dyn Ingestor, deps)`. The ingestor streams `Resource`s one at a
time through an `IngestCallback` — no buffering of an entire source's resources in memory. Per
resource, `core` runs: skip-check (unchanged content hash) → chunk → embed →
`upsert_chunks_and_blocks` (crash-safe A6 ordering — embed before delete, delete-and-insert in a
single replace transaction, issue #79). Per-resource errors become stats counters and progress
events, not aborts of the run. `IngestCallback` provides default-no-op `on_discovered(total)` and
`on_skipped(uri, SkipReason)` hooks so ingestors can report enumeration size and pre-filtered items
without every implementation having to wire them up. After `ingest()` returns, `core` runs the
delete-sweep: URIs seen in a prior run but not this one — including URLs the ingestor reports `Gone`
— are deleted. This single pipeline replaces the legacy `index_document` extraction pipeline, which
extracted and buffered per-document logic directly inside `core`.

## 2. Surface ordering & storage default

1. **CLI + MCP ship first; web UI follows in a second iteration.** Rationale: the primary early
   users are technical and agent users; CLI+MCP exercise the entire core without any frontend build,
   and the embedded-first process model (§3) makes them usable with zero daemon setup. **Rejected:**
   web-UI-first — front-loads a frontend build and a daemon before the core has proven users.
2. **libsql embedded is the local default, behind a trait.** Storage goes behind the
   `RetrievalStore` trait in `core`; the default implementation is **libsql** (Turso's SQLite fork,
   in-process, MIT-licensed) — a single engine providing DiskANN vector search, FTS5 for BM25, and
   relational metadata in one file. Qdrant server becomes the remote-mode adapter on the roadmap;
   **Qdrant Edge** (in-process, pre-GA ~0.6.x as of early 2026) is a watch-item
   ([06-roadmap.md](06-roadmap.md) §3). Hybrid fusion (RRF) is done in our code above the trait, not
   delegated ([04-search-pipeline.md](04-search-pipeline.md) §5). **Rejected:** Qdrant as local
   default — Qdrant has no embedded mode (server-only), which would force a daemon-always model and
   contradict §3.

## 3. Process model: embedded-first, daemon-optional

**Decision:** CLI and MCP link `core` directly and open the store **in-process** (libsql database \+
ONNX models). No daemon is required for any MVP function. A daemon (`localdb serve`) is optional;
when one is running, CLI and MCP become thin clients of its HTTP API.

- **Discovery:** a platform-appropriate sentinel at a well-known path in the data dir (a Unix domain
  socket on Unix; an exclusively-locked file on Windows) ([03-config.md](03-config.md) §4). Sentinel
  present **and** the daemon's HTTP API responsive → route through daemon; otherwise → embedded
  mode, removing the stale sentinel on the way. Presence alone is not enough: the daemon claims the
  sentinel before it binds TCP and writes `daemon.url`, so there is a window in which it is held by
  a daemon that cannot yet serve. Holding the sentinel exclusively is a separate guarantee — it is
  the single-daemon mutex, and it is why a second `localdb serve` fails with `daemon_running` rather
  than racing the first. No configuration needed for the common case. The daemon also records its
  actual client-reachable base URL in a sibling `daemon.url` file at startup (substituting loopback
  for an unspecified/wildcard bind, since that address isn't itself connectable) so discovery works
  for any configured `server.bind`/`server.port` ([05-surfaces.md](05-surfaces.md) §3), not just the
  default `127.0.0.1:7700`.
- **Concurrency model:** SQLite WAL and `busy_timeout=5000` is the sole concurrency primitive. No
  advisory file lock. Multi-process is the first-class topology. Multiple stdio MCP servers, a CLI
  session running `localdb index`, and an optional `localdb serve` daemon may all share one data
  directory as peers. The daemon is no longer special. SQLite admits one writer at a time.
  Concurrent writers serialise via `busy_timeout`. An exhausted busy-timeout maps to
  `Error::RuntimeStateLocked` (exit 4). Within a process, `store-libsql` realises WAL's
  concurrent-reader benefit directly: writes serialise on a single writer connection (transactions
  via `BEGIN IMMEDIATE`), while reads are served from a small pool of read-only connections that
  never queue behind a writer. Cross-process coordination is unchanged — WAL plus `busy_timeout`
  remains the sole primitive between separate OS processes.
- **Daemon-exclusive capabilities:** continuous file watching, scheduled URL refresh, the HTTP API
  and (later) web UI, background job queue. Embedded mode does one-shot equivalents (`localdb index`
  = scan now; no watching).

**Rationale:** `localdb search foo` must work seconds after install with nothing running — this is
the local-first promise. **Rejected:** daemon-always — heavier install, worse first-run;
pure-embedded with no daemon — loses watching, refresh, and the web surface for home-server mode.

## 4. Stores vs. backends

Two concepts, deliberately separated:

- A **store** (logical, `core`): a named knowledge base with identity (stable ID), a `visibility`
  field (`private` | `shared` — enum exists in MVP, only `private` is functional), ACL hooks (empty
  in MVP), its own sources, and its own indexing policy ([03-config.md](03-config.md) §2).
  **Multiple stores per instance from day one** — e.g. files vs. bookmarks vs. (later) email. Stores
  are the unit of sharing and federation ([VISION.md](../VISION.md)).
- A **backend** (physical): an implementation of `RetrievalStore` that holds a store's index. MVP:
  `libsql` (embedded, single engine with DiskANN vectors and FTS5). Roadmap: `qdrant` (remote
  server), possibly Qdrant Edge. A store declares its backend in config; default is `libsql`.

`RetrievalStore` (trait sketch — normative surface, not final signatures): upsert chunks (dense
vector + text for BM25 + metadata), delete by resource, dense search, BM25 search, metadata
filtering, store-level stats. Fusion happens above the trait in `core`.

**Resource replaces Document** as the logical content unit. A `Resource` is the ingested, identified
representation of a source item (file, URL, etc.); it carries blocks that are then chunked for
indexing. The term "document" is retired from the domain model in favor of "resource".

## 5. Federation-readiness constraints (design constraints only)

MVP implements none of the federation behavior in [VISION.md](../VISION.md), but every MVP component
must respect:

1. **Stable, content-addressed IDs** for resources and chunks
   ([02-domain-model.md](02-domain-model.md) §3) — IDs must be meaningful outside the node that
   minted them.
2. **Provenance on every chunk** (origin store, source, content hash, fetch time).
3. **Per-store visibility** modeled as an enum, never a boolean bolted on later.
4. **No assumption of a single store** anywhere in core, surfaces, or config.

## 6. Runtime & concurrency

**Decision:** async on **tokio** for all I/O and orchestration — but not literally everything.

- **Async:** the daemon (HTTP API, file watching, schedulers, job queue), the `RetrievalStore` and
  `Embedder` traits (their backends are inherently async: libsql's Rust API is async, hosted
  providers are HTTP), URL fetching, and surface plumbing.
- **Not async:** CPU-bound work — ONNX inference, extraction/parsing, chunking, blake3 hashing —
  runs on a blocking/rayon pool via `spawn_blocking`-style handoff, never on the async executor.
  Pure domain logic in `core` (ID derivation, fusion, policy hashing, config resolution) stays sync
  and runtime-agnostic; only the orchestration around it is async.
- **Embedded mode:** one-shot CLI/MCP commands spin up a tokio runtime per invocation; the cost is
  negligible against model load and index I/O.

**Rationale:** the daemon needs real concurrency (watchers + jobs + HTTP) regardless, and the
storage/embedding dependencies are async-native — one execution model everywhere beats a sync core
wrapped in adapter shims. **Rejected:** fully synchronous core with hand-rolled threads (fights the
storage backend's async API, reinvents the daemon's scheduling); async-everything including CPU work
(starves the executor during indexing).

## 7. Development practices: TDD and coverage gates

**Decision:** test-driven development is the **default mode** for all crates: write the failing test
first, then the implementation. Coverage gates, enforced in CI (e.g. `cargo llvm-cov`):

- **≥ 80%** line coverage for critical functions — search orchestration, fusion, chunking,
  extraction normalization, config resolution, ID derivation.
- **≥ 90%** for anything that **modifies data** — store upserts/deletes, index job execution,
  resource/chunk writes, config/state mutation, migrations.

Trait-based seams (`RetrievalStore`, `Embedder`) exist partly to make this practical. Core logic is
tested against a real tmpdir SqliteBackend. Adapter crates are tested against the real backend
(libsql tmpdir, ONNX tiny model) in integration tests. Every ticket in [PLAN.md](../PLAN.md) carries
test expectations. A ticket is not done below its gate.
