# Quick Start

This guide walks through the complete workflow: initialize, create a store, add a source, index
files, and search — using only the CLI in embedded mode (no daemon required).

For installation instructions, see [install.md](install.md).

## Step 1 — Initialize

Run `init` once to write the config file and prepare the data directory:

```bash
localdb init
```

Output:

```
Initialized localdb at ~/Library/Application Support/com.localdb.localdb.localdb
  Config: ~/Library/Application Support/com.localdb.localdb.localdb/config.yaml
  Data:   ~/Library/Application Support/com.localdb.localdb.localdb/data

Note: embedding models will be downloaded on first index.
Run `localdb store add <name>` to create a store.
```

(Paths shown are the macOS defaults — see [configuration.md](configuration.md) for Linux
paths and the `--config` flag. Yes, the `com.localdb.localdb.localdb` segment is verbose;
see [architecture.md known gaps](architecture.md#known-gaps).)

The generated `config.yaml` contains only the version key by default:

```yaml
version: 1
# localdb configuration
# Add stores and sources below.
```

> **Note on the "embedding models will be downloaded" message:** This is accurate. The default
> embedder (`pplx-embed-context-v1-0.6b`) is downloaded from HuggingFace (~706 MB) the first
> time `localdb index` or `localdb search` runs. No API key or license click-through is required.
> Subsequent runs use the cached model. See
> [install.md#a-note-on-embedding-models](install.md#a-note-on-embedding-models).

## Step 2 — (Optional) Override data paths

By default the data directory follows your platform's standard location. To keep everything under
a single directory (useful for development or isolation), add a `paths` block to your config:

```yaml
version: 1
paths:
  data: ~/localdb/data
  models: ~/localdb/models
  logs: ~/localdb/logs
```

The config file path can also be set with the `LOCALDB_CONFIG` environment variable or the
`--config <path>` flag on any command.

## Step 3 — Check initial status

Confirm the installation is working:

```bash
localdb status
```

```
daemon: not running (embedded mode)
stores (0):
  (none)
```

## Step 4 — Create a store

A store is a named, isolated index. Create one called `notes`:

```bash
localdb store add notes
```

```
Added store: notes
```

Verify it was created:

```bash
localdb store list
```

```
notes [libsql] (runtime)
```

The `(runtime)` label means the store was created via `store add` and lives in the unified
database. The `[libsql]` label is the storage backend.

> **YAML-declared stores:** Stores declared in `config.yaml` under the `stores:` key appear in
> `store list` as `(yaml)` but cannot be indexed yet — `localdb index` will return
> `store not found` for them. Use `localdb store add` (runtime stores) for all indexing workflows
> today. See [../specs/03-config.md](../specs/03-config.md) for the design intent.

## Step 5 — Add a source

Point the `notes` store at a directory of files. Here we use `~/notes` as the source path:

```bash
localdb source add ~/notes --store notes
```

```
Added source 01KTVH6AY4DC84HWW7M2PP4F0X to store 'notes'
```

The returned identifier (a ULID) is the source ID. List sources to confirm:

```bash
localdb source list --store notes
```

```
01KTVH6AY4DC84HWW7M2PP4F0X [path] ~/notes
```

## Step 6 — Index

Scan the source directory and write chunks to the store:

```bash
localdb index --store notes
```

```
Indexing source 01KTVH6AY4DC84HWW7M2PP4F0X (~/notes)
Index complete: 3 indexed, 0 skipped, 3 chunks written, 0 errors
```

(Output reflects a corpus of three files; your counts will differ.)

After indexing, the on-disk layout under the data directory looks like:

```
data/
  localdb.db            # unified SQLite database (stores, sources, documents, chunks, FTS5, vectors)
  localdb.db-wal        # WAL sidecar (libsql managed)
  localdb.db-shm        # shared-memory sidecar (libsql managed)
```

## Step 7 — Search

Run a plain-text search across the indexed store:

```bash
localdb search how does rust handle errors
```

```
1. file:///path/to/notes/rust-error-handling.md > Error handling in Rust
   Error handling in Rust
Rust uses the Result type for recoverable errors and panic! for unrecoverable ones. The question-

2. file:///path/to/notes/meeting.txt
   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark c

3. file:///path/to/notes/lancedb-notes.md > LanceDB notes
   LanceDB notes
LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combi
```

(Paths shown from a scratch run.)

Limit results with `--limit`:

```bash
localdb search --limit 2 rank fusion
```

### JSON output

Pass `--json` to get machine-readable citations. The citation shape is the canonical
`localdb` Citation object (see [../specs/02-domain-model.md](../specs/02-domain-model.md) §6):

```bash
localdb search -s notes --json hybrid search
```

```json
{
  "citations": [
    {
      "chunk_id": "f0113639ebf62fa402aa506a80e0f6dba19a970cfbea3c80ffbb4ca082db30e7",
      "document_id": "ff6ff626d0062eab2d3a5f76dbbe75e6a265a127d99486cacfcde9f42777fe1d",
      "heading_path": [
        "LanceDB notes"
      ],
      "provenance": {
        "content_hash": "360be062b82116aa1a7f707bc9ea9d2f60e0f619e84e4f0f72e8f689d0e18f64",
        "fetched_at": "2026-06-11T14:17:30Z"
      },
      "score": {
        "bm25": 1.9203118085861206,
        "dense": 0.64,
        "fused": 0.032266458495966696
      },
      "snippet": "LanceDB notes\nLanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combining vector similarity with BM25 full-text scoring.\n",
      "span": {
        "end": 172,
        "start": 0
      },
      "store": {
        "id": "01KTVGQ62TQN8X6XN9E5FDZN67",
        "name": "notes"
      },
      "title": "LanceDB notes",
      "uri": "file:///private/tmp/localdb-recon.0z2dTw/notes/lancedb-notes.md"
    }
  ]
}
```

(Output truncated to one result; paths shown from a scratch run.)

**Score fields:** `bm25` is the BM25 full-text score; `dense` is the normalized Hamming
similarity (`1.0 − hamming_dist / nbits`) from the binary-quantized local ONNX embedder
(`pplx-embed-context-v1-0.6b` by default). `fused` is the Reciprocal Rank Fusion score
used for final ranking, combining both components.

## Step 8 — Verify status after indexing

```bash
localdb status
```

```
daemon: not running (embedded mode)
stores (1):
  notes [libsql] (runtime)
```

## What's next

- **Configuration reference:** [configuration.md](configuration.md) — full YAML schema, path
  overrides, per-store indexing policy.
- **CLI reference:** [cli.md](cli.md) — all commands, flags, exit codes, and JSON shapes.
- **MCP integration:** [mcp.md](mcp.md) — connecting localdb to AI agents via the MCP stdio
  server.
- **Architecture and design:** [../specs/01-architecture.md](../specs/01-architecture.md)
