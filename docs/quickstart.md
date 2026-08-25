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
Auto-indexing source 01KTVH6AY4DC84HWW7M2PP4F0X ...
Indexing /home/user/notes
  discovered 3 files
  indexed 3 docs, 0 skipped, 0 deleted, 5 chunks
```

`source add` indexes the new source immediately — there's no separate step required to get
searchable results. (Output reflects a corpus of three files; your counts will differ.)

> **Note on the model download:** the default embedder (`provider: local`,
> `pplx-embed-context-v1-0.6b`) is downloaded from HuggingFace (~706 MB) the first time indexing
> actually runs — which, per above, is as early as this `source add` step, not a later explicit
> `localdb index`/`localdb search`. No API key or license click-through is required. Subsequent runs
> use the cached model. To fetch it ahead of time instead, run `localdb init --download-model` (see
> [cli.md](cli.md#localdb-init)). See
> [install.md#a-note-on-embedding-models](install.md#a-note-on-embedding-models) for details.

The returned identifier (a ULID) is the source ID. List sources to confirm:

```bash
localdb source list --store notes
```

```
01KTVH6AY4DC84HWW7M2PP4F0X [path] /home/user/notes
```

## Step 5 — Re-index after changes (optional)

`source add` already indexed the source back in Step 4, so files are searchable immediately. Run
`localdb index` again whenever files under a source's path change — new/changed files get picked up,
removed files get pruned. Since nothing has changed since Step 4, running it now is a no-op:

```bash
localdb index --store notes
```

```
Indexing /home/user/notes
  discovered 3 files
  indexed 0 docs, 3 skipped, 0 deleted, 0 chunks
Index complete: 0 indexed, 3 skipped, 0 chunks written, 0 unsupported, 0 errors
```

(Output reflects a corpus of three files with nothing changed since Step 4; your counts will
differ.)

The on-disk layout under the data directory looks like:

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

3. file:///home/user/notes/rrf.md > Reciprocal rank fusion
   RRF combines multiple ranked result lists into a single ranking by summing the reciprocal of each item's rank across lists. It's a simple, robust way to fuse BM25 and vector search scores without needing to normalize them onto the same scale.

```

(Paths shown from a scratch run.)

Limit results with `--limit`:

```bash
localdb search --limit 1 rank fusion
```

### JSON output

Pass `--json` to get machine-readable citations. The citation shape is the canonical `localdb`
Citation object (see
[specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
§6):

```bash
localdb search -s notes --json hybrid search
```

```json
{
  "citations": [
    {
      "chunk_id": "5b83d9d595f5da78124fe05a913289f0dbca976d25facb6c26ea0633f688f58e",
      "resource_id": "256150fc0a5e7c08be82a65c127d332dfd683507d52505c1fa6353cf98bb051d",
      "store": {
        "id": "01KTVGQ62TQN8X6XN9E5FDZN67",
        "name": "notes"
      },
      "uri": "file:///home/user/notes/lancedb-notes.md",
      "title": "LanceDB notes",
      "heading_path": ["LanceDB notes"],
      "block": {
        "seq": 1,
        "kind": "text"
      },
      "chunk_position": {
        "seq_in_block": 0
      },
      "location": {
        "span": {
          "start": 0,
          "end": 157
        }
      },
      "snippet": "LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combining vector similarity with BM25 full-text scoring.",
      "score": {
        "fused": 0.03278688524590164,
        "dense": 0.6455078125,
        "bm25": 1.3081889152526855
      },
      "provenance": {
        "fetched_at": "2026-06-11T14:17:30Z",
        "content_hash": "55567825f371ea048f61a59fa156068945a7ef0d9276b7813438820002ce72a2"
      },
      "metadata": {
        "kind": "document",
        "title": "LanceDB notes",
        "creator": [],
        "subject": [],
        "description": null,
        "publisher": null,
        "contributor": [],
        "date": null,
        "type": null,
        "format": "text/markdown",
        "identifier": null,
        "source": null,
        "language": null,
        "relation": [],
        "coverage": null,
        "rights": null,
        "page_count": null,
        "word_count": null
      }
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
stores (2):
  default [libsql] 0 documents, 0 chunks
  notes [libsql] 3 documents, 5 chunks

database: ~/Library/Application Support/localdb/data/localdb.db
  size: 180.0 KB (+ 0 B WAL)
  ~36.0 KB per chunk (5 chunks total)
  largest tables:
    chunks_vec_idx_shadow — 48.0 KB
    sources — 24.0 KB
    resources — 16.0 KB
    chunks — 16.0 KB
    stores — 12.0 KB
```

`default` is still the empty store scaffolded back in Step 1 — nothing was ever added to it in this
walkthrough.

## What's next

- **Configuration reference:** [configuration.md](configuration.md) — full YAML schema, path
  overrides, per-store indexing policy.
- **CLI reference:** [cli.md](cli.md) — all commands, flags, exit codes, and JSON shapes.
- **MCP integration:** [mcp.md](mcp.md) — connecting localdb to AI agents via the MCP stdio server.
- **Architecture and design:**
  [specs/01-architecture.md](https://github.com/dokterbob/localdb/blob/main/specs/01-architecture.md)
