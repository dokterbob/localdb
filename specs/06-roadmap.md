# Spec 06 — Roadmap & Federation Direction

> Status: accepted draft, 2026-06-10. Phases are ordered; dates are deliberately absent. Everything
> beyond Phase 1 is direction, revisable as usage teaches us.

## 1. Phase ordering

| Phase                          | Scope                                                                                                                                                                                                                                                                                                                                        | Notes                                                                             |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| **1 — MVP**                    | Files + URLs → hybrid search (BM25+dense, RRF) with citations, via **CLI + MCP**; multiple stores; embedded-first with optional daemon + HTTP API                                                                                                                                                                                            | Complete.                                                                         |
| **1.5 — Ingestor framework**   | Resource/Block/Ingestor architecture: typed blocks replace Markdown IR, `Ingestor` trait, `messages` chunker, schema replacement, context expansion queries                                                                                                                                                                                  | Current work. See [07-adr-blocks-canonical-ir.md](07-adr-blocks-canonical-ir.md). |
| 2 — Connectors                 | Individual ingestors: Notion, Telegram, Signal, email (mbox/EML), transcription (SRT/VTT/Whisper), HackMD, Obsidian enhancements. **Atom/RSS: shipped** (#116) — `feed-rs`-based `Feed` ingestor, discovery + single-document modes ([02-domain-model.md](02-domain-model.md) §2). Each remaining connector tracked as its own GitHub issue. | Builds on the Ingestor trait from Phase 1.5.                                      |
| 3 — Web UI                     | Search/browse + admin on the existing HTTP API. SSE job progress: **shipped** (#83) — `GET /v1/jobs/{id}/events`, consumed by the CLI's daemon-routed job attach (`cli/src/job_attach.rs`); ahead of this phase's own UI work.                                                                                                               | First management GUI.                                                             |
| 4 — Remote store / home-server | Qdrant **server** adapter behind `RetrievalStore`; headless Linux mode; reverse-proxy guidance                                                                                                                                                                                                                                               | "Laptop → home server without rethinking the stack."                              |
| 5 — Shared stores              | `visibility: shared` becomes functional; **mature auth only** — OIDC/OAuth2 for client-server sharing; per-store ACLs                                                                                                                                                                                                                        | No homegrown crypto.                                                              |
| 6 — Federation                 | Propagating shares through the social graph                                                                                                                                                                                                                                                                                                  | §3 below.                                                                         |

Note: Phase 4 (Message connectors) from the old roadmap is now split: the framework is Phase 1.5
(current), individual connectors are Phase 2.

Also tracked but unscheduled: entities/graph layer (metadata-only entities first; graph extraction
only after baseline retrieval quality is proven).

## 2. Federation requirements (summary of [VISION.md](../VISION.md))

When Phase 6 opens, the design must satisfy:

- **Credential/capability propagation with direct connections.** Shares carry store references +
  introductions; the recipient obtains its own credential from each store's **origin** and connects
  directly. **No content relay through intermediaries** — an offline friend must never break access
  to a third party's store.
- **Provenance & trust metadata** on every chunk, including the share-path (who shared what, via
  whom) — fields reserved in [02-domain-model.md](02-domain-model.md) §4.
- **Peer discovery and connectivity** across NATs.
- **Mature auth/crypto only.**

Candidate substrates to evaluate then — research pointers, no commitment: **iroh** (direct
connections, tickets), **UCAN** (delegated capability tokens), **Willow protocol**, **ATProto**,
**Matrix**. Evaluation criteria: maturity/audit status, Rust support, fit for
capability-delegation-without-relay.

## 3. Turso watch-item

Turso is the future embedded direction for libsql — the hosted/sync layer on top of the same engine
already in use. It becomes a candidate to join — not necessarily replace — the current embedded-only
libsql setup when **all** hold:

1. The Turso feature set relevant to localdb (e.g. vector sync, multi-db routing) reaches production
   stability.
2. Embedded ↔ Turso server sync story is documented and working from the **Rust** crate.
3. Licensing and self-hosting story is confirmed compatible with the project's permissive stack.

## 4. Packaging roadmap

MVP: `cargo install` + GitHub release tarballs (macOS arm64, Linux x86_64/arm64). Then: **Homebrew**
formula with `brew services` (launchd) for the daemon; **systemd** unit for Linux; web UI assets
embedded in the binary at Phase 3. Model files are never bundled
([04-search-pipeline.md](04-search-pipeline.md) §4).

## 5. Consolidated "later" list

Deferred items referenced by other specs, in one place: reranking stage; original-file line mapping
for citations; OCR / scanned PDFs; additional ebook formats (see below); OS keychain secret storage;
interactive CLI browse; gRPC (if demanded); entities/graph; backup/export/import strategy (with
Phase 4); metrics/tracing endpoints (structured logs ship in MVP; Prometheus metrics arrive with the
daemon-centric Phase 4); per-format native block extraction (beyond `markdown_to_blocks()`
conversion); per-host max-concurrency alongside `fetch::http::HostLimiter`'s rate pacing (a no-op
today, since ingestion is fully sequential — would need a companion semaphore, and only earns its
keep once ingestion goes concurrent); a per-host circuit breaker to short-circuit a feed once every
entry is coming back rate-limited, rather than paying the per-entry retry cost for each one; the
scheduler's multi-source starvation, where several URL sources in one store coming due in the same
60 s tick means only the first wins the per-store in-flight guard and the rest fail submission and
are never stamped `last_refreshed` (that happens only on completion), so they retry next tick and
refresh less often than configured overall — pre-existing, amplified by per-host pacing making jobs
run longer, not fixed by this work; job cancellation (issue #218) — a `localdb job cancel` CLI
subcommand fronting a new `DELETE /v1/jobs/{id}` HTTP route, neither of which exists yet (both would
become new entries in [05-surfaces.md](05-surfaces.md) §2/§3); emitting a progress event before the fetch
rather than only after it returns, so a slow document is visible as in-flight instead of showing
nothing until `DocumentStarted`/`DocumentFinished` land back-to-back; a job-duration watchdog, now
that per-host pacing and retry make "slow but fine" and "actually stuck" harder to tell apart from
the outside; and sub-1-req/s pacing via `governor`'s `Quota::with_period`, should the integer-only
`requests_per_second` ever prove too coarse.

### Document & ebook formats

**Shipped:** Markdown, plain text, HTML, text-layer PDF, Office (DOCX/PPTX/XLSX/XLS/CSV via
`anytomd`), and **EPUB** (EPUB 2 & 3 via the pure-Rust `rbook` crate; OPF Dublin Core maps 1:1 onto
`DocumentMetadata`). See [04-search-pipeline.md](04-search-pipeline.md) §2.

**Deferred ebook formats:**

- **MOBI / AZW / AZW3** — PalmDOC/KF8 compression and frequent DRM mean these realistically require
  shelling out to Calibre; the only Rust crate (`mobi`) is stale (Dec 2022). Not worth a dependency
  now.
- **FB2 / CBZ** — on `rbook`'s roadmap but not yet implemented; a clean on-ramp when `rbook` ships
  them.
- **Kreuzberg** (broad-surface extractor) — capable but **Elastic License 2.0** (source-available,
  not OSI-permissive); the project stays on its permissive (Apache/MIT) small-crate stack. Revisit
  only if its license changes.
