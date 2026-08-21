# Quick Start

This guide walks through the complete workflow: create a store, add a source, index files, and
search — using only the CLI in embedded mode (no daemon required). Config is created for you
automatically along the way; an explicit init step is optional.

For installation instructions, see [install.md](install.md).

## Step 1 — Check initial status

Confirm the installation is working:

```bash
localdb status
```

```
daemon: not running (embedded mode)
stores (1):
  default [libsql] 0 documents, 0 chunks

database: ~/Library/Application Support/localdb/data/localdb.db
  size: 140.0 KB (+ 0 B WAL)
  largest tables:
    sources — 24.0 KB
    resources — 16.0 KB
    chunks — 16.0 KB
    stores — 12.0 KB
    blocks — 12.0 KB
```

Running this — or any command other than `db status`/`migrate`/`downgrade`/`vacuum` — is what
creates the config file, along with the data/models/logs directories, on first use; there's no
separate init step required. Scaffolding also creates a `default` store, which is why it already
shows up above. The generated `config.yaml` is a commented template with every key at its default
value, spelled out for discoverability, not a bare stub; see
[configuration.md#config-is-created-for-you](configuration.md#config-is-created-for-you) for the
full generated file and the `$schema` editor-integration section. If you'd rather do this explicitly
up front instead of implicitly on first use — e.g. to review the generated paths, or to pre-download
the embedding model with `--download-model` — see `localdb init` in [cli.md](cli.md#localdb-init).

## Step 2 — (Optional) Override data paths

By default the data directory follows your platform's standard location. To keep everything under a
single directory (useful for development or isolation), add a `paths` block to your config:

```yaml
version: 1
paths:
  data: ~/localdb/data
  models: ~/localdb/models
  logs: ~/localdb/logs
```

The config file path can also be set with the `LOCALDB_CONFIG` environment variable or the
`--config <path>` flag on any command.

## Step 3 — Create a store

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
default [libsql]
notes [libsql]
```

`default` is the store scaffolding created back in Step 1; `notes` is the one just added. The
`[libsql]` label is the storage backend.

## Step 4 — Add a source

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
01KTVH6AY4DC84HWW7M2PP4F0X [path] /home/user/notes
```

## Step 5 — Index

Scan the source directory and write chunks to the store:

```bash
localdb index --store notes
```

```
Indexing /home/user/notes
Index complete: 3 indexed, 0 skipped, 3 chunks written, 0 unsupported, 0 errors
```

(Output reflects a corpus of three files; your counts will differ.)

> **Note on the model download:** the default embedder (`provider: local`,
> `pplx-embed-context-v1-0.6b`) is downloaded from HuggingFace (~706 MB) the first time
> `localdb index` or `localdb search` runs. No API key or license click-through is required.
> Subsequent runs use the cached model. To fetch it ahead of time instead, run
> `localdb init --download-model` (see [cli.md](cli.md#localdb-init)). See
> [install.md#a-note-on-embedding-models](install.md#a-note-on-embedding-models) for details.

After indexing, the on-disk layout under the data directory looks like:

```
data/
  localdb.db            # unified SQLite database (stores, sources, documents, chunks, FTS5, vectors)
  localdb.db-wal        # WAL sidecar (libsql managed)
  localdb.db-shm        # shared-memory sidecar (libsql managed)
```

## Step 6 — Search

Run a plain-text search across the indexed store:

```bash
localdb search hybrid search
```

```
1. file:///home/user/notes/lancedb-notes.md > LanceDB notes
   LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combining vector similarity with BM25 full-text scoring.

2. file:///home/user/notes/meeting.txt
   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark connectors are deferred to the next milestone.

```

(Paths shown from a scratch run.)

Limit results with `--limit`:

```bash
localdb search --limit 1 rank fusion
```

### JSON output

Pass `--json` to get machine-readable citations. The citation shape is the canonical `localdb`
Citation object (see [../specs/02-domain-model.md](../specs/02-domain-model.md) §6):

```bash
localdb search -s notes --json hybrid search
```

```json
{
  "citations": [
    {
      "block": {
        "kind": "text",
        "seq": 1
      },
      "chunk_id": "82b4631e898166f7834a786b1e8e56125ce6bfc2193fc210f591179527abbdcb",
      "chunk_position": {
        "seq_in_block": 0
      },
      "heading_path": ["LanceDB notes"],
      "location": {
        "span": {
          "end": 157,
          "start": 0
        }
      },
      "metadata": {
        "contributor": [],
        "coverage": null,
        "creator": [],
        "date": null,
        "description": null,
        "format": "text/markdown",
        "identifier": null,
        "kind": "document",
        "language": null,
        "page_count": null,
        "publisher": null,
        "relation": [],
        "rights": null,
        "source": null,
        "subject": [],
        "title": "LanceDB notes",
        "type": null,
        "word_count": null
      },
      "provenance": {
        "content_hash": "55567825f371ea048f61a59fa156068945a7ef0d9276b7813438820002ce72a2",
        "fetched_at": "2026-06-11T14:17:30Z"
      },
      "resource_id": "ee2cfd35725ead3b0fb7ebccdcc4cf9fa0ea6990ac2fa1276dc689e1abed6700",
      "score": {
        "bm25": 1.9203118085861206,
        "dense": 0.640625,
        "fused": 0.032266458495966696
      },
      "snippet": "LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combining vector similarity with BM25 full-text scoring.",
      "store": {
        "id": "01KTVGQ62TQN8X6XN9E5FDZN67",
        "name": "notes"
      },
      "title": "LanceDB notes",
      "uri": "file:///home/user/notes/lancedb-notes.md"
    }
  ]
}
```

(The structural fields above — `block`, `chunk_position`, `heading_path`, `location.span`,
`snippet`, `metadata`, `chunk_id`, `resource_id` and `provenance.content_hash` — are captured from a
real indexing run. `score`, `store` and `provenance.fetched_at` are illustrative.)

(Output truncated to one result; paths shown from a scratch run.)

**Score fields:** `bm25` is the BM25 full-text score; `dense` is the normalized Hamming similarity
(`1.0 − hamming_dist / nbits`) from the binary-quantized local ONNX embedder
(`pplx-embed-context-v1-0.6b` by default). `fused` is the Reciprocal Rank Fusion score used for
final ranking, combining both components.

## Step 7 — Verify status after indexing

```bash
localdb status
```

```
daemon: not running (embedded mode)
stores (1):
  notes [libsql]
```

## What's next

- **Configuration reference:** [configuration.md](configuration.md) — full YAML schema, path
  overrides, per-store indexing policy.
- **CLI reference:** [cli.md](cli.md) — all commands, flags, exit codes, and JSON shapes.
- **MCP integration:** [mcp.md](mcp.md) — connecting localdb to AI agents via the MCP stdio server.
- **Architecture and design:** [../specs/01-architecture.md](../specs/01-architecture.md)
