# PLAN — MVP Implementation Tickets

Execution plan for delegating the MVP slice (Phase 1 in [specs/06-roadmap.md](specs/06-roadmap.md))
to subagents. **DRY rule:** all design detail lives in the specs; a ticket is executable by
reading the ticket plus its `Spec:` references only. If a ticket seems to need design information
not in a referenced spec section, that is a spec bug — fix the spec, don't improvise in code.

**Process for every ticket (from [specs/01-architecture.md](specs/01-architecture.md) §7):**
TDD — failing test first. Coverage gates enforced per ticket: **≥80%** on critical functions,
**≥90%** on anything that modifies data (store writes, deletes, jobs, config/state mutation).
A ticket is not done below its gate.

## Waves

Tickets within a wave are mutually independent (parallel subagents, worktree isolation); each
wave depends only on prior waves.

| Wave | Tickets |
|---|---|
| 1 | T01 |
| 2 | T02 |
| 3 | T03, T04, T05, T06 |
| 4 | T07, T08 |
| 5 | T09, T10, T11 |
| 6 | T12 |

Dependency graph (acyclic): T01 ← T02 ← {T03, T04, T05, T06}; T07 ← {T03,T04,T05,T06};
T08 ← {T02,T05,T06}; T09 ← {T03,T07,T08}; T10 ← {T03,T08}; T11 ← {T03,T07,T08}; T12 ← {T09,T11}.

---

### T01 — Workspace scaffold & CI
**Scope:** Cargo workspace with empty crates `core`, `extract`, `store-lancedb`, `embed`,
`server`, `mcp`, `cli` and the `localdb` binary crate (subcommand skeleton that prints help);
CI running fmt, clippy (deny warnings), tests, and `cargo llvm-cov` with the coverage gates wired
as a check.
**Depends on:** —
**Spec:** [01](specs/01-architecture.md) §1 (crate table, invariant), §6 (runtime), §7 (gates).
**Acceptance:** `cargo build --workspace` and `cargo test --workspace` pass; `localdb --help`
lists the subcommands from [05](specs/05-surfaces.md) §2; CI fails a PR that drops below gates
(demonstrate with a dummy function + test).
**Agent notes:** touch only workspace plumbing; no domain types; pick axum/clap/tokio versions
here so later tickets inherit them.

### T02 — Core domain model & error taxonomy
**Scope:** in `core`: Store, Source, Document, Block, Chunk, Citation, IndexJob types; ULID and
content-addressed blake3 ID derivation; provenance struct; reserved `msg.*` meta-key validation;
the shared error enum.
**Depends on:** T01
**Spec:** [02](specs/02-domain-model.md) all sections; [05](specs/05-surfaces.md) §5 (error codes).
**Acceptance:** unit tests prove ID stability (same content ⇒ same ID; changed content/span ⇒
changed ID); citation serializes to the exact JSON shape of [02](specs/02-domain-model.md) §6;
error enum covers every code in [05](specs/05-surfaces.md) §5. ≥80% coverage; ID derivation
treated as critical.
**Agent notes:** `core` only; no I/O, no backend deps; serde for all public types.

### T03 — Config: schema, loader, runtime-state split
**Scope:** YAML loading with strict unknown-key rejection, path-precise validation errors,
`version: 1` handling, platform path resolution, env/flag overrides; runtime-state DB (e.g.
redb/sqlite) for runtime-owned objects; ownership resolution (YAML-owned vs runtime-owned) and
merged effective-config view; effective indexing-policy hash (`policy_version`).
**Depends on:** T02
**Spec:** [03](specs/03-config.md) all sections; [04](specs/04-search-pipeline.md) §4
(policy versioning).
**Acceptance:** fixture configs (valid, typo'd key, bad duration, unversioned) produce the
specified outcomes; YAML-owned object mutation returns `config_readonly`; policy hash changes
iff effective `{chunking, embedding}` changes; YAML file bytes are never written by any code
path (test asserts file unchanged). ≥90% on runtime-state mutation; ≥80% on resolution.
**Agent notes:** lives in `core` (config module); platform paths via `directories` crate or
equivalent; do not implement file watching here (T11).

### T04 — Extraction crate
**Scope:** `extract`: format detection; Markdown, plain text, HTML (readability main-content),
text-layer PDF → normalized document text + Blocks (kind, span, heading_path, PDF page numbers);
`unsupported_format` signaling for everything else.
**Depends on:** T02
**Spec:** [04](specs/04-search-pipeline.md) §2; [02](specs/02-domain-model.md) (Block).
**Acceptance:** golden-file tests per format (fixtures in-repo); heading paths and spans correct
on nested-heading Markdown and HTML fixtures; a scanned-PDF fixture yields `unsupported_format`,
not garbage text; spans index into the normalized text exactly. ≥80%; normalization is critical.
**Agent notes:** `extract` crate only; no chunking (T07), no network (URL fetching is T07/T11) —
HTML extraction takes bytes in.

### T05 — `RetrievalStore` trait + LanceDB adapter
**Scope:** finalize the `RetrievalStore` trait in `core` (upsert chunks, delete by document,
dense search, BM25 search, metadata filters, stats) plus an in-memory fake for core tests;
implement `store-lancedb` against embedded LanceDB with tantivy BM25.
**Depends on:** T02
**Spec:** [01](specs/01-architecture.md) §4; [04](specs/04-search-pipeline.md) §5 (filter
pushdown expectations).
**Acceptance:** trait conformance test suite that runs against both fake and LanceDB (tmpdir);
upsert→search round-trip for dense and BM25 legs; delete-by-document removes all chunks;
metadata filters honored. ≥90% on upsert/delete paths; ≥80% elsewhere.
**Agent notes:** no fusion here (T08 owns RRF above the trait); keep LanceDB types out of `core`.

### T06 — Embedder trait + local ONNX + hosted providers
**Scope:** document-aware `Embedder` trait in `core` (nested chunks-per-document); `embed` crate:
local ONNX implementation (contextualized default model + bge-small-class fallback), model
download/cache with checksum + resume, OpenAI-compatible flat provider, Perplexity/Voyage
contextualized providers; timeout/retry/batching policy for hosted providers (not spec-constrained:
pick sensible defaults and document them in the crate).
**Depends on:** T02
**Spec:** [04](specs/04-search-pipeline.md) §4; [03](specs/03-config.md) §1 (provider config), §6
(secrets via env).
**Acceptance:** trait tests with a deterministic fake embedder; ONNX path tested with a tiny real
model in CI (vector dims and determinism asserted); hosted providers tested against a mock HTTP
server incl. retry/timeout; `model_missing` raised with actionable message when cache is empty
and downloads disabled. ≥80%; download/cache writes ≥90%.
**Agent notes:** also deliver the benchmark harness from [04](specs/04-search-pipeline.md) §4
(gating benchmark) as a `cargo bench`/example — running it on target hardware is a human step,
not CI.

### T07 — Ingestion pipeline
**Scope:** in `core`: scan-and-index orchestration — enumerate `path` sources (globs), fetch
`url` sources (conditional GET), extract (T04), chunk per preset (`prose`, `code`; `messages`
reserved), embed (T06), upsert (T05); content-hash incremental skip; replace-by-URI on change;
deletes; IndexJob lifecycle and stats; `policy_version` stamping and staleness detection.
**Depends on:** T03, T04, T05, T06
**Spec:** [04](specs/04-search-pipeline.md) §1, §3, §4 (policy versioning);
[02](specs/02-domain-model.md) (IndexJob, §3 consequence).
**Acceptance:** integration test over a fixture tree (fake embedder + LanceDB tmpdir): initial
index, no-op re-run (0 writes), edit ⇒ replace, delete ⇒ chunk removal, unsupported file counted
in stats; policy change ⇒ store marked stale ⇒ reindex rewrites all chunks; job states/stats
correct throughout. **≥90% across this ticket** (it is all data-modifying).
**Agent notes:** one-shot semantics only — no file watching, no scheduler, no daemon (T11);
chunkers implemented here per [04](specs/04-search-pipeline.md) §3 defaults.

### T08 — Hybrid search & citations
**Scope:** in `core`: query orchestration — BM25 leg + dense leg (query embedding via T06),
RRF fusion (k=60, K=50 per leg), multi-store fan-out with global fusion, metadata/store filters,
result shaping to Citation objects with per-leg scores; rerank seam left as a no-op stage.
**Depends on:** T02, T05, T06
**Spec:** [04](specs/04-search-pipeline.md) §5; [02](specs/02-domain-model.md) §6.
**Acceptance:** RRF unit tests against hand-computed fixtures (incl. ties and single-leg-only
hits); multi-store fan-out test (fake store) proves global ordering; citations carry correct
span/heading_path/uri from fixture data; relevance smoke test on the fixture corpus (known query
⇒ known doc in top 3). ≥80%; fusion is critical.
**Agent notes:** read-only — independent of T07; seed stores directly through the
`RetrievalStore` fake/adapter in tests.

### T09 — CLI
**Scope:** `cli` + binary wiring: `init`, `status`, `store add/list/remove`,
`source add/list/remove`, `index`, `search`, with `--json`, `--store`, `--config`; daemon probe →
embedded vs thin-client routing per command; write-lock acquisition in embedded writes; exit-code
mapping; first-run model-download prompt in `init`.
**Depends on:** T03, T07, T08
**Spec:** [05](specs/05-surfaces.md) §1, §2, §5 (exit codes); [01](specs/01-architecture.md) §3.
**Acceptance:** end-to-end test (assert_cmd-style, no daemon): init → store add → source add →
index → search returns citations; `--json` output matches canonical shapes; locked-store path
returns exit code 4 / `store_locked`; daemon-attached routing covered with a mock socket/HTTP
server. ≥80%; lock handling ≥90%.
**Agent notes:** `serve` and `mcp` subcommands exist but delegate to T11/T10 crates — stub them
behind a feature flag if those tickets are unmerged; no business logic in `cli` (invariant,
[01](specs/01-architecture.md) §1).

### T10 — MCP server
**Scope:** `mcp` crate: stdio MCP server exposing read-only `search`, `get_document`,
`list_stores`; structured citation output + text rendering; daemon probe → embedded vs
thin-client; `--allow-write` flag parsed but rejecting (reserved).
**Depends on:** T03, T08
**Spec:** [05](specs/05-surfaces.md) §1, §4; [02](specs/02-domain-model.md) §6.
**Acceptance:** protocol-level tests with a scripted MCP client over stdio: tool list exactly the
three read-only tools; `search` returns structured citations matching the canonical JSON;
unknown store name → `store_not_found` as MCP tool error; no mutating capability reachable. ≥80%.
**Agent notes:** read path only — does not need T07; use the official Rust MCP SDK if its tier
fits, else minimal stdio JSON-RPC.

### T11 — Daemon & HTTP API
**Scope:** `server` crate behind `localdb serve`: axum REST API per resource list; unix socket
for discovery; write lock held for the daemon's lifetime; job queue executing T07 pipelines;
file watching (`notify`, debounced) and URL refresh scheduling; YAML config watch-and-reload;
loopback-only default with refused non-loopback start when no auth; structured logging.
**Depends on:** T03, T07, T08
**Spec:** [05](specs/05-surfaces.md) §3, §5; [01](specs/01-architecture.md) §3;
[03](specs/03-config.md) §3 (reload semantics), §4 (socket/lock paths).
**Acceptance:** API integration tests: store/source CRUD (runtime-owned), `config_readonly` for
YAML-owned, `POST /jobs` → poll to `done`, `/search` returns citations, pagination cursors work;
watcher test: file change ⇒ re-index ⇒ search reflects it; non-loopback bind without auth exits
nonzero with clear error; second daemon on same data dir fails with `daemon_running`. Job/CRUD
write paths ≥90%; handlers ≥80%.
**Agent notes:** largest ticket — if splitting, cut between "API + socket + lock" and
"watch/refresh schedulers"; SSE explicitly out (roadmap).

### T12 — Packaging & release
**Scope:** release workflow producing macOS arm64 + Linux x86_64/arm64 tarballs; `cargo install`
path verified; versioned `--version`; README install instructions; smoke script: install → init →
index fixture → search on a clean machine/container.
**Depends on:** T09, T11
**Spec:** [06](specs/06-roadmap.md) §4.
**Acceptance:** CI release job produces all three artifacts from a tag; smoke script passes in a
clean Linux container; binary has no dynamic deps beyond platform baseline (check with
otool/ldd). Coverage gates N/A (no product code); smoke script is the test.
**Agent notes:** Homebrew/launchd/systemd are Phase ≥2 ([06](specs/06-roadmap.md) §4) — do not add.

---

## Later (post-MVP — see [specs/06-roadmap.md](specs/06-roadmap.md))

Not ticketed here: web UI; SSE job streaming; Qdrant server adapter; message connectors
(`imap`/`mbox`); shared stores + OIDC/OAuth2; federation; reranking; OCR/DOCX extraction;
keychain secrets; entities/graph; metrics/tracing.
