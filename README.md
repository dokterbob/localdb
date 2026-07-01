# localdb

**localdb** is a personal knowledge server. Point it at your notes, bookmarks, specs, and
documentation — then search them instantly from the command line or let any MCP-capable AI
assistant retrieve cited, verifiable excerpts from your own corpus. Everything runs on your
machine: one binary, no cloud, no daemon required for search, no API key.

The long-horizon goal is larger: a private, trust-weighted alternative to the feed — your
knowledge enriched by what the people you trust have found, with provenance at every hop.
The foundation for that is built in from day one: content-addressed documents, per-chunk
provenance, and stores as first-class shareable units. See [VISION.md](VISION.md).

**Status: v0.1.0 pre-release.** Hybrid search uses real dense embeddings via the default local model (`pplx-embed-context-v1-0.6b`, ONNX on CPU by default; CoreML ANE/GPU on Apple Silicon macOS automatically); the first `localdb index` or `localdb search` downloads ~706 MB from HuggingFace (no API key required). The HTTP daemon reads from and writes to the same unified database as the CLI; ingestion via `POST /v1/jobs` is currently a no-op. See [Honest status](#honest-status) below.

**License:** [AGPL-3.0-or-later](LICENSE).

---

## Feature highlights

- **Citeable hybrid search** — BM25 + dense vector (RRF fusion) returning structured `Citation`
  objects: file URI, heading path, exact text snippet, byte span, content hash, per-component
  scores, and full document metadata. Every result is verifiable.
- **Document metadata** — `DocumentMetadata` (Dublin Core: title, creator, date, description, …)
  extracted from frontmatter and carried on every citation, so agents can attribute sources properly.
- **Local files and URLs** — `localdb source add ~/notes` or
  `localdb source add https://example.com/page`; incremental re-index skips unchanged content.
- **Embedded-first** — `localdb search` opens the store in-process; nothing needs to be running.
  The MCP server works the same way.
- **MCP server** — `localdb mcp` exposes three read-only tools (`search`, `list_stores`,
  `get_document`) to any MCP-capable AI assistant. Connect once, search forever.
- **Multiple stores** — each store is isolated; query one or all with `--store`.
- **Context-aware dense search** — the default embedder (`pplx-embed-context-v1-0.6b`) is a
  late-chunking model from Perplexity AI that encodes each chunk in the context of its full
  document, producing strong retrieval quality. Stored as binary-quantized 128-byte
  vectors (Hamming / IVF_FLAT), keeping index size small and search fast without a GPU.
  On Apple Silicon macOS, the binary runs the model on the Neural Engine / GPU via CoreML
  automatically — no `--features` flag is needed. The default `local` provider auto-selects
  CoreML at runtime and falls back to ONNX (CPU) otherwise; both produce
  index-interchangeable vectors. The model is a public MIT release, so no API key or
  license click-through is needed.
  Alternative: any OpenAI-compatible embedding endpoint, including local private models via
  llama.cpp or MLX (Apple Silicon, SSD-backed KV cache).
- **libsql backend**: embedded database with DiskANN vector index and FTS5 full-text search, no separate server.
- **`--json` everywhere** — machine-readable output on every command.
- **`localdb status`** — shows indexed stores and daemon state at a glance.

---

## Comparison to other tools

localdb is for personal knowledge search from the command line or from an AI assistant, with
no cloud dependency, no daemon required for search, and one binary to install. It's agent-first
rather than chat-first: the CLI and MCP server are the primary surfaces, validated in practice
against Codex, Claude Code, Claude Desktop, and Hermes Agent, using both cloud (Anthropic,
OpenAI, DeepSeek) and local (Gemma) model providers.

It is deliberately narrow — "do one thing well": a verifiable retrieval primitive (index,
search, cite), not an all-in-one chat app or team platform. That keeps its API stable enough
for other things to be built on top instead of bundled in — a second-brain UI, or an agent's
own live scratchpad search. The roadmap points toward unbounded content types (connectors
beyond files/URLs) and, eventually, federation — searching datasets shared by people you trust,
larger than any one person could assemble alone — which is an axis no surveyed competitor
addresses yet. See [docs/comparison.md](docs/comparison.md) for the full survey against eight
adjacent projects.

| Project | License | Search | MCP surface | Citations |
|---|---|---|---|---|
| **GPT4All (LocalDocs)** | MIT | Vector-only | None | File + snippet, no spans |
| **Khoj** | AGPL-3.0 | Vector-only | Client-only (no MCP server) | File + excerpt, no spans/hashes |
| **Basic Memory** | AGPL-3.0 | Hybrid full-text + vector | Native MCP server, read-write notes | Note-level, no byte-span/hash |

GPT4All is the most common comparison point (and appears effectively stalled — no commits or
releases in 13+ months); Khoj is the most popular actively-maintained self-hosted alternative;
Basic Memory is the closest architectural peer, trading localdb's read-only cited-corpus model
for read-write note editing over MCP.

---

## Install

### From source (works today)

Requires a Rust toolchain (**Linux: 1.82 or later; macOS: 1.85 or later**, as CoreML is
built automatically on macOS). Install via [rustup](https://rustup.rs/).

```bash
git clone https://github.com/dokterbob/localdb
cd localdb
cargo install --path localdb
localdb --version
```

On Apple Silicon macOS, CoreML (ANE/GPU) acceleration is built in automatically — no
`--features` flag is needed. The default `local` embedding provider selects CoreML at
runtime when available and falls back to ONNX (CPU) otherwise; indexes built by either
backend are queryable by the other.

### Pre-built tarballs

| Platform | Tarball suffix | Notes |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | CoreML (ANE/GPU) built in — auto-selected at runtime |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | ONNX CPU |
| Linux arm64 | `aarch64-unknown-linux-gnu` | ONNX CPU |

Download and install from the [Releases](https://github.com/dokterbob/localdb/releases) page:

```bash
# Replace VERSION and PLATFORM with your values from the table above
VERSION=0.1.0
PLATFORM=aarch64-apple-darwin   # or x86_64-unknown-linux-gnu / aarch64-unknown-linux-gnu
curl -L "https://github.com/dokterbob/localdb/releases/download/v${VERSION}/localdb-v${VERSION}-${PLATFORM}.tar.gz" \
  | tar -xz -C /usr/local/bin --strip-components=1 "localdb-v${VERSION}-${PLATFORM}/localdb"
localdb --version
```

See [docs/release-engineering.md](docs/release-engineering.md) for full pipeline details and how to cut a release.

---

## 60-second quickstart

```bash
# 1. Create a config file
localdb init

# 2. Create a store
localdb store add notes

# 3. Add sources — local directories and/or URLs
localdb source add ~/notes --store notes
localdb source add https://example.com/page --store notes   # optional

# 4. Index
localdb index --store notes

# 5. Check what got indexed
localdb status

# 6. Search
localdb search "how does rust handle errors" --store notes
```

Example output from step 6 (paths shown from a scratch run):

```
1. file:///private/tmp/.../notes/rust-error-handling.md > Error handling in Rust
   Error handling in Rust
Rust uses the Result type for recoverable errors and panic! for unrecoverable ones. The question-

2. file:///private/tmp/.../notes/meeting.txt
   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark c

3. file:///private/tmp/.../notes/lancedb-notes.md > LanceDB notes
   LanceDB notes
LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combi
```

Add `--json` to get structured `Citation` objects with chunk IDs, document IDs, provenance
hashes, per-component scores, and document `metadata` fields (title, creator, date, etc.):

```bash
localdb search "hybrid search" --store notes --json
```

---

## MCP hookup

```bash
claude mcp add localdb -- localdb mcp
```

This registers `localdb` as a local MCP server over stdio. Three read-only tools are exposed:
`search` (hybrid search returning Citation JSON), `list_stores` (store names, document counts,
chunk counts), and `get_document` (full document text and metadata by document ID).

Once connected, any MCP-capable AI assistant can call `search` against your indexed stores
and return cited excerpts with source URI, heading path, and document metadata — grounded
in actual passages from your files.

See [docs/mcp.md](docs/mcp.md) for full tool schemas and example calls.

---

## Experimental HTTP daemon

```bash
localdb serve   # binds http://127.0.0.1:7700 by default
```

The daemon exposes a REST API. It is **experimental**: ingestion via `POST /v1/jobs` is currently a no-op. The daemon reads and writes the same unified database as the CLI, so CLI-indexed data is visible to it. See [docs/http-api.md](docs/http-api.md) for endpoint reference and known limitations.

---

## Honest status

| Area | What is true today |
|---|---|
| Search ranking | Hybrid BM25 + dense (RRF fusion). Default embedder is `pplx-embed-context-v1-0.6b` (local ONNX, ~706 MB download on first use). |
| Embedding models | Downloaded automatically on first `localdb index` or `localdb search` from the public HuggingFace repo `perplexity-ai/pplx-embed-context-v1-0.6b`. No API key required. |
| Embedding backend | Default provider `local` runs ONNX on CPU. On Apple Silicon macOS (Rust ≥1.85), the macOS binary includes CoreML by default and auto-selects the ANE/GPU backend at runtime, falling back to ONNX otherwise. CoreML/ONNX indexes are interchangeable. Force a backend with `local-coreml` / `local-onnx`. |
| HTTP daemon | Experimental preview. Ingestion via POST /v1/jobs is a no-op; reads and writes the unified database. |
| YAML-declared stores | Appear in `store list` but **cannot be indexed** (`localdb index` only resolves runtime stores). Use `localdb store add` + `localdb source add` instead. |
| CLI while daemon runs | CLI and daemon can run concurrently. SQLite WAL and busy_timeout serialise concurrent writes. |

Docs sync: the old Known Gaps entries for source path validation and the macOS bundle ID are resolved in code and reflected in `docs/architecture.md`.

Design rationale and planned behavior live in the [specs/](specs/) directory.

---

## Documentation

| Document | Contents |
|---|---|
| [docs/install.md](docs/install.md) | Full install options, platform notes, shell completion |
| [docs/comparison.md](docs/comparison.md) | Comparison to GPT4All, Khoj, Basic Memory, and 5 other adjacent projects |
| [docs/release-engineering.md](docs/release-engineering.md) | Release pipeline, binary targets, MSRV, how to cut a release |
| [docs/quickstart.md](docs/quickstart.md) | Annotated end-to-end walkthrough with real output |
| [docs/configuration.md](docs/configuration.md) | YAML config schema, paths, store/source options |
| [docs/cli.md](docs/cli.md) | All commands and flags, exit codes, error messages |
| [docs/http-api.md](docs/http-api.md) | REST endpoint reference, request/response shapes, limitations |
| [docs/mcp.md](docs/mcp.md) | MCP tool schemas, stdio wire protocol, example calls |
| [docs/architecture.md](docs/architecture.md) | Crate layout, storage, search pipeline overview |
| [specs/01-architecture.md](specs/01-architecture.md) | Workspace layout, embedded-first process model, storage trait |
| [specs/02-domain-model.md](specs/02-domain-model.md) | Store, Source, Document, Block, Chunk, Citation; content-addressed IDs |
| [specs/03-config.md](specs/03-config.md) | YAML schema, per-store indexing policy, config vs runtime-state split |
| [specs/04-search-pipeline.md](specs/04-search-pipeline.md) | Ingestion, chunking, embeddings, BM25+dense RRF |
| [specs/05-surfaces.md](specs/05-surfaces.md) | CLI command tree, REST API, MCP tools, error taxonomy |
| [specs/06-roadmap.md](specs/06-roadmap.md) | Phase ordering, federation, packaging |
| [VISION.md](VISION.md) | Long-horizon direction: peer-to-peer store sharing |
| [skills/localdb/SKILL.md](skills/localdb/SKILL.md) | Agent skill definition for localdb-aware AI assistants |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup, test gates, contribution guidelines |
| [docs/design-decisions.md](docs/design-decisions.md) | Open design questions with options and recommendations |

---

## License

[AGPL-3.0-or-later](LICENSE). See the license file for full terms.
