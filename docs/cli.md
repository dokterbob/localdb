# localdb CLI reference

`localdb` is a local-first hybrid-search document index. This page is the
complete reference for its command-line interface (v0.1.0).

For design decisions and process-model details see
[specs/05-surfaces.md](../specs/05-surfaces.md). For the HTTP daemon surface see
[docs/http-api.md](http-api.md). For the MCP stdio surface see
[docs/mcp.md](mcp.md).

---

## Global flags

These flags are accepted by every subcommand.

| Flag | Description |
|---|---|
| `--config <PATH>` | Path to the config file. Default: the platform config dir — `~/Library/Application Support/com.localdb.localdb.localdb/config.yaml` on macOS, `~/.config/localdb/config.yaml` on Linux. Can also be set via the `LOCALDB_CONFIG` environment variable. |
| `--json` | Emit machine-readable JSON instead of human-readable text. All JSON shapes are stable API. |
| `--store <NAME>` | Operate only on the named store. Repeatable to target multiple stores; omit to target all stores. |
| `-h, --help` | Print help. |
| `-V, --version` | Print version. |

**Environment variable:** `LOCALDB_CONFIG=<path>` is equivalent to `--config <path>`.

---

## Exit codes

Exit codes are stable API. See [specs/05-surfaces.md §5](../specs/05-surfaces.md#5-shared-error-taxonomy) for the full error taxonomy that drives them.

| Code | Meaning | Example trigger |
|---|---|---|
| `0` | OK | Successful command |
| `1` | Internal error | Bug or locked redb database (see daemon note below) |
| `2` | Invalid usage or config | Unknown subcommand, duplicate store, bad config file |
| `3` | Not found | `store remove <name>` — store does not exist |
| `4` | Conflict / locked | `serve` when a daemon is already running on the same data dir |
| `5` | Unavailable | Daemon unreachable (stale socket) |

**Quirk:** `search --store <unknown-store>` currently exits `0` and prints
`No indexed stores found.` rather than `exit 3`. This is a known divergence from
the spec; `store remove <name>` correctly exits `3`.

---

## `localdb init`

Initialize config and data directory.

```
Initialize config and data directory; prompt for first-run model download

Usage: localdb init [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

Creates the config file and data directory if they do not exist. Prints the
paths it created. The generated config file contains only `version: 1`; add
`paths`, `stores`, and other keys as needed (see
[specs/03-config.md](../specs/03-config.md)).

**Note on embedding models:** `init` prints `embedding models will be downloaded
on first index`. In v0.1.0 this message is inaccurate — no model download
occurs; the current build uses a hash-based internal embedder. See the note in
[`index`](#localdb-index) for details.

**Example:**

```
$ localdb init --config ~/notes/localdb-config.yaml
Initialized localdb at ~/notes
  Config: ~/notes/localdb-config.yaml
  Data:   ~/Library/Application Support/com.localdb.localdb.localdb/data

Note: embedding models will be downloaded on first index.
Run `localdb store add <name>` to create a store.
```

(The data path defaults to the platform data dir unless `paths.data` is
overridden in the config.)

---

## `localdb status`

Show stores, document/chunk counts, and daemon state.

```
Show stores, counts, policy staleness, and daemon state

Usage: localdb status [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

**Examples:**

```
$ localdb status
daemon: not running (embedded mode)
stores (1):
  notes [lancedb] (runtime)
```

```
$ localdb status --json
{
  "daemon": "not running (embedded mode)",
  "stores": [
    {
      "backend": "lancedb",
      "name": "notes",
      "ownership": "runtime",
      "visibility": "private"
    }
  ]
}
```

---

## `localdb store`

Manage stores.

```
Manage stores

Usage: localdb store [OPTIONS] <COMMAND>

Commands:
  add     Add a new store
  list    List all stores
  remove  Remove a store
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

### `localdb store add`

```
Add a new store

Usage: localdb store add [OPTIONS] <NAME>

Arguments:
  <NAME>  Store name

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

Creates a runtime-owned store backed by LanceDB. Runtime stores are persisted in
the data directory and survive restarts; they are distinct from YAML-declared
stores (see note below).

Exits `2` (`invalid_request`) if a store with that name already exists:

```
$ localdb store add notes
Added store: notes

$ localdb store add notes
error: invalid request: store 'notes' already exists
exit: 2
```

### `localdb store list`

```
List all stores

Usage: localdb store list [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

Lists both runtime stores (created with `store add`) and YAML-declared stores.
The ownership label is `runtime` or `yaml`.

```
$ localdb store list
notes [lancedb] (runtime)

$ localdb store list --json
{
  "stores": [
    {
      "backend": "lancedb",
      "name": "notes",
      "ownership": "runtime",
      "visibility": "private"
    }
  ]
}
```

### `localdb store remove`

```
Remove a store

Usage: localdb store remove [OPTIONS] <NAME>

Arguments:
  <NAME>  Store name or ID

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

Exits `3` (`store_not_found`) if the name does not match any known store:

```
$ localdb store remove nope
error: store not found: nope
exit: 3
```

**YAML-declared stores:** stores declared in `config.yaml` under `stores:` appear
in `store list` with ownership `yaml` but cannot be indexed in v0.1.0 (see
[YAML-declared stores](#yaml-declared-stores-limitation) below). Use
`localdb store add` for stores you intend to index.

---

## `localdb source`

Manage sources on a store.

```
Manage sources on a store

Usage: localdb source [OPTIONS] <COMMAND>

Commands:
  add     Add a new source to a store
  list    List sources on a store
  remove  Remove a source from a store
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

### `localdb source add`

```
Add a new source to a store

Usage: localdb source add [OPTIONS] <SOURCE>

Arguments:
  <SOURCE>  Source path or URL

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

Registers a filesystem path (or URL) as a source for the given store. The
`--store` flag is required.

**Note:** path existence is not validated at registration time — `source add
/does/not/exist` succeeds (exit 0). The error surfaces at `index` time.

```
$ localdb source add ~/notes --store notes
Added source 01KTVH6AY4DC84HWW7M2PP4F0X to store 'notes'
```

### `localdb source list`

```
List sources on a store

Usage: localdb source list [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

```
$ localdb source list --store notes
01KTVH6AY4DC84HWW7M2PP4F0X [path] /home/user/notes

$ localdb source list --store notes --json
{
  "sources": [
    {
      "id": "01KTVH6AY4DC84HWW7M2PP4F0X",
      "kind": "path",
      "preset": "prose",
      "root": "/home/user/notes",
      "store": "notes",
      "url": null
    }
  ]
}
```

(paths shown from a scratch run)

### `localdb source remove`

```
Remove a source from a store

Usage: localdb source remove [OPTIONS] <ID>

Arguments:
  <ID>  Source ID

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

The `<ID>` is the ULID shown by `source list`.

---

## `localdb index`

Run a one-shot scan-and-index job.

```
Run a one-shot scan-and-index job

Usage: localdb index [OPTIONS]

Options:
      --config <PATH>       Path to config file (default: platform data dir / localdb / config.yaml)
      --source <SOURCE_ID>  Limit to a specific source (by ID)
      --json                Emit JSON output instead of human-readable text
      --store <NAME>        Operate on this store (repeatable; defaults to all stores)
  -h, --help                Print help
  -V, --version             Print version
```

Walks every registered source for the targeted store(s), extracts and chunks
documents, and writes them to the LanceDB store on disk. Progress is printed to
stdout.

**Embeddings:** the CLI calls `embed::create_embedder` from the config policy.
The default embedder (`pplx-embed-context-v1-0.6b`, local ONNX) is downloaded
automatically on first run (~706 MB). See
[specs/04-search-pipeline.md](../specs/04-search-pipeline.md) for the pipeline.

```
$ localdb index --store notes
Indexing source 01KTVH6AY4DC84HWW7M2PP4F0X (/home/user/notes)
Index complete: 3 indexed, 0 skipped, 3 chunks written, 0 errors
```

Use `--source <ID>` to re-index a single source without touching others in the
same store.

YAML-declared stores cannot be indexed in v0.1.0; use `localdb store add` +
`localdb source add` instead (see
[YAML-declared stores](#yaml-declared-stores-limitation)).

---

## `localdb search`

Hybrid search with citations.

```
Hybrid search with citations

Usage: localdb search [OPTIONS] <QUERY>...

Arguments:
  <QUERY>...  Natural language query (no quotes needed; flags must precede the query)

Options:
      --config <PATH>   Path to config file (default: platform data dir / localdb / config.yaml)
      --limit <LIMIT>   Maximum number of results to return [default: 10]
      --json            Emit JSON output instead of human-readable text
  -s, --store <NAME>    Operate on this store (repeatable; defaults to all stores)
  -h, --help            Print help
  -V, --version         Print version
```

> **Options-first:** flags (`--limit`, `--store`, `-s`, `--json`) must appear
> **before** the query words. Anything after the first query word is captured
> verbatim as query text — so `localdb search --limit 5 rank fusion` works, but
> `localdb search rank fusion --limit 5` treats `--limit 5` as part of the query.

Runs hybrid BM25 + dense-vector search across the targeted stores and returns
ranked citations. The Citation JSON shape is documented in
[specs/02-domain-model.md](../specs/02-domain-model.md) §6.

**Ranking:** hybrid BM25 + dense (RRF fusion). The `dense` score is the cosine
similarity from the configured ONNX embedder; `fused` is the final RRF score.

**Examples:**

```
$ localdb search how does rust handle errors
1. file:///home/user/notes/rust-error-handling.md > Error handling in Rust
   Error handling in Rust
Rust uses the Result type for recoverable errors and panic! for unrecoverable ones. The question-

2. file:///home/user/notes/meeting.txt
   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark c

3. file:///home/user/notes/lancedb-notes.md > LanceDB notes
   LanceDB notes
LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combi
```

(paths shown from a scratch run)

```
$ localdb search --limit 2 rank fusion
1. file:///home/user/notes/meeting.txt
   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark c

2. file:///home/user/notes/rust-error-handling.md > Error handling in Rust
   Error handling in Rust
Rust uses the Result type for recoverable errors and panic! for unrecoverable ones. The question-
```

JSON output (full citation shape):

```
$ localdb search -s notes --json hybrid search
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
        "dense": 1.0,
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

(paths shown from a scratch run)

**Quirk:** `search --store <unknown>` exits `0` with an empty result set and the
message `No indexed stores found. Run 'localdb index' first.` rather than
`exit 3`. This matches neither the spec nor the behavior of `store remove` (which
correctly exits `3`).

---

## `localdb serve`

> **Experimental.** The HTTP daemon is a preview in v0.1.0. See limitations below.

Start the HTTP API daemon.

```
Start the HTTP API daemon (file watching, scheduled refresh, REST API)

Usage: localdb serve [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
      --store <NAME>   Operate on this store (repeatable; defaults to all stores)
  -h, --help           Print help
  -V, --version        Print version
```

Binds `127.0.0.1:7700` by default (configurable via `server.bind` / `server.port`
in `config.yaml`). Prints an announce line on startup:

```
$ localdb serve
daemon listening on http://127.0.0.1:7700
```

Also creates a Unix socket at `<data_dir>/daemon.sock` that CLI commands use to
detect the daemon.

Exits `4` (`daemon_running`) if a daemon is already running on the same data dir:

```
$ localdb serve
error: daemon is already running
exit: 4
```

For the full HTTP API reference see [docs/http-api.md](http-api.md).

### Known limitations (v0.1.0)

- **In-memory store only.** The daemon uses a shared in-memory store. Data
  indexed via the CLI (`localdb index`) is not visible to the daemon's
  `/v1/search` endpoint, and vice versa. The HTTP API returns the correct
  response shapes but operates on no real data.
- **CLI is blocked while the daemon runs.** Every CLI command opens the
  runtime-state database before probing the daemon socket. While the daemon
  holds that lock, CLI commands fail with `exit 1` (`internal error: Database
  already open`). Stop the daemon before using CLI commands against the same
  data directory.
- **Stale socket after kill.** If the daemon process is killed without a clean
  shutdown, `daemon.sock` is not removed. Subsequent CLI commands report
  `daemon: running` but searches fail with `exit 5` (`daemon is unreachable`).
  Fix by removing the stale socket file:

  ```
  $ rm <data_dir>/daemon.sock
  ```

---

## `localdb mcp`

Run the MCP server on stdio for use with AI agents.

```
Run the MCP server on stdio for use with AI agents

Usage: localdb mcp [OPTIONS]

Options:
      --allow-write
          Enable write tools (reserved for future use; always rejected in v1).
          
          Parsing this flag now makes the CLI stable for callers even though the server rejects all mutating operations in v1.

      --config <PATH>
          Path to config file (default: platform data dir / localdb / config.yaml)

      --json
          Emit JSON output instead of human-readable text

      --store <NAME>
          Operate on this store (repeatable; defaults to all stores)

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

Starts a JSON-RPC 2.0 MCP server on stdin/stdout, using embedded mode (no daemon
required). The server is fully functional in v0.1.0 and exposes three read-only
tools: `search`, `get_document`, and `list_stores`.

`--allow-write` is accepted on the command line for forward compatibility but all
mutating tool calls are rejected in v1.

See [docs/mcp.md](mcp.md) for the full tool reference, input schemas, and
example JSON-RPC exchanges.

**Example** (connect via any MCP-capable client, or pipe JSON-RPC by hand):

```
$ localdb mcp --config ~/notes/localdb-config.yaml
```

The server reads newline-delimited JSON-RPC from stdin and writes responses to
stdout. MCP clients (Claude Desktop, etc.) handle the transport automatically.

---

## Typical workflow

```sh
# 1. Initialize (first time only)
localdb init

# 2. Create a runtime store
localdb store add notes

# 3. Register a source directory
localdb source add ~/notes --store notes

# 4. Index
localdb index --store notes

# 5. Search
localdb search "how does rust handle errors"

# 6. Search with JSON output for scripting
localdb search "hybrid search" --store notes --json
```

---

## YAML-declared stores (limitation)

Stores declared in `config.yaml` under `stores:` appear in `store list` with
ownership `yaml` and are visible to `search`, but `localdb index --store
<yaml-store>` exits `3` (`store not found`) in v0.1.0 because the indexer
resolves stores only from the runtime-state database:

```
$ localdb store list          # shows yaml-declared store
handbook [lancedb] (yaml)

$ localdb index --store handbook
error: store not found: handbook
exit: 3
```

**Working path today:** use `localdb store add` (runtime store) + `localdb source
add` for any stores you intend to index. YAML store indexing is planned; see
[specs/06-roadmap.md](../specs/06-roadmap.md).

YAML store/source schema reference is in
[specs/03-config.md](../specs/03-config.md).

---

## Config validation errors

Bad config files exit `2` with a path-precise message. Common cases:

| Config problem | Error message |
|---|---|
| Unknown top-level key | `invalid config: unknown field 'bogus_key', expected one of 'version', 'server', 'paths', 'defaults', 'stores', 'providers'` |
| Wrong version | `invalid config: unsupported config version 2; only version 1 is supported. Hint: add 'version: 1' at the top of your config file.` |
| Source missing required field | `invalid config: stores[0].sources[0].root: required for kind 'path'` |
| Config file not found | `invalid config: cannot read config file '/path/to/config.yaml': No such file or directory` |
| Not valid YAML | `invalid config: invalid type: map, expected field identifier at line 1 column 2` |
