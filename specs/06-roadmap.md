# Spec 06 — Roadmap & Federation Direction

> Status: accepted draft, 2026-09-06. Phases are ordered; dates are deliberately absent. Everything
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

The formerly unscheduled entities/graph layer is now scheduled — see §2 (mid-term).

## 2. Capability roadmap (near / mid / long-term)

Orthogonal to the phase ordering above: the capability pillars the project grows along. Each is
stress-tested against two workload classes — large heterogeneous document collections
(public-records corpora, scanned archives, thousands of books) and agent-memory workloads (a
persistent knowledge layer an AI agent reads and, eventually, writes). Every pillar has a tracking
issue carrying the research and open design questions.

**Near-term**

- **First-class OCR** (#344) — scanned PDFs and images become indexable instead of being rejected by
  `is_scanned_pdf()`; candidate paths range from `pdf_oxide`'s dormant `ocr` feature through
  platform OCR (Apple Vision) to an owned PP-OCRv5 ONNX pipeline on the runtime infrastructure
  `embed` already ships.
- **Store sharing: federated access between localdb servers** (#345) — a user grants other localdb
  instances direct, credentialed access to stores they host; later, stores-of-stores as curated
  _reference_ lists (pointers plus introductions, never relayed content). This pulls the Phase 5
  direction into a scoped ticket; the §3 requirements below constrain the design. This is access
  federation, **not** import/export — backup/export/import remains a separate later item (§6).
- **Identical and near-identical document grouping** (#346) — exact grouping via content hash
  (builds on #95), "soft dedup" of re-scans/re-OCRs/redaction variants via vector proximity.
- **Agent / second-brain memory over MCP** — the read side works today and is documented as a recipe
  ([docs/agent-memory.md](../docs/agent-memory.md)); the MCP write path is tracked as (#349).

**Mid-term**

- **Knowledge-graph layer** (#347) — entity/relationship extraction with a local small-model default
  and hosted opt-in (mirroring the embedder provider pattern), SQLite-first graph storage, and
  graph-derived candidates as a third RRF leg. The previous ordering rationale is preserved:
  metadata-only entities first; graph extraction only after baseline retrieval quality is proven.
- **Entity resolution & provenance** (#348) — explicit, evidence-carrying
  `same_as`/`possible_same_as`/`not_same_as` assertions, no silent merges; depends on #347.

**Long-term**

- **Audio/video transcription** — transcript formats (SRT/VTT/Whisper output) are already a Phase 2
  connector target (§1); the long-term item is the transcription pipeline itself.
- **Scale-out** beyond the single-node embedded design. The current architecture already handles
  multi-thousand-book collections well; corpora in the millions of pages will eventually stress
  single-node indexing and storage.

## 3. Federation requirements

[The project vision](../docs/vision.md) (published on the docs site) tells the sharing story in
plain language; this section holds the technical version of it, and is what Phase 6 designs are
judged against.

**Sharing propagates; content does not relay.** The defining constraint is **direct connections, no
indirection**. When Alice shares a list of stores with Bob — some hosted on her node, some that were
shared _to_ her from Carol — Bob's node does not pull Carol's content through Alice. Alice's server
may be a laptop that is asleep. Instead:

1. Alice's share is a list of **store references + introductions**: for each store, where its origin
   lives and a capability (or a way to request one) to access it.
2. Bob's node contacts each remote store's **origin** (Carol's node) directly, presents the
   delegated introduction, and requests its own credential.
3. From then on Bob ↔ Carol is a **direct connection**. Alice being offline never breaks Bob's
   access to Carol's store.

What propagates through the social graph is **credentials and capabilities, not proxied traffic** —
capability handoff in the OCAP/delegation tradition.

**Mature authentication, or none.** Shared stores use mature, audited authentication mechanisms —
OIDC/OAuth2 for client-server sharing, established capability-token systems for peer delegation.
**No homegrown crypto, ever.** If a sharing feature would require inventing a protocol, the feature
waits.

The full requirements list:

- **Credential/capability propagation with direct connections**, as above. **No content relay
  through intermediaries** — an offline friend must never break access to a third party's store.
- **Provenance & trust metadata** on every chunk, including the share-path (who shared what, via
  whom) — fields reserved in [02-domain-model.md](02-domain-model.md) §4. Trust signals are metadata
  the user can filter and rank on, never a black-box score.
- **Peer discovery and connectivity** across NATs.
- **Mature auth/crypto only.**

Candidate substrates to evaluate then — research pointers, no commitment: **iroh** (direct
connections, tickets), **UCAN** (delegated capability tokens), **Willow protocol**, **ATProto**,
**Matrix**. Evaluation criteria: maturity/audit status, Rust support, fit for
capability-delegation-without-relay.

**What the MVP carries for this.** The MVP ships none of the federation behavior; it carries exactly
four architectural hooks, each cheap now and expensive to retrofit
([01-architecture.md](01-architecture.md) §5):

| Hook                                                                                | Where specified                          |
| ----------------------------------------------------------------------------------- | ---------------------------------------- |
| Stable, content-addressed document/chunk IDs                                        | [02-domain-model.md](02-domain-model.md) |
| Provenance metadata on every chunk                                                  | [02-domain-model.md](02-domain-model.md) |
| Per-store visibility enum (`private` \| `shared`; only `private` functional in MVP) | [01-architecture.md](01-architecture.md) |
| Store as first-class entity (multiple stores per instance)                          | [01-architecture.md](01-architecture.md) |

## 4. Turso watch-item

Turso is the future embedded direction for libsql — the hosted/sync layer on top of the same engine
already in use. It becomes a candidate to join — not necessarily replace — the current embedded-only
libsql setup when **all** hold:

1. The Turso feature set relevant to localdb (e.g. vector sync, multi-db routing) reaches production
   stability.
2. Embedded ↔ Turso server sync story is documented and working from the **Rust** crate.
3. Licensing and self-hosting story is confirmed compatible with the project's permissive stack.

## 5. Packaging roadmap

MVP: `cargo install` + GitHub release tarballs (macOS arm64, Linux x86_64/arm64). **Delivered since
v0.1.0:** the dist release pipeline (tarballs + shell installer + attestations) and the **Homebrew**
tap (`brew install dokterbob/localdb/localdb`) with shell completions and `brew services` for the
daemon (launchd on macOS, systemd under brew on Linux) — see `docs/release-engineering.md`. Still
ahead: a bare **systemd** unit for non-brew Linux installs; web UI assets embedded in the binary at
Phase 3. Model files are never bundled ([04-search-pipeline.md](04-search-pipeline.md) §4).

## 6. Consolidated "later" list

Deferred items referenced by other specs, in one place (OCR and the entities/graph layer have moved
to the §2 capability roadmap): reranking stage; original-file line mapping for citations; additional
ebook formats (see below); OS keychain secret storage; interactive CLI browse; gRPC (if demanded);
backup/export/import strategy (with Phase 4); metrics/tracing endpoints (structured logs ship in
MVP; Prometheus metrics arrive with the daemon-centric Phase 4); per-format native block extraction
(beyond `markdown_to_blocks()` conversion); per-host max-concurrency alongside
`fetch::http::HostLimiter`'s rate pacing (a no-op today, since ingestion is fully sequential — would
need a companion semaphore, and only earns its keep once ingestion goes concurrent); a per-host
circuit breaker to short-circuit a feed once every entry is coming back rate-limited, rather than
paying the per-entry retry cost for each one; the scheduler's multi-source starvation, where several
URL sources in one store coming due in the same 60 s tick means only the first wins the per-store
in-flight guard and the rest fail submission and are never stamped `last_refreshed` (that happens
only on completion), so they retry next tick and refresh less often than configured overall —
pre-existing, amplified by per-host pacing making jobs run longer, not fixed by this work; emitting
a progress event before the fetch rather than only after it returns, so a slow document is visible
as in-flight instead of showing nothing until `DocumentStarted`/`DocumentFinished` land
back-to-back; a job-duration watchdog, now that per-host pacing and retry make "slow but fine" and
"actually stuck" harder to tell apart from the outside; and sub-1-req/s pacing via `governor`'s
`Quota::with_period`, should the integer-only `requests_per_second` ever prove too coarse.

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
