# localdb

A **local-first document index with hybrid search and citations** — point it at your files and
they become a searchable knowledge layer available everywhere you work: CLI, MCP agents, and
(experimentally) an HTTP API. One binary, no daemon required for search, works entirely on your
machine. Built for technical users, self-hosters, and AI agent workflows that need reliable,
citeable retrieval of local documents.

**Status: v0.1.0 pre-release.** Hybrid search is currently BM25-driven — dense vector scoring
uses a placeholder embedder and always returns 1.0; local ONNX embedding models are not yet
wired. The HTTP daemon is experimental (in-memory store, does not see CLI-indexed data). See
[Honest status](#honest-status) below.

**License:** [AGPL-3.0-or-later](LICENSE).

---

## Feature highlights

- **Hybrid search with citations** — BM25 + dense (RRF fusion) returning structured
  `Citation` objects with URI, heading path, snippet, span, content hash, and per-component scores.
- **Embedded-first** — `localdb search` opens the store in-process; nothing needs to be running.
- **MCP server** — `localdb mcp` exposes three read-only tools (`search`, `list_stores`,
  `get_document`) directly to Claude and other MCP clients.
- **Multiple stores** — each store is isolated; query one or all with `--store`.
- **LanceDB backend** — columnar vector + BM25 index, embedded, no separate server.
- **`--json` everywhere** — machine-readable output on every command.

---

## Install

### From source (works today)

Requires a Rust toolchain (1.82 or later). Install via [rustup](https://rustup.rs/).

```bash
git clone https://github.com/dokterbob/localdb
cd localdb
cargo install --path localdb
localdb --version
```

### Pre-built tarballs

Available from the [Releases](https://github.com/dokterbob/localdb/releases) page once a
release is tagged.

---

## 60-second quickstart

```bash
# 1. Create a config file
localdb init

# 2. Create a store
localdb store add notes

# 3. Point it at a directory of files
localdb source add ~/notes --store notes

# 4. Index
localdb index --store notes

# 5. Search
localdb search "how does rust handle errors" --store notes
```

Example output from step 5 (paths shown from a scratch run):

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
hashes, and per-component scores:

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

See [docs/mcp.md](docs/mcp.md) for full tool schemas and example calls.

---

## Experimental HTTP daemon

```bash
localdb serve   # binds http://127.0.0.1:7700 by default
```

The daemon exposes a REST API. It is **experimental**: it uses an in-memory store and does not
see data indexed via the CLI. CLI commands also fail while the daemon is running on the same
data directory. See [docs/http-api.md](docs/http-api.md) for endpoint reference and known
limitations.

---

## Honest status

| Area | What is true today |
|---|---|
| Search ranking | BM25-driven. Dense scores are placeholder (always `1.0`). RRF fusion runs but is BM25-weighted in practice. |
| Embedding models | No model download happens. `init` mentions a future model download; this is not yet wired. |
| HTTP daemon | Experimental preview. Uses an in-memory store; does not share data with CLI-indexed stores. |
| YAML-declared stores | Appear in `store list` but **cannot be indexed** (`localdb index` only resolves runtime stores). Use `localdb store add` + `localdb source add` instead. |
| CLI while daemon runs | Every CLI command fails with a DB lock error while a daemon is running on the same data directory. Stop the daemon before CLI use. |

Design rationale and planned behavior live in the [specs/](specs/) directory.

---

## Documentation

| Document | Contents |
|---|---|
| [docs/install.md](docs/install.md) | Full install options, platform notes, shell completion |
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
| [PLAN.md](PLAN.md) | MVP implementation tickets (T01–T12) |
| [skills/localdb/SKILL.md](skills/localdb/SKILL.md) | Agent skill definition for localdb-aware AI assistants |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup, test gates, contribution guidelines |

---

## License

[AGPL-3.0-or-later](LICENSE). See the license file for full terms.
