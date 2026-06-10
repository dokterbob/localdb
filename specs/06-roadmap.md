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
only after baseline retrieval quality is proven).

## 2. Federation requirements (summary of [VISION.md](../VISION.md))

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

## 3. Qdrant Edge watch-item

Qdrant Edge (in-process Qdrant) was pre-GA (~0.6.x, unstable) as of early 2026. It becomes a
candidate to join — not necessarily replace — LanceDB as an embedded backend when **all** hold:

1. GA / 1.0 release with a stability commitment.
2. Hybrid (dense + sparse) query API confirmed working from the **Rust** crate.
3. Server-sync story (Edge ↔ Qdrant server) documented, since that pairs with Phase 3.

## 4. Packaging roadmap

MVP: `cargo install` + GitHub release tarballs (macOS arm64, Linux x86_64/arm64).
Then: **Homebrew** formula with `brew services` (launchd) for the daemon; **systemd** unit for
Linux; web UI assets embedded in the binary at Phase 2. Model files are never bundled
([04-search-pipeline.md](04-search-pipeline.md) §4).

## 5. Consolidated "later" list

Deferred items referenced by other specs, in one place: reranking stage; SSE job streaming;
original-file line mapping for citations; OCR / scanned PDFs / DOCX-family extraction; OS keychain
secret storage; interactive CLI browse; gRPC (if demanded); entities/graph; backup/export/import
strategy (with Phase 3); metrics/tracing endpoints (structured logs ship in MVP; Prometheus
metrics arrive with the daemon-centric Phase 3).
