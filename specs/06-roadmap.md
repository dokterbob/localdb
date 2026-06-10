# Spec 06 — Roadmap & Federation Direction

> Status: accepted draft, 2026-06-10. Phases are ordered; dates are deliberately absent.
> Everything beyond Phase 1 is direction, revisable as usage teaches us.

## 1. Phase ordering

| Phase | Scope | Notes |
|---|---|---|
| **1 — MVP** | Files + URLs → hybrid search (BM25+dense, RRF) with citations, via **CLI + MCP**; multiple stores; embedded-first with optional daemon + HTTP API | The whole of [PLAN.md](../PLAN.md). |
| 2 — Web UI | Search/browse + admin on the existing HTTP API; SSE job progress | First management GUI; config edits via API (runtime-owned objects only, [03-config.md](03-config.md) §3). |
| 3 — Remote store / home-server | Qdrant **server** adapter behind `RetrievalStore`; headless Linux mode; reverse-proxy guidance | "Laptop → home server without rethinking the stack." |
| 4 — Message connectors | Email first (`imap`, `mbox`), then messenger/chat connectors; `messages` chunking preset goes live | Builds on the reserved mapping in [02-domain-model.md](02-domain-model.md) §5. |
| 5 — Shared stores | `visibility: shared` becomes functional; **mature auth only** — OIDC/OAuth2 for client-server sharing; per-store ACLs | No homegrown crypto ([VISION.md](../VISION.md)). |
| 6 — Federation | Propagating shares through the social graph | §3 below. |

Also tracked but unscheduled: entities/graph layer (metadata-only entities first; graph extraction
only after baseline retrieval quality is proven — scope doc §13).

## 2. Deviation ledger (vs. `rust_backend_scope.md`)

1. Web UI moved from "first surface" to Phase 2 — rationale in [01-architecture.md](01-architecture.md) §2.
2. Qdrant moved from "local default" to Phase-3 remote adapter; LanceDB embedded is the local
   default behind the `RetrievalStore` trait — rationale in [01-architecture.md](01-architecture.md) §2.

## 3. Federation requirements (summary of [VISION.md](../VISION.md))

When Phase 6 opens, the design must satisfy:

- **Credential/capability propagation with direct connections.** Shares carry store references +
  introductions; the recipient obtains its own credential from each store's **origin** and
  connects directly. **No content relay through intermediaries** — an offline friend must never
  break access to a third party's store.
- **Provenance & trust metadata** on every chunk, including the share-path (who shared what, via
  whom) — fields reserved in [02-domain-model.md](02-domain-model.md) §4.
- **Peer discovery and connectivity** across NATs.
- **Mature auth/crypto only.**

Candidate substrates to evaluate then — research pointers, no commitment: **iroh** (direct
connections, tickets), **UCAN** (delegated capability tokens), **Willow protocol**, **ATProto**,
**Matrix**. Evaluation criteria: maturity/audit status, Rust support, fit for
capability-delegation-without-relay.

## 4. Qdrant Edge watch-item

Qdrant Edge (in-process Qdrant) was pre-GA (~0.6.x, unstable) as of early 2026. It becomes a
candidate to join — not necessarily replace — LanceDB as an embedded backend when **all** hold:

1. GA / 1.0 release with a stability commitment.
2. Hybrid (dense + sparse) query API confirmed working from the **Rust** crate.
3. Server-sync story (Edge ↔ Qdrant server) documented, since that pairs with Phase 3.

## 5. Packaging roadmap

MVP: `cargo install` + GitHub release tarballs (macOS arm64, Linux x86_64/arm64).
Then: **Homebrew** formula with `brew services` (launchd) for the daemon; **systemd** unit for
Linux; web UI assets embedded in the binary at Phase 2. Model files are never bundled
([04-search-pipeline.md](04-search-pipeline.md) §4).

## 6. Consolidated "later" list

Deferred items referenced by other specs, in one place: reranking stage; SSE job streaming;
original-file line mapping for citations; OCR / scanned PDFs / DOCX-family extraction; OS keychain
secret storage; interactive CLI browse; gRPC (if demanded); entities/graph; metrics/tracing
endpoints (structured logs ship in MVP; Prometheus metrics arrive with the daemon-centric Phase 3).

## Appendix A — Traceability: `rust_backend_scope.md` open questions §1–§15

| § | Question | Resolution |
|---|---|---|
| 1 | Repo layout | Monorepo, one workspace, one binary — [01](01-architecture.md) §1. |
| 2 | Extraction strategy | v1 matrix + explicit exclusions — [04](04-search-pipeline.md) §2; OCR/DOCX deferred (§6 above). |
| 3 | Embedding defaults | Contextualized default + lightweight preset + gating benchmark; download-on-first-run — [04](04-search-pipeline.md) §4. Sparse leg = BM25 (tantivy), not a learned sparse model. Reranking post-MVP. YAML expression — [03](03-config.md) §1. |
| 4 | External provider contract | One OpenAI-compatible provider abstraction for embeddings; LLMs stay out of the core process; secrets via env — [04](04-search-pipeline.md) §4, [03](03-config.md) §6. Timeout/retry/batching detail: deferred to implementation tickets (T06). |
| 5 | Search semantics | RRF defaults, chunking presets, metadata filtering, rewriting/reranking out of core — [04](04-search-pipeline.md) §3–§5. |
| 6 | API design | REST canonical, no-auth-local, job model, pagination; SSE later — [05](05-surfaces.md) §3. |
| 7 | MCP design | Read-only v1 tool list, opt-in mutation later, citation representation — [05](05-surfaces.md) §4. |
| 8 | CLI scope | Embedded-or-thin-client per command; command-oriented — [05](05-surfaces.md) §2. |
| 9 | Web UI scope | Moved to Phase 2 (§1, §2 above); scope defined then. |
| 10 | Filesystem layout | Per-platform paths, model cache, logs, socket, lock — [03](03-config.md) §4. Backup/export: open item, Phase 3. |
| 11 | Deployment modes | Embedded-first local (MVP), home-server/headless (Phase 3), shared/reverse-proxied (Phase 5); remote Qdrant is Phase 3, not v1 — §1 above. |
| 12 | Packaging | §5 above. |
| 13 | Entities/graph | Unscheduled, after retrieval quality — §1 above. |
| 14 | Observability | Structured logs + IndexJob visibility + error taxonomy in MVP ([05](05-surfaces.md) §5); metrics/tracing deferred (§6 above). |
| 15 | Security model | Local trust assumption + refused non-loopback-without-auth ([05](05-surfaces.md) §3); remote auth Phase 5; MCP read-only default ([05](05-surfaces.md) §4); secrets [03](03-config.md) §6. Multi-user: single-user until Phase 5. |
