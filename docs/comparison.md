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

- **A single, dependency-free binary.** localdb is a statically-typed Rust program: `cargo
  install` or download a tarball, and it runs. No Python interpreter, virtualenv, or `pip`
  resolution; no `node_modules`; no JVM; no Docker. This is a real, verifiable difference from
  most of the field below — Khoj, Basic Memory, PrivateGPT, and txtai are Python packages
  (`pip`/`uv`/venv required); Onyx's backend is Python and its frontend Node; AnythingLLM's
  stack is Node.js. Only GPT4All and Recoll match localdb here, and neither has hybrid search
  or an MCP server (see the table below).
- **No external services.** No Postgres, no vector-database server, no Redis, no Docker Compose
  stack. Contrast Onyx's genuine multi-service architecture (Postgres + OpenSearch + Redis +
  MinIO, ~13 containers via Docker Compose) or Khoj's internally-managed Postgres/pgvector
  process.
- **Throw (almost) anything at it.** The extraction pipeline handles Markdown, plain text,
  HTML, PDF (text-layer), Office documents (DOCX/PPTX/XLSX/XLS/CSV), and EPUB (2 & 3) —
  all parsed in-process by pure-Rust or embedded libraries, no shelling out to a converter
  binary or a separate Tika/JVM process. And this is a floor, not a ceiling: the domain model
  already has typed slots (`IngestorKind::Notion`, `Telegram`, `Signal`, `HackMd`, `Email`,
  `Transcription`, `Feed`) reserved for connectors landing next — see
  [specs/06-roadmap.md](../specs/06-roadmap.md) Phase 2 and "Where localdb is headed" below.
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
- **Structured, verifiable citations.** Every result carries a source URI, heading path, exact
  byte span, and content hash — not just a file name and a snippet.

No project surveyed below combines all of these. Each matches localdb on some axes and diverges
on others — that comparison, not a "we win" scorecard, is the point of this document.

## At a glance

A five-feature checklist across all nine projects (localdb included), each independently
verifiable against the sources cited in the per-project sections below:

| Project | Single binary, no runtime¹ | No external services² | Hybrid BM25+vector | Native MCP server | Structured citations³ |
|---|:---:|:---:|:---:|:---:|:---:|
| **localdb** | ✅ | ✅ | ✅ | ✅ | ✅ |
| GPT4All (LocalDocs) | ✅ | ✅ | ❌ | ❌ | ❌ |
| Khoj | ❌ | ⚠️ | ❌ | ❌ | ❌ |
| Basic Memory | ❌ | ✅ | ✅ | ✅ | ❌ |
| AnythingLLM | ⚠️ | ✅ | ❌ | ❌ | ❌ |
| Onyx | ❌ | ❌ | ✅ | ✅ | ❌ |
| PrivateGPT | ❌ | ⚠️ | ❌ | ❌ | ❌ |
| txtai | ❌ | ✅ | ✅ | ❌ | ❌ |
| Recoll | ✅ | ✅ | ❌ | ❌ | ❌ |

¹ No Python/Node/JVM interpreter or Docker required just to run the thing.
² No separately-running Postgres/Vespa/OpenSearch/Redis/MinIO etc. for single-user use.
³ Byte-span + content-hash citations, not just a filename-and-snippet reference.

`⚠️` = partial: AnythingLLM's desktop build is a self-contained installer but its Docker/source
path is a Node.js stack; Khoj as of 2026 can auto-start an embedded Postgres/pgvector process
(`USE_EMBEDDED_DB=true`), so no manual provisioning is needed, but it's still an internally-run
database server, not an embedded library; PrivateGPT embeds Qdrant locally but still needs an
externally-running OpenAI-compatible inference server (Ollama/vLLM/llama.cpp) for the LLM
itself.

This table is deliberately narrow — five axes, chosen because they're where localdb's design
differs most from the field. It is not a completeness ranking: Onyx, for instance, ships a far
larger *connector* library today than localdb does (see "Where localdb is behind").

## Comparison table

| Project | License | Deployment | Search | MCP | Citations | Maintenance |
|---|---|---|---|---|---|---|
| **[GPT4All](https://www.nomic.ai/gpt4all) LocalDocs** ([repo](https://github.com/nomic-ai/gpt4all)) | MIT | Compiled desktop installer (.exe/.dmg/.deb/AppImage), fully self-contained | Vector-only (on-device Nomic embeddings) | None | File + snippet in a Sources panel, no spans | **Stalled** — last commit 2025-05-27 (13+ months), last release v3.10.0 (2025-02-25); open "Is GPT4all dead?" issue |
| **[Khoj](https://khoj.dev)** ([repo](https://github.com/khoj-ai/khoj)) | AGPL-3.0 | `pip install khoj` or Docker Compose; can auto-start an embedded Postgres/pgvector process (`USE_EMBEDDED_DB=true`) | Vector-only (bi-encoder + cross-encoder rerank), no BM25 | Client-only (consumes external MCP tools in research mode); no MCP *server* — an open issue (#1364) requests one | Sources panel with file + excerpt, no spans/hashes | Very active (35.4k★, last commit 2026-06-24). Hosted "Khoj Cloud" was sunset 2026-04-15; the open-source project remains maintained |
| **[Basic Memory](https://basicmemory.com)** ([repo](https://github.com/basicmachines-co/basic-memory), [docs](https://docs.basicmemory.com)) | AGPL-3.0 | `uv tool install basic-memory` / `uvx` (Python 3.12+); "just files plus a local SQLite index, no servers required" | Hybrid full-text + vector (FastEmbed, SQLite) | Native MCP server — but a large read/write tool surface (write/edit/move/delete notes, knowledge graph, canvas) vs. localdb's 3 read-only tools | Note-level retrieval, no byte-span/content-hash | Very active (3.3k★, last push 2026-06-29, v0.22.1 released 2026-06-13) |
| [AnythingLLM](https://anythingllm.com) ([repo](https://github.com/Mintplex-Labs/anything-llm)) | MIT | Desktop installer (self-contained) or Docker; underlying stack is Node.js; LanceDB embedded by default | Vector-only; hybrid BM25 PR open/unmerged since ~June 2026 | Client-only, explicitly no Resources/Prompts/Sampling | Not a stated strength | Very active (62.4k★, last push 2026-07-01) |
| [Onyx](https://onyx.app) (formerly Danswer) ([repo](https://github.com/onyx-dot-app/onyx), [docs](https://docs.onyx.app/welcome)) | Open-core: CE MIT, EE proprietary | Multi-service stack via Docker Compose (~13 containers: Postgres, OpenSearch, Redis, MinIO, background workers, …) | Hybrid (OpenSearch-backed; replaced Vespa in v3.0.0, fully removed by v4.x) | Both client and server | Enterprise-oriented | Active (30.6k★, last commit 2026-07-01) |
| [PrivateGPT](https://docs.privategpt.dev) (zylon-ai) ([repo](https://github.com/zylon-ai/private-gpt)) | Apache-2.0 | Homebrew (macOS) or `uv tool install` (Python 3.11, pinned); Qdrant embedded locally, but needs an external OpenAI-compatible inference server (Ollama/vLLM/llama.cpp) | Vector-centric RAG | MCP *client* connectors ("PrivateGPT 1.0" relaunch: v1.0.0 2026-06-03 after ~2 quiet years, v1.0.1 2026-06-18) | Retrieval + citations as API features | Active again (57.3k★, last commit 2026-06-29) |
| [txtai](https://neuml.github.io/txtai/) (neuml) ([repo](https://github.com/neuml/txtai)) | Apache-2.0 | `pip install txtai` (Python 3.10+); Faiss/HNSW/SQLite embedded by default, no server required | Native hybrid BM25+dense scoring | No first-party MCP; only third-party wrapper is stale (14★, no commits since 2025-04) | Configurable, general-purpose | Active (12.7k★, last push 2026-06-22, v9.10.0 released 2026-06-04) |
| [Recoll](https://www.recoll.org) ([repo](https://framagit.org/medoc92/recoll)) | GPL | Native C++/Qt desktop app; distro packages, AppImage, Flatpak; no Python/Node runtime | Classical BM25/Xapian only, no vector/LLM | None | File path + excerpt | Active, mature (single long-time maintainer; last commit 2026-06-29) |

## Per-project narrative

**GPT4All LocalDocs.** A desktop chat app with a "LocalDocs" plugin that lets the bundled LLM
retrieve from a folder of documents via on-device (Nomic) embeddings. It overlaps with localdb
on "compiled, self-contained, no external services" — but it's vector-only (no BM25/hybrid), has
no MCP surface at all, and citations are a file name plus snippet with no byte spans. Format
support is narrower than it first appears: only `.txt`/`.md`/`.rst` are officially supported and
extensively tested; PDF works but is explicitly flagged by the maintainers as not extensively
tested, and DOCX requires manually whitelisting the extension with no guaranteed extraction
quality. Maintenance is the bigger concern for anyone evaluating it today: no commits since
2025-05-27 (13+ months), the last release was v3.10.0 (2025-02-25), and the project's own issue
tracker has an open "Is GPT4all dead?" thread.

**Khoj.** A self-hostable (or Docker-Compose-deployed) "AI second brain" with a much larger
surface than localdb — chat, scheduled research agents, image generation — with document search
as one feature among many. Search is vector-only (bi-encoder retrieval with a cross-encoder
rerank stage), no BM25. As of 2026 it can auto-start an embedded Postgres/pgvector process
(`USE_EMBEDDED_DB=true`), so single-user setups no longer require manually provisioning a
database server — but it's still running a full Postgres instance internally, not an embedded
library the way localdb's libsql or Basic Memory's SQLite are. Its MCP story is client-only: it
can *consume* external MCP tools inside its research mode, but does not expose its own document
store as an MCP server — a gap tracked in its own issue tracker (#1364). Khoj's hosted cloud
offering was sunset 2026-04-15 as the company pivoted commercially, but the open-source project
itself remains very actively maintained (35.4k★, commits within the week), making it the most
credible actively-maintained alternative for someone who wants a bundled chat experience rather
than a retrieval primitive.

**Basic Memory.** The closest architectural peer to localdb: local-first, a native MCP server,
and hybrid full-text + vector search (FastEmbed embeddings over SQLite), installed without any
external database ("just files plus a local SQLite index, no servers required"). Two divergences
matter. First, scope and trust boundary: its MCP tool surface is read-write — an agent can
create, edit, move, and delete notes and build a knowledge graph through it — where localdb's
three MCP tools are read-only. That's a deliberate tradeoff each project makes differently, not
a bug in either: Basic Memory is a notes app that happens to be agent-editable, localdb is a
retrieval layer that assumes something else owns writes. Second, and directly relevant to
[issue #38](https://github.com/dokterbob/localdb/issues/38)'s original question: **Basic
Memory is scoped strictly to Markdown.** Its source tree contains a Markdown parser
(`src/basic_memory/markdown/`) and exactly four chat-export importers (ChatGPT, Claude
conversations, Claude projects, a generic `memory.json` format) — there is no PDF, DOCX, or
other binary/office document ingestion in the codebase or its docs. localdb already ships PDF,
Office (DOCX/PPTX/XLSX/XLS/CSV), and EPUB extraction today, in-process, with connectors for
richer content (Notion, email, chat, transcription) planned next — see "Where localdb is
headed" below. Citations are note-level, without byte spans or content hashes. Actively
maintained (3.3k★, releases roughly weekly).

**AnythingLLM.** A desktop binary (self-contained installer) that can also scale into a
Node.js/Docker deployment for teams. It has genuinely broad document support — PDF, DOCX, PPTX,
XLSX, TXT, CSV, and more, all uploadable through drag-and-drop — and uses LanceDB as its default
embedded vector store, so single-user use needs no external database. Search is vector-only
today; a hybrid BM25 pull request has been open and unmerged since roughly June 2026. Its MCP
support is explicitly client-only — the project's own docs state it does not implement
Resources, Prompts, or Sampling — which is a narrower MCP story than localdb's server. Largest
and fastest-growing community of the projects surveyed (62.4k★).

**Onyx (formerly Danswer).** The most operationally heavy project in this list: a genuine
multi-service architecture — Postgres, OpenSearch, Redis, MinIO, and background workers via
Docker Compose (~13 containers) or Helm. (Onyx replaced Vespa with OpenSearch in v3.0.0 and
removed it fully by v4.x, but the multi-service shape is unchanged.) This is the sharpest
contrast with localdb's single-binary, no-external-services design — Onyx is built for
team/enterprise deployments, not a personal machine. It is open-core (community edition
MIT-licensed, enterprise features proprietary), has hybrid search backed by OpenSearch, and
supports MCP on both the client and server side. Worth calling out honestly: Onyx's *connector*
library (40+ integrations — Slack, Confluence, Google Drive, Jira, Notion, web crawling, and
more, alongside file-upload formats including PDF and Office documents) is far more mature than
localdb's today, where only local files and URLs are indexed. Actively maintained (30.6k★).

**PrivateGPT (zylon-ai).** Positions itself as a self-hosted API layer in front of local model
runtimes (Ollama, vLLM, llama.cpp) with RAG built in, installed via Homebrew or a pinned `uv
tool install`. Qdrant runs embedded locally (no separate vector-DB server), but the LLM itself
must come from an externally-running inference server — so it isn't fully self-contained
end-to-end the way localdb is. Went through roughly two quiet years and relaunched in June 2026
as "PrivateGPT 1.0" (v1.0.0 on 2026-06-03, v1.0.1 on 2026-06-18) with MCP *client* connectors —
again, consuming MCP tools rather than exposing retrieval as one. Active again after the lull
(57.3k★), but has less of a track record than Khoj or Basic Memory in its current form.

**txtai.** A general-purpose embeddings/search framework rather than a personal-knowledge
product — it has native hybrid BM25+dense scoring built in, which is architecturally close to
localdb's RRF fusion, and is Apache-2.0 licensed, installed via `pip install txtai` with
Faiss/HNSW/SQLite embedded by default (no server required). Broader document extraction (PDF,
DOCX, and more) is available through its optional Textractor pipeline, which wraps Apache
Tika — meaning that path pulls in a JVM dependency, a real contrast with localdb's in-process
Rust parsers. It has no first-party MCP server; the only community MCP wrapper is stale (14★,
no commits since April 2025). Actively maintained as a library/framework (12.7k★), but adopting
it as an MCP-facing personal search server would mean building the MCP layer yourself.

**Recoll.** A mature, GPL desktop full-text search tool built on Xapian, natively compiled
(C++/Qt), with no Python or Node runtime needed. Its official site moved to
[recoll.org](https://www.recoll.org) (from the old lesbonscomptes.com domain) in 2024. Its
format coverage is genuinely broad (PDF, Office documents, email, archives, and more) but relies
on external helper programs (`pdftotext`, `antiword`, etc.) installed separately on the system,
where localdb's parsers are compiled into the single binary with no companion tools to install.
No vector search, no LLM integration, no MCP — it's included here as the classical-IR baseline,
and a reminder that BM25-only search is a legitimate, well-understood choice: hybrid search adds
an embedding model and a vector index to maintain, which is not free.

## Where localdb is behind

In the interest of the same honesty as the README's [Honest status](../README.md#honest-status)
table:

- **v0.1.0 pre-release.** Several of the projects above (Khoj, Basic Memory, AnythingLLM, Onyx)
  have years of production use and large user bases; localdb does not yet.
- **Connector breadth.** Onyx alone ships 40+ SaaS/enterprise connectors today (Slack,
  Confluence, Notion, Google Drive, and more); localdb indexes local files and URLs only, with
  connectors for richer content types on the roadmap but not yet shipped (see below).
- **No GUI or chat interface.** localdb is CLI + MCP only. Anyone wanting a chat-first
  experience out of the box (GPT4All, Khoj, AnythingLLM) will find localdb requires an external
  agent or client to talk to. A web UI is planned (Phase 3 of
  [specs/06-roadmap.md](../specs/06-roadmap.md)) but CLI, MCP, and agents remain the primary
  surfaces for the foreseeable future.
- **Single-user, single-node.** There is no multi-tenant deployment story today, unlike Onyx.
- **MCP is read-only today.** Mutating tools (`add_source`, `reindex`, and similar) are
  specified as the next planned addition, gated behind an explicit `localdb mcp --allow-write`
  opt-in flag that will never be on by default (see
  [specs/05-surfaces.md §4](../specs/05-surfaces.md)) — but they are not shipped yet. There is
  deliberately no plan for in-place *edit* tools: an agent that needs to change indexed content
  can re-add the source and let incremental re-index pick up the change, which is a smaller,
  more auditable surface than exposing note-editing verbs the way Basic Memory does.
- **No knowledge graph / entity layer yet.** An entities/graph layer is tracked in
  [specs/06-roadmap.md](../specs/06-roadmap.md) ("tracked but unscheduled: entities/graph layer,
  metadata-only entities first, graph extraction only after baseline retrieval quality is
  proven") but is not designed or built. Basic Memory already has one.
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

Two nearer-term axes sit ahead of federation on the roadmap:

- **Ingest all the things.** Files and URLs are indexed today; the domain model already
  reserves typed slots for what's next (`IngestorKind::Notion`, `Telegram`, `Signal`, `HackMd`,
  `Email`, `Transcription`, `Feed` — see `core/src/block.rs`), tracked as Phase 2 in
  [specs/06-roadmap.md](../specs/06-roadmap.md). This would put localdb ahead of the
  content-scoped competitors (GPT4All, Basic Memory) on content coverage well before federation
  is in scope — though it's worth being honest that Onyx's connector library is already larger
  today (see "Where localdb is behind").
- **A knowledge graph, MCP write tools, and eventually a web UI.** An entities/graph layer,
  MCP tools for managing sources and stores (not editing content — see above), and a
  browse/search web UI (Phase 3) are all directional next steps, not committed dates. CLI, MCP,
  and agentic clients remain the primary way to use localdb in the meantime.
