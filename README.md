# localdb (working name)

A **FOSS local-first knowledge server** for local-first technical users, agent workflows, and
self-hosters: point it at your files and URLs and they become a hybrid-searchable knowledge layer
available everywhere you work — CLI, MCP agents, HTTP API, and (later) a web UI. Rust backend,
embedded-first (no daemon required), local embeddings by default.

**License:** [AGPL-3.0-or-later](LICENSE).

## Install

### Option 1 — Pre-built tarball (recommended)

Download the tarball for your platform from the
[latest release](https://github.com/localdb/localdb/releases/latest):

| Platform | Download |
|---|---|
| macOS Apple Silicon | `localdb-<version>-aarch64-apple-darwin.tar.gz` |
| Linux x86_64 | `localdb-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `localdb-<version>-aarch64-unknown-linux-gnu.tar.gz` |

```bash
# Example: macOS Apple Silicon
VERSION=0.1.0
PLATFORM=aarch64-apple-darwin
curl -L "https://github.com/localdb/localdb/releases/download/v${VERSION}/localdb-v${VERSION}-${PLATFORM}.tar.gz" \
  | tar -xz -C /usr/local/bin --strip-components=1 localdb-v${VERSION}-${PLATFORM}/localdb
localdb --version
```

### Option 2 — `cargo install` (from source)

Requires a Rust toolchain (1.82 or later). Install via [rustup](https://rustup.rs/).

```bash
cargo install --git https://github.com/localdb/localdb localdb
# Or, from a checked-out repo:
cargo install --path localdb
localdb --version
```

### Quick start

```bash
localdb init
localdb store add my-docs
localdb --store my-docs source add ~/Documents/notes
localdb --store my-docs index
localdb --store my-docs search "hybrid search embeddings"
```

> **Note:** The first `index` run downloads the default ONNX embedding model (~80 MB).
> Disable downloads and use a hosted provider by setting `embedding.provider` in
> `~/.config/localdb/config.yaml`.

### Daemon mode (optional)

```bash
localdb serve   # starts the HTTP API + file watcher on localhost:7700
```

The daemon adds file watching, scheduled URL refresh, and the REST API.
It is not required for CLI or MCP use.

## Documents

| Document | What it is |
|---|---|
| [VISION.md](VISION.md) | Long-horizon direction: stores shared peer-to-peer, propagating through the social graph — and the four hooks the MVP carries for it. |
| [specs/01-architecture.md](specs/01-architecture.md) | Workspace/crate layout, embedded-first daemon-optional process model, store-vs-backend abstraction, TDD & coverage gates. |
| [specs/02-domain-model.md](specs/02-domain-model.md) | Store, Source, Document, Block, Chunk, Citation, IndexJob; content-addressed IDs; provenance; citation shape. |
| [specs/03-config.md](specs/03-config.md) | YAML schema, per-store indexing policy, the bootstrap-config vs runtime-state split, file locations. |
| [specs/04-search-pipeline.md](specs/04-search-pipeline.md) | Ingestion, extraction matrix, chunking presets, document-aware (contextualized) embeddings, BM25+dense with RRF. |
| [specs/05-surfaces.md](specs/05-surfaces.md) | CLI command tree, REST API, read-only MCP tools, shared error taxonomy. |
| [specs/06-roadmap.md](specs/06-roadmap.md) | Phase ordering, federation requirements, Qdrant Edge watch-item, packaging. |
| [PLAN.md](PLAN.md) | MVP implementation tickets (T01–T12) in dependency waves, sized for agent delegation. |

## The short version

One binary (`localdb`) with subcommands. CLI and MCP open the store in-process — `localdb search`
works seconds after install with nothing running. An optional daemon adds file watching, URL
refresh, and the HTTP API. Multiple stores per instance, each private or (later) shared. LanceDB
embedded behind a storage trait; contextualized embeddings via local ONNX by default. Built
test-first: ≥80% coverage on critical functions, ≥90% on anything that modifies data.
