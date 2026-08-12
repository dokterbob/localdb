# CLAUDE.md — localdb contributor reference

## Build / Test / Lint

```sh
cargo build --workspace
cargo test --workspace
cargo test -p localdb-core          # single crate
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo llvm-cov --workspace --lcov --output-path lcov.info
cargo llvm-cov report --summary-only
```

All five commands run in CI (`.github/workflows/ci.yml`).
`cargo llvm-cov` requires the `llvm-tools-preview` component and `cargo-llvm-cov` installed.

**Coverage gates:** workspace line coverage must be ≥ 80%; data-modifying paths must be ≥ 90%.
Design rationale and enforcement detail: `specs/01-architecture.md §7`.
Default workflow is **TDD** — write the failing test first.

**Reclaim disk under pressure:** these caches are regenerable — delete them instead of `cargo clean`,
which also wipes the dependency cache and costs a full rebuild (~15 min for this workspace):
- `rm -rf target/debug/incremental` — incremental compilation cache; safe to delete any time.
- `rm -rf target/llvm-cov-target` — `cargo llvm-cov`'s separate instrumented build tree; delete after a coverage run.

## Crate map

| Crate | Role |
|---|---|
| `core` | Domain model, traits (`RetrievalStore`, `Embedder`), error taxonomy — no I/O frameworks |
| `cli` | Thin surface over `core`; `init`, `store`, `source`, `index`, `search` commands |
| `embed` | Embedder implementations: ONNX (local), OpenAI-compatible, Perplexity, Voyage |
| `extract` | Format detection and text extraction (Markdown, plain text, HTML, PDF → Markdown) |
| `fetch` | The `UrlFetcher` impl (reqwest). Two clients: `new()` unrestricted for operator-configured URLs, `new_public_only()` with the SSRF destination guard for URLs discovered in untrusted content (feed entry links) |
| `ingest` | Concrete `Ingestor` impls (`FileIngestor`, `UrlIngestor`, future connectors — Atom/RSS, Notion, Telegram, …); depends on `core` + `extract`; owns all acquisition I/O |
| `localdb` | Binary entry point; wires all subcommands |
| `mcp` | `rmcp`-based MCP server, stdio (embedded or daemon-proxied) and HTTP (`/mcp`); tools: `search`, `get_document`, `get_chunks`, `list_stores` |
| `server` | HTTP daemon (`/v1` axum routes), background jobs, file-watch, discovery-socket lifecycle |
| `store-libsql` | `RetrievalStore` impl: libsql (DiskANN vectors + FTS5 BM25); RRF fusion lives in `core`, not here |

**Design authority is `specs/`** — read the relevant spec before changing behavior; fix the spec first if it is wrong.

## Key specs

| File | Covers |
|---|---|
| `specs/01-architecture.md` | Layer invariants, process model, async model, coverage policy |
| `specs/02-domain-model.md` | Types, IDs, `Citation` shape |
| `specs/03-config.md` | YAML schema, path resolution |
| `specs/04-search-pipeline.md` | Chunking, embedding, RRF fusion |
| `specs/05-surfaces.md` | CLI subcommands, exit codes, HTTP routes, MCP tools |
| `specs/06-roadmap.md` | Planned features and milestones |

## Conventions

- **No domain logic in surface crates** (`cli`, `mcp`, `server`) — see `specs/01-architecture.md §1`.
- **Exit codes are stable API**: 0 ok, 1 internal, 2 invalid usage/config, 3 not found, 4 conflict/locked, 5 unavailable — see `specs/05-surfaces.md §5`. Do not add new codes without a spec change.
- **Async**: the project's async model is documented in `specs/01-architecture.md §6` — follow it for all new async code.
- **CLI uses real embeddings via config policy**: `cli` calls `embed::create_embedder` from the config; `FakeEmbedder` is only used in unit tests. The default embedder is `provider: local` (auto), `model: pplx-embed-context-v1-0.6b` — a context-aware late-chunking model (MIT-licensed, public HuggingFace repo `perplexity-ai/pplx-embed-context-v1-0.6b`). On macOS the `local` provider auto-selects CoreML (ANE/GPU) automatically — the macOS binary enables `embed`'s `local-coreml` feature by default via `cli/Cargo.toml`'s `[target.'cfg(target_os = "macos")'.dependencies]`, so no `--features` flag is needed. It falls back to ONNX otherwise; CoreML/ONNX vectors are index-interchangeable (force a backend with `local-coreml` / `local-onnx`). The first `localdb index` or `localdb search` triggers a one-time ~706 MB download; no API key or license click-through is required. Alternative local model: `model: bge-small-en-v1.5` (384-dim, much smaller). Hosted alternatives: `provider: perplexity` with `model: pplx-embed-context-v1` (requires API key), or `provider: openai-compatible`.
- **ONNX Runtime is loaded dynamically, never statically linked** (issue #133): `embed`'s `ort` dependency uses `load-dynamic`, and `embed/build.rs` embeds Microsoft's *official* ONNX Runtime build (pinned version, sha256-verified) for every `local-onnx` target — Linux x64, Linux aarch64, and macOS aarch64 alike. `embed::ort_runtime::ensure_ort_initialized` extracts it to the user's cache dir and calls `ort::init_from` before any other `ort` API is touched. Never re-enable `ort`'s `download-binaries` or any `api-*` default feature (`embed/Cargo.toml` pins `default-features = false` deliberately) — `download-binaries` reintroduces pyke.io's prebuilt archive, which gave release binaries a `GLIBC_2.38` floor and broke startup on glibc-2.35 distros (Mint 21, Ubuntu 22.04); see `docs/architecture.md` §"ONNX Runtime loading" and `pykeio/ort#523`.
- **HTTP daemon is experimental**: reads (`/v1/search`, `/v1/documents/{id}`, `/v1/status`) DO see CLI-indexed data — the daemon opens the same unified `localdb.db` as the CLI, not an in-memory store, and there is no separate write-lock (SQLite WAL + `busy_timeout=5000` serialise concurrent writers). `POST /v1/jobs` (ingestion) runs the real pipeline (`server/src/job_exec.rs`) through an async, single-worker queue with a per-store in-flight guard (issue #187) — a duplicate submission for a store already running gets `index_in_progress`, HTTP 409 / CLI exit 4. `localdb index` submits a job to the daemon and attaches to its live progress via SSE (`GET /v1/jobs/{id}/events`, issue #83, falling back to polling), rendering an identical summary/`--json`/`--strict` to embedded mode; `--delete` works daemon-attached too. **Stopping the daemon before running `localdb index` is no longer required.** Still experimental as a surface — no auth, and only one job runs at a time (job worker pool size N>1 is a follow-up). See `specs/05-surfaces.md §2-3` and `docs/architecture.md#known-gaps`.
- **Schema changes require a chain entry AND a `create_schema` fold-in**: every migration is written twice — once in `store-libsql/src/migrations/chain.rs`'s `migrations()`, once folded into `schema::create_schema` — the drift-guard test (`drift_guard_create_schema_equals_baseline_plus_chain`) fails otherwise. Migrations are explicit: `localdb db migrate` applies them; `open` never migrates on any surface. See `docs/migrations.md`.

## Commit style

Ticket branches use a `TXX:` prefix (e.g. `T12: add packaging & release workflow`).
Review commits on ticket branches use `TXX review: …`.
Merge commits: `Merge ticket/tXX (wave N)`.
Plain imperative for standalone fixes (e.g. `Wire serve and mcp subcommands to their crate implementations`).

## Known gaps (v0.1.0)

See `docs/architecture.md#known-gaps` for the full list.
