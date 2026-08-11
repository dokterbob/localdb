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
| `-s, --store <NAME>` | Narrow to these stores; repeatable. It is a **filter**, so omitting it means **all stores** for `search`, `status`, `store list`, `source list`, `source remove <ULID>`, `index` and `mcp`. Three exceptions: `source add` (and the `add` alias) defaults to the store named `default`, exit 2 if absent; `source remove <path\|url>` requires it, exit 2 without it; and `init`, `serve`, `store add`, `store remove`, `db status`/`migrate`/`downgrade`/`vacuum` **reject it outright** (exit 2) because they aren't store-scoped. An explicit name is always validated — unknown is exit 3, never silently ignored. See [specs/05-surfaces.md §2.2](../specs/05-surfaces.md#22-store-scope). |
| `-y, --yes` | Skip confirmation prompts for destructive operations (`db migrate` legacy rebuild, `db downgrade`). |
| `-h, --help` | Print help. |
| `-V, --version` | Print version. |

**Environment variable:** `LOCALDB_CONFIG=<path>` is equivalent to `--config <path>`.

---

## Exit codes

Exit codes are stable API. See [specs/05-surfaces.md §5](../specs/05-surfaces.md#5-shared-error-taxonomy) for the full error taxonomy that drives them.

| Code | Meaning | Example trigger |
|---|---|---|
| `0` | OK | Successful command |
| `1` | Internal error | Bug or unrecoverable runtime failure |
| `2` | Invalid usage or config | Unknown subcommand, duplicate store, bad config file |
| `3` | Not found | `store remove <name>` — store does not exist |
| `4` | Conflict / locked | `serve` when a daemon is already running on the same data dir |
| `5` | Unavailable | Daemon unreachable (stale socket) |

---

## `localdb init`

Initialize config and data directory.

```
Initialize config and data directory; prompt for first-run model download

Usage: localdb init [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Creates the config file and data directory if they do not exist. Prints the
paths it created. The generated config file contains only `version: 1`; add
`paths` and other keys as needed (see
[specs/03-config.md](../specs/03-config.md)).

**Not store-scoped:** `init` runs before any store exists — the only store it
creates is `default`, which `--store` cannot rename or redirect — so passing
`--store` exits `2` rather than being silently ignored. The check runs first,
so a misused flag creates no directories and writes no config.

**Note on embedding models:** `init` prints a note about model download. It is
accurate: the default embedder (`pplx-embed-context-v1-0.6b`, local ONNX) is
downloaded from HuggingFace (~706 MB) the first time `localdb index` or
`localdb search` runs. See the note in [`index`](#localdb-index) for details.

**Example:**

```
$ localdb init --config ~/notes/localdb-config.yaml
Initialized localdb at ~/notes
  Config: ~/notes/localdb-config.yaml
  Data:   ~/Library/Application Support/com.localdb.localdb.localdb/data

Note: when using 'local-onnx' provider, the ONNX model is downloaded on first index.
      Hosted providers (openai-compatible, perplexity, voyage) require an API key in config.
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
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

**Examples:**

```
$ localdb status
daemon: not running (embedded mode)
stores (1):
  notes [libsql]
```

```
$ localdb status --json
{
  "daemon": "not running (embedded mode)",
  "stores": [
    {
      "backend": "libsql",
      "name": "notes",
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
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
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
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Creates a store backed by libsql. Stores are persisted in
the unified database (`<data_dir>/localdb.db`) and survive restarts.

**Not store-scoped:** the store is named by the `<NAME>` argument, so passing
`--store` exits `2` rather than being silently ignored.

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
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Lists stores created with `store add`.

```
$ localdb store list
notes [libsql]

$ localdb store list --json
{
  "stores": [
    {
      "backend": "libsql",
      "name": "notes",
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
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Exits `3` (`store_not_found`) if the name does not match any known store:

```
$ localdb store remove nope
error: store not found: nope
exit: 3
```

**Not store-scoped:** the store is named by the `<NAME>` argument, so passing
`--store` exits `2` rather than being silently ignored. This is checked before
the confirmation prompt, so a misused flag never gets as far as asking you to
confirm a deletion.

---

## `localdb source`

Manage sources on a store. With `--store` omitted, `list` and `remove <ULID>`
span **every** store — `-s` is a filter. `add` is the exception: a write has to
land in one named place, so it targets the store named `default` and exits `2`
if there isn't one. `remove <path|url>` is the other: the same path can be a
source in several stores, so it requires an explicit `--store`
(specs/05-surfaces.md §2.2).

```
Manage sources on a store

Usage: localdb source [OPTIONS] <COMMAND>

Commands:
  add     Add a new source to a store
  list    List sources across stores
  remove  Remove a source from a store
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

### `localdb source add`

```
Add a new source to a store

Usage: localdb source add [OPTIONS] <SOURCES>...

Arguments:
  <SOURCES>...  Source paths or URLs (one or more)

Options:
      --config <PATH>      Path to config file (default: platform data dir / localdb / config.yaml)
      --refresh <REFRESH>  Refresh interval for URL sources (e.g. "1h", "30m", "3600")
      --json               Emit JSON output instead of human-readable text
  -s, --store <NAME>       Operate on these stores (repeatable); a filter, not a selector
  -y, --yes                Skip confirmation prompts for destructive operations
  -h, --help               Print help (see more with '--help')
  -V, --version            Print version
```

Registers one or more filesystem paths (or URLs) as sources for a store.
`--store` is repeatable; omit it and the source is added to the store named
`default` (exit `2` if no such store exists) — it is never guessed from
whatever stores happen to exist (specs/05-surfaces.md §2.2).

This is the **one** command where omitting `--store` narrows rather than
spans. Everything else treats `-s` as a filter over all stores; a write can't,
because "add this source to every store" is not what anyone means.

**Note:** path existence is not validated at registration time — `source add
/does/not/exist` succeeds (exit 0). The error surfaces at `index` time.

```
$ localdb source add ~/notes --store notes
Added source 01KTVH6AY4DC84HWW7M2PP4F0X to store 'notes'
```

### `localdb source list`

```
List sources across stores

Usage: localdb source list [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Omit `--store` and this lists **every** store's sources; pass `--store`
(repeatable) to narrow to one or more specific stores. A store-name column
appears in the output only when more than one store is in scope
(specs/05-surfaces.md §2.2), so a single-store database and an explicit
`-s <one-store>` both keep the original column-free format.

```
$ localdb source list                     # no --store: every store
books    01KWEZN72MJ4T8Q1V3XA9BCDEF [path] /Volumes/Archive/books
default  01KTVH6AY4DC84HWW7M2PP4F0X [path] /home/user/notes
hydra    01KWEXGA9YR5S2P7N4MB6GHIJK [path] /home/user/hydra-docs

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
      "store": {
        "name": "notes"
      },
      "store_id": "01KTVGQ62TQN8X6XN9E5FDZN67",
      "url": null
    }
  ]
}
```

(paths shown from a scratch run)

### `localdb source remove`

```
Remove a source from a store

Usage: localdb source remove [OPTIONS] <IDS>...

Arguments:
  <IDS>...  Source IDs, paths, or URLs (one or more)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

A `<ID>` may be the ULID shown by `source list`, or a source's path/URL. The
two shapes have different `--store` rules, because they differ in whether they
identify a store on their own (specs/05-surfaces.md §2.2):

| Argument | `--store` omitted |
|---|---|
| ULID | Searches **every** store — a ULID is globally unique, so its owning store is not in question |
| path or URL | Exit `2`, asking for `--store` — the same path can be registered in several stores at once |

```
$ localdb source remove 01KWEZN72MJ4T8Q1V3XA9BCDEF   # found wherever it lives
Removed source: 01KWEZN72MJ4T8Q1V3XA9BCDEF

$ localdb source remove ~/notes
error: source remove by path/url requires --store; pass --store <name> or use the source ULID
exit: 2
```

An explicit `--store` still hard-filters a ULID removal: if the source exists
but lives outside the named scope, this is `source_not_found` (exit `3`)
rather than a silent redirect to its real store.

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
      --strict              Exit with code 2 if any document failed extraction (never aborts mid-run)
  -s, --store <NAME>        Operate on these stores (repeatable); a filter, not a selector
  -y, --yes                 Skip confirmation prompts for destructive operations
  -h, --help                Print help (see more with '--help')
  -V, --version             Print version
```

Omit `--store` and every store in the database is indexed; pass `--store`
(repeatable) to index only specific stores. Indexing more than one store
prints a `[store]`-prefixed line per store plus a combined `Total:` line
(`--json` wraps into `{"stores": [...], "total": {...}}`); a single store in
scope keeps the original unprefixed output (specs/05-surfaces.md §2.2).

Walks every registered source for the targeted store(s), extracts and chunks
documents, and writes them to the unified libsql database on disk
(`<data_dir>/localdb.db`). Progress is printed to stderr; the final summary
goes to stdout (or is omitted from stdout entirely in `--json` mode until the
summary JSON itself).

**Embeddings:** the CLI calls `embed::create_embedder` from the config policy.
The default embedder (`pplx-embed-context-v1-0.6b`, local ONNX) is downloaded
automatically on first run (~706 MB). See
[specs/04-search-pipeline.md](../specs/04-search-pipeline.md) for the pipeline.

```
$ localdb index --store notes
Indexing /home/user/notes
Index complete: 3 indexed, 0 skipped, 3 chunks written, 0 unsupported, 0 errors
```

Use `--source <ID>` to re-index a single source without touching others in the
same store.

---

## `localdb search`

Hybrid search with citations.

```
Hybrid search with citations

Usage: localdb search [OPTIONS] <QUERY>...

Arguments:
  <QUERY>...  Natural language query (no quotes needed; everything after the options is treated as the query)

Options:
      --config <PATH>
          Path to config file (default: platform data dir / localdb / config.yaml)
      --limit <LIMIT>
          Maximum number of results to return (must be >= 1) [default: 3]
      --content-length <CONTENT_LENGTH>
          Max characters of snippet text shown per result in human-readable output [default: 1000]
      --json
          Emit JSON output instead of human-readable text
  -s, --store <NAME>
          Operate on these stores (repeatable); a filter, not a selector
  -y, --yes
          Skip confirmation prompts for destructive operations
  -h, --help
          Print help (see more with '--help')
  -V, --version
          Print version
```

Omit `--store` and every store is searched; pass `--store` (repeatable) to
narrow to specific stores (specs/05-surfaces.md §2.2) — unchanged behavior,
listed here for completeness.

> **Options-first:** flags (`--limit`, `--content-length`, `--store`, `-s`,
> `--json`) must appear **before** the query words. Anything after the first
> query word is captured verbatim as query text — so `localdb search --limit 5
> rank fusion` works, but `localdb search rank fusion --limit 5` treats
> `--limit 5` as part of the query.

Runs hybrid BM25 + dense-vector search across the targeted stores and returns
ranked citations. The Citation JSON shape is documented in
[specs/02-domain-model.md](../specs/02-domain-model.md) §6.

**Ranking:** hybrid BM25 + dense (RRF fusion). With the default binary-quantized
local model, `dense` is the normalized Hamming similarity (`1.0 - hamming_dist / nbits`);
a float32 embedder yields cosine similarity instead. `fused` is the final RRF score.

**Examples:**

```
$ localdb search hybrid search
1. file:///home/user/notes/lancedb-notes.md > LanceDB notes
   LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combining vector similarity with BM25 full-text scoring.

2. file:///home/user/notes/meeting.txt
   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark connectors are deferred to the next milestone.

```

(paths shown from a scratch run)

```
$ localdb search --limit 1 rank fusion
1. file:///home/user/notes/meeting.txt
   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark connectors are deferred to the next milestone.

```

JSON output (full citation shape):

```
$ localdb search -s notes --json hybrid search
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
      "heading_path": [
        "LanceDB notes"
      ],
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

(The structural fields above — `block`, `chunk_position`, `heading_path`,
`location.span`, `snippet`, `metadata`, `chunk_id`, `resource_id` and
`provenance.content_hash` — are captured from a real indexing run. `score`,
`store` and `provenance.fetched_at` are illustrative.)

There is no top-level `document_id`, `block_seq`, `block_kind`, or `span` in the
Citation shape — those are superseded by `resource_id`, the nested `block {seq, kind}`,
`chunk_position {seq_in_block}`, and `location {span, window_block_seqs}` respectively.
See [specs/02-domain-model.md](../specs/02-domain-model.md) §6.

---

## `localdb db`

Inspect or migrate the database schema. See [docs/migrations.md](migrations.md) for
the full migration walkthrough and the migration-authoring guide, and
[specs/05-surfaces.md §2.1](../specs/05-surfaces.md#21-schema-migrations) for the
design.

```
Inspect or migrate the database schema (specs/05-surfaces.md §2.1)

Usage: localdb db [OPTIONS] <COMMAND>

Commands:
  status     Show schema version, pending migrations, and migration history
  migrate    Apply pending migrations to bring the database up to this binary's head version
  downgrade  Reverse migrations using stored down-SQL (default: one step back)
  help       Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Opening a store never migrates it — a version mismatch on open is refused (exit
`2`) with a hint pointing at one of these commands. They are the only surfaces
allowed to change a store's schema version.

**None of the three subcommands are store-scoped.** They operate on the whole
database file passed via `--config`/the default data dir, not a single named
store, so `--store`/`-s` is **rejected outright** — exit `2` — rather than
silently ignored (specs/05-surfaces.md §2.2):

```
$ localdb db status --store notes
error: invalid request: `db` commands operate on the whole database file; --store is not applicable
exit: 2
```

**All three subcommands require the daemon to be stopped.** Run against a live
daemon they exit `4` (`daemon_running`), the same as every other daemon-aware
write command — the daemon never applies migrations itself:

```
$ localdb db migrate
error: daemon is already running
exit: 4
```

### `localdb db status`

```
Show schema version, pending migrations, and migration history

Usage: localdb db status [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Read-only. Never refuses — a store newer than this binary, or one that predates
the migration framework entirely, is reportable state, not an error.

```
$ localdb db status
schema version: 4 (this binary's head: 4, baseline: 4)
up to date
history:
  v4 baseline  applied 2026-07-01T10:00:00Z  (not downgradable: baseline schema predates the migration framework; cannot downgrade below v4)
```

With pending migrations the second line becomes
``2 pending migrations; run `localdb db migrate` ``. `--json` emits
`current_version`, `head_version`, `baseline_version`, `pending`, `legacy`,
`too_new`, `uninitialized`, `table_present`, and a `migrations` history array
(per row: `version`, `name`, `applied_at`, `downgradable`,
`down_unsupported_reason`).

An existing-but-uninitialized store — a store file that opens fine but has no
schema at all yet (`PRAGMA user_version` is `0`; a zero-byte file the user
pointed at is the common case) — is reported distinctly, never as "up to
date":

```
$ localdb db status
schema version: 0 (this binary's head: 4, baseline: 4)
store exists but is uninitialized (no schema yet); any normal localdb command, or `localdb db migrate`, will initialize it to v4
```

`--json` sets `"uninitialized": true` for this case. `pending` stays `0`
rather than reporting `head_version - 0`: an uninitialized store has no
schema to incrementally apply on top of, only a fresh create (any normal
command, or `localdb db migrate`, both of which create it fresh at head) — so
callers should check `uninitialized` before treating `pending == 0` as
"nothing to do".

### `localdb db migrate`

```
Apply pending migrations to bring the database up to this binary's head version

Usage: localdb db migrate [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Applies every pending migration in ascending order, one transaction per step,
with per-step progress on stderr, then a summary:

```
$ localdb db migrate
applied migration v5 'create_auth_tables' in 12ms
migrated: v4 -> v5 (1 step applied)
```

If nothing is pending it prints `already at head (vN)` and exits `0`. If any
applied migration marks derived data stale (a re-embedding/re-extraction-class
migration), it ends with a hint — the migration itself never re-indexes:

```
hint: run `localdb index` to re-index stale content
```

An ordinary forward migration needs **no confirmation**. A legacy store
(schema v1–v3, predating the migration baseline) is the exception: migrating it
is a destructive rebuild — all indexed data is lost — so it prompts first:

```
$ localdb db migrate
This store's schema (v2) predates the migration baseline (v4); migrating it erases ALL indexed data and rebuilds from scratch. Continue? [y/N] y
rebuilt legacy store: v2 -> v4 (all indexed data erased)
```

Declining leaves the store untouched (prints `Aborted.`, exit `0`). `--yes`
skips the prompt; a non-interactive session (or `--json`) without `--yes` exits
`2` (`this command is destructive; re-run with --yes to confirm`). Exits `2`
without touching anything if the store is newer than this binary (the hint
points at `db downgrade` or upgrading localdb).

### `localdb db downgrade`

```
Reverse migrations using stored down-SQL (default: one step back)

Usage: localdb db downgrade [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --to <VERSION>   Target schema version to downgrade to (default: one step below the current version)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Steps the store's schema back to `--to <VERSION>` (default: one step, i.e. the
current version minus one) by replaying the down-SQL stored in the store's own
`schema_migrations` table — not the compiled-in chain, which is why an *older*
localdb binary can downgrade a store a newer binary migrated forward. Requires
confirmation for every *plausible* downgrade (`--yes` to skip; same
non-interactive rule as `migrate`):

```
$ localdb db downgrade --to 5
This reverses the store's schema to version 5, replaying stored down-SQL and discarding any data or structure introduced by later migrations. Continue? [y/N] y
downgraded migration v6 'add_access_requests_collected_at_column' in 3ms
downgraded: v6 -> v5 (1 step)
```

An **impossible** target — already at or below the frozen baseline (v4), or a
`--to` at or above the current version (`nothing to downgrade`) — is checked
*before* that confirmation prompt and refused immediately, exit `2`, store
untouched. It never asks "Continue? [y/N]" first: an operation that can only
fail doesn't need "are you sure":

```
$ localdb db downgrade --to 4
error: invalid config: nothing to downgrade: target version 4 must be below the current version 4
exit: 2
```

If any migration on the path to a plausible target has no down-SQL
(irreversible; its row records a `down_unsupported_reason` instead), the whole
downgrade is refused — exit `2`, nothing changed — naming the blocking
migration and the nearest reachable target. This check runs inside
`downgrade_store` itself (after confirmation), since it depends on which rows
are actually on the path, not just the target number:

```
$ localdb db downgrade --to 4
This reverses the store's schema to version 4, replaying stored down-SQL and discarding any data or structure introduced by later migrations. Continue? [y/N] y
error: invalid config: cannot downgrade past migration 'drop_chunks_block_id' (version 7): chunks.block_id cannot be reconstructed; re-index required after downgrade. Nothing was changed. Downgrade to version 7 instead (`db downgrade --to 7`) to keep it applied and only replay the migrations above it.
exit: 2
```

A store with no migration history yet (`run 'localdb db migrate' first`) is
also refused inside `downgrade_store`, after confirmation.

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
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
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

**Not store-scoped:** the daemon serves every store in the database, on `/v1`
and `/mcp` alike, so there is nothing for `--store` to narrow — passing it
exits `2` rather than being silently ignored. The check runs before the daemon
binds a port. To limit an *MCP client* to a subset of stores, scope the client
instead: `localdb mcp --store <name>` (see [docs/mcp.md](mcp.md#store-scoping)).

Exits `4` (`daemon_running`) if a daemon is already running on the same data dir:

```
$ localdb serve
error: daemon is already running
exit: 4
```

For the full HTTP API reference see [docs/http-api.md](http-api.md).

### Known limitations (v0.1.0)

- **`POST /v1/jobs` runs real ingestion.** ([#187](https://github.com/dokterbob/localdb/issues/187))
  The daemon's job endpoint runs the actual ingestion pipeline through an async,
  single-worker job queue with a per-store in-flight guard — a second submission
  for a store already running gets `index_in_progress` (409). When a daemon is
  running, `localdb index` (`cli/src/job_attach.rs`) submits a job and attaches to
  its live progress over SSE (`GET /v1/jobs/{id}/events`, falling back to polling),
  rendering an identical summary/`--json`/`--strict` to embedded mode; `--delete`
  works daemon-attached too. Stopping the daemon before `localdb index` is no
  longer necessary. Daemon-side reads (`/v1/search`, `/v1/documents/{id}`,
  `/v1/status`) see the same data, because the daemon opens the same unified
  database (`<data_dir>/localdb.db`) as the CLI.
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
Run the MCP server on stdio for use with AI agents.

Exposes every store when `--store` is omitted; pass `--store <NAME>` (repeatable) to limit the session to those stores. The limit is enforced whether the server runs embedded or proxies to a running daemon, and an unknown name exits 3. Note this is a guardrail, not a security boundary: the daemon's MCP endpoint is unauthenticated, so a client that bypasses `localdb mcp` can still reach every store.

Usage: localdb mcp [OPTIONS]

Options:
      --allow-write
          Enable write tools (reserved for future use; no effect in v1).
          
          v1 registers no mutating tool, so the tool set is identical with and without this flag; passing it prints a warning. Parsing it now makes the CLI stable for callers.

      --config <PATH>
          Path to config file (default: platform data dir / localdb / config.yaml)

      --json
          Emit JSON output instead of human-readable text

  -s, --store <NAME>
          Operate on these stores (repeatable); a filter, not a selector.
          
          Omitted, this means "all stores" for `search`, `status`, `store list`, `source list`, `source remove <ULID>`, `index` and `mcp`; the store named `default` for `source add` and the `add` alias (exit 2 if absent). `source remove <path|url>` requires it (exit 2 without it). It is rejected outright (exit 2) by `init`, `serve`, `store add`, `store remove` and the `db` subcommands, which are not store-scoped. An explicit name is always validated: unknown is exit 3. See `--help` on the specific subcommand for its exact rule.

  -y, --yes
          Skip confirmation prompts for destructive operations

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

Starts a JSON-RPC 2.0 MCP server on stdin/stdout. If no daemon is running it
uses embedded mode; if one is, it proxies to that daemon's `/mcp` route. The
server is fully functional in v0.1.0 and exposes four read-only tools:
`search`, `get_document`, `get_chunks`, and `list_stores`.

Omitting `--store` exposes every store; pass `--store` (repeatable) to limit
the session to those stores. The limit is enforced in **both** modes, and an
unknown name exits `3`:

```
$ localdb mcp --store books --store research   # only these two are reachable
$ localdb mcp --store typo
error: store not found: typo
exit: 3
```

A database with no stores at all is not an error here — the server starts and
exposes zero stores, because an MCP server that exits non-zero at startup reads
to its client as broken rather than as empty.

> **Scoping, not a security boundary.** The daemon's `/mcp` route is loopback
> and unauthenticated, so anything that can open a socket can bypass
> `localdb mcp` and talk to it unscoped. `--store` stops an agent from
> *accidentally* reading another project's docs; it does not contain a hostile
> one. See [docs/mcp.md](mcp.md#store-scoping).

`--allow-write` is accepted on the command line for forward compatibility, but
v1 registers no mutating tool at all — the tool set is identical with and
without it, and passing it prints a warning to stderr saying so.

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

## Config validation errors

Bad config files exit `2` with a path-precise message. Common cases:

| Config problem | Error message |
|---|---|
| Unknown top-level key | `invalid config: unknown field 'bogus_key', expected one of 'version', 'server', 'paths', 'defaults', 'providers'` |
| Wrong version | `invalid config: unsupported config version 2; only version 1 is supported. Hint: add 'version: 1' at the top of your config file.` |
| Source missing required field | `invalid config: stores[0].sources[0].root: required for kind 'path'` |
| Config file not found | `invalid config: cannot read config file '/path/to/config.yaml': No such file or directory` |
| Not valid YAML | `invalid config: invalid type: map, expected field identifier at line 1 column 2` |
