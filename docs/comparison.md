# Comparison to Existing Projects

## Why this doc exists

[Issue #38](https://github.com/dokterbob/localdb/issues/38) asked how localdb differs from
tools people already use for personal knowledge search — GPT4All's LocalDocs feature was the
reporter's own reference point. This document answers that directly: what localdb's combination
of design choices actually is, how it compares against eight adjacent projects as of
2026-07-01, where it is behind, and where it is headed that nothing else surveyed does yet.

This is a snapshot, not a standing claim of superiority. Several of the projects below move
fast; re-check before citing a specific fact (star count, release date, feature status) more
than a few months out.

## What makes localdb's combination distinctive

localdb is a **search/retrieval primitive, not an app** — "do one thing well," in the Unix
sense. Its surface is deliberately narrow: index, search, cite. The CLI and MCP server expose
three read-only tools (`search`, `list_stores`, `get_document`); there is no note editor, chat
UI, or agent-orchestration layer built into the binary. That narrowness is a design choice, not
an omission: it keeps the retrieval API stable enough for other things to be built *on top* of
it. A project's or agent's own Markdown knowledge base can be pointed at with
`localdb source add` and kept current with incremental re-index (unchanged content is skipped
by content hash), so a second-brain UI or an agent's own scratchpad-memory layer can search
current state without owning the indexing pipeline itself. Most of the projects surveyed below
take the opposite bet: they bundle a chat interface, a notes editor, or an agent-orchestration
layer directly into the same binary as the retrieval engine, which is a reasonable product
choice but couples the retrieval API to that layer's roadmap.

Around that core choice, several other properties combine in a way no single surveyed project
matches all at once:

- **KISS / self-contained.** One binary, no external services — no Postgres, no vector-database
  server, no Redis, no Docker. Contrast Onyx's genuine multi-service stack (Postgres + Vespa +
  Redis + MinIO + Celery) or AnythingLLM's docker/cloud scale-up path.
- **Sensible defaults.** `localdb init` to a working `search` is four commands (see the
  [README quickstart](../README.md#60-second-quickstart)); the default embedding backend
  auto-selects CoreML on Apple Silicon and falls back to ONNX elsewhere; no API key is required
  to start.
- **Agent-first, not chat-first.** The CLI and MCP server are the primary surfaces, not a
  bolted-on integration layer. This has been validated in practice against multiple agentic
  clients — Codex, Claude Code, Claude Desktop, and Hermes Agent — and across both cloud model
  providers (Anthropic, OpenAI, DeepSeek) and a local one (Gemma, via ONNX/CoreML). Most
  competitors' MCP stories are client-only (they consume external MCP tools but don't expose
  one) or rely on a third-party wrapper that has gone stale.
- **Blazingly efficient.** Binary-quantized (1-bit) embedding vectors plus a DiskANN index, and
  native CoreML (ANE/GPU) or ONNX inference — no GPU and no cloud embedding call is required.
  Several vector-search competitors assume a heavier, separately-hosted embedding service.
- **Unbounded content, not just documents.** Files and URLs are indexed today; connectors for
  Notion, email, Telegram, Signal, HackMD, Obsidian, and audio transcription are planned (see
  [specs/06-roadmap.md](../specs/06-roadmap.md)). Several competitors are scoped specifically to
  "your notes" or "your vault" as a fixed content model.
- **Structured, verifiable citations.** Every result carries a source URI, heading path, exact
  byte span, and content hash — not just a file name and a snippet.

No project surveyed below combines all of these. Each matches localdb on some axes and diverges
on others — that comparison, not a "we win" scorecard, is the point of this document.

## Comparison table

| Project | License | Deployment | Search | MCP | Citations | Maintenance |
|---|---|---|---|---|---|---|
| **GPT4All LocalDocs** | MIT | Desktop app, local-first (now also supports remote model providers) | Vector-only (Nomic on-device embeddings) | None | File + snippet in a Sources panel, no spans | **Stalled** — no commits/releases in 13+ months (last release v3.10.0, 2025-02-25); open "Is GPT4all dead?" issue |
| **Khoj** | AGPL-3.0 | Self-hostable or cloud; `--anonymous-mode` for local-only | Vector-only (bi-encoder + cross-encoder rerank), no BM25 | Client-only (consumes external MCP tools in research mode); no MCP *server* — an open issue (#1364) requests one | Sources panel with file + excerpt, no spans/hashes | Very active (35.4k★, pushed within the week) |
| **Basic Memory** | AGPL-3.0 | Local-first, plain Markdown on disk; optional paid cloud sync | Hybrid full-text + vector (FastEmbed, SQLite) | Native MCP server — but a large read/write tool surface (write/edit/move/delete notes, knowledge graph, canvas) vs. localdb's 3 read-only tools | Note-level retrieval, no byte-span/content-hash | Very active (3.3k★, pushed days ago, weekly releases) |
| AnythingLLM | MIT | Desktop binary that scales into Docker/cloud | Vector-only; hybrid BM25 PR open/unmerged since ~June 2026 | Client-only, explicitly no Resources/Prompts/Sampling | Not a stated strength | Very active (58k★) |
| Onyx (formerly Danswer) | Open-core: CE MIT, EE proprietary | Genuine multi-service stack (Postgres + Vespa + Redis + MinIO + Celery via Docker Compose/Helm) | Hybrid (Vespa-backed) | Both client and server | Enterprise-oriented | Active |
| PrivateGPT (zylon-ai) | Apache-2.0 | Self-hosted API layer in front of Ollama/vLLM/llama.cpp | Vector-centric RAG | MCP *client* connectors (relaunched as "PrivateGPT 1.0" June 2026, Claude-Messages-API-shaped) | Retrieval + citations as API features | Active again after a 2025 lull |
| txtai | Apache-2.0 | Self-hosted framework + own HTTP API | Native hybrid BM25+dense scoring | No first-party MCP; only third-party wrapper is stale (14★, no commits since 2025-04) | Configurable, general-purpose | Active (12.7k★) |
| Recoll | GPL | Desktop app | Classical BM25/Xapian only, no vector/LLM | None | File path + excerpt | Active, mature |

## Per-project narrative

**GPT4All LocalDocs.** A desktop chat app with a "LocalDocs" plugin that lets the bundled LLM
retrieve from a folder of documents via on-device (Nomic) embeddings. It overlaps with localdb
on "local-first, no cloud required," but it's vector-only (no BM25/hybrid), has no MCP surface
at all, and citations are a file name plus snippet with no byte spans. Maintenance is the bigger
concern for anyone evaluating it today: no commits or releases in over 13 months, the last
release was v3.10.0 (2025-02-25), and the project's own issue tracker has an open "Is GPT4all
dead?" thread.

**Khoj.** A self-hostable (or cloud) "AI second brain" with a much larger surface than localdb —
chat, scheduled research agents, image generation — with document search as one feature among
many. Search is vector-only (bi-encoder retrieval with a cross-encoder rerank stage), no BM25.
Its MCP story is client-only: it can *consume* external MCP tools inside its research mode, but
does not expose its own document store as an MCP server — a gap tracked in its own issue
tracker (#1364). Development is very active (35.4k★, commits within the week), which makes it
the most credible actively-maintained alternative for someone who wants a bundled chat
experience rather than a retrieval primitive.

**Basic Memory.** The closest architectural peer to localdb: local-first, plain Markdown files
on disk as the source of truth, a native MCP server, and hybrid full-text + vector search
(FastEmbed embeddings over SQLite). The key divergence is scope and trust boundary: its MCP
tool surface is read-write — an agent can create, edit, move, and delete notes and build a
knowledge graph through it — where localdb's three MCP tools are read-only. That's a deliberate
tradeoff each project makes differently, not a bug in either: Basic Memory is a notes app that
happens to be agent-editable, localdb is a retrieval layer that assumes something else owns
writes. Citations are note-level, without byte spans or content hashes. Actively maintained
(3.3k★, weekly releases).

**AnythingLLM.** A desktop binary aimed at going from single-user to a Dockerized,
multi-workspace deployment. Search is vector-only today; a hybrid BM25 pull request has been
open and unmerged since roughly June 2026. Its MCP support is explicitly client-only — the
project's own docs state it does not implement Resources, Prompts, or Sampling — which is a
narrower MCP story than localdb's server. Largest community of the projects surveyed (58k★).

**Onyx (formerly Danswer).** The most operationally heavy project in this list: a genuine
multi-service architecture (Postgres, Vespa, Redis, MinIO, Celery workers) deployed via Docker
Compose or Helm. This is the sharpest contrast with localdb's single-binary, no-external-services
design — Onyx is built for team/enterprise deployments, not a personal machine. It is open-core
(community edition MIT-licensed, enterprise features proprietary), has hybrid search backed by
Vespa, and supports MCP on both the client and server side. Actively maintained.

**PrivateGPT (zylon-ai).** Positions itself as a self-hosted API layer in front of local model
runtimes (Ollama, vLLM, llama.cpp) with RAG built in. Went through a 2025 slowdown and relaunched
in June 2026 as "PrivateGPT 1.0" with a Claude-Messages-API-shaped interface and MCP *client*
connectors — again, consuming MCP tools rather than exposing retrieval as one. Active again
after the lull, but has less of a track record than Khoj or Basic Memory in its current form.

**txtai.** A general-purpose embeddings/search framework rather than a personal-knowledge
product — it has native hybrid BM25+dense scoring built in, which is architecturally close to
localdb's RRF fusion, and is Apache-2.0 licensed. It has no first-party MCP server; the only
community MCP wrapper is stale (14★, no commits since April 2025). Actively maintained as a
library/framework (12.7k★), but adopting it as an MCP-facing personal search server would mean
building the MCP layer yourself.

**Recoll.** A mature, GPL desktop full-text search tool built on Xapian. No vector search, no
LLM integration, no MCP — it's included here as the classical-IR baseline. It's a reminder that
BM25-only search is a legitimate, well-understood choice, and that hybrid search is not free:
it adds an embedding model and a vector index to maintain.

## Where localdb is behind

In the interest of the same honesty as the README's [Honest status](../README.md#honest-status)
table:

- **v0.1.0 pre-release.** Several of the projects above (Khoj, Basic Memory, AnythingLLM, Onyx)
  have years of production use and large user bases; localdb does not yet.
- **No GUI or chat interface.** localdb is CLI + MCP only. Anyone wanting a chat-first
  experience out of the box (GPT4All, Khoj, AnythingLLM) will find localdb requires an external
  agent or client to talk to.
- **Single-user, single-node.** There is no multi-tenant deployment story today, unlike Onyx.
- **No agent/tool orchestration beyond search.** localdb does not run scheduled research agents,
  summarization pipelines, or a knowledge graph — deliberately, per its "do one thing well"
  scope, but that means those capabilities have to come from whatever sits on top of it.
- **Ingestion via the HTTP daemon is a documented no-op.** True real-time (file-watch-triggered)
  re-indexing is the intended shape of the daemon, but isn't live yet: `server/src/watcher.rs`
  currently only watches for config-file changes in the running daemon, and `POST /v1/jobs` is a
  no-op (see the README's Honest status table and `core/src/ingestion.rs`, which notes the
  daemon "adds file watching" as a T11 follow-up). Today, keeping an index current means running
  `localdb index` again — incremental, but not automatic.

## Where localdb is headed that nobody else is

Per [VISION.md](../VISION.md), the long-horizon goal is **federation**: the ability to search
datasets far larger than any one person could assemble alone, by securely sharing direct,
credentialed access to stores — someone's curated book collection, a Wikipedia-scale corpus, a
friend's notes — without proxying content through intermediaries or inventing homegrown crypto.

**None of the eight projects surveyed above do this at all.** Each is scoped to a single user's
(or single organization's) own content. This is the one dimension of this comparison with no
current point of reference, and this document will need revisiting once federation ships.

The nearer-term forward-looking axis is the unbounded-content roadmap: connectors beyond files
and URLs (Notion, email, Telegram, Signal, HackMD, Obsidian, audio transcription), which would
put localdb ahead of the note-scoped competitors (GPT4All, Basic Memory) on content coverage
well before federation is in scope.
