# localdb CLI reference

`localdb` is a local-first hybrid-search document index. This page is the complete reference for its
command-line interface.

For design decisions and process-model details see
[specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md). For the
HTTP daemon surface see [docs/http-api.md](http-api.md). For the MCP stdio surface see
[docs/mcp.md](mcp.md).

---

## Global flags

These flags are accepted by every subcommand.

| Flag                 | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--config <PATH>`    | Path to the config file. Default: the platform config dir — `~/Library/Application Support/localdb/config.yaml` on macOS, `~/.config/localdb/config.yaml` on Linux. Can also be set via the `LOCALDB_CONFIG` environment variable.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `--json`             | Emit machine-readable JSON instead of human-readable text. All JSON shapes are stable API.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `-s, --store <NAME>` | Narrow to these stores; repeatable. It is a **filter**, so omitting it means **all stores** for `search`, `status`, `store list`, `source list`, `source remove <ULID>`, `document list`, `document get`, `index` and `mcp`. Three exceptions: `source add` (and the `add` alias) defaults to the store named `default`, exit 2 if absent; `source remove <path\|url>` requires it, exit 2 without it; and `init`, `serve`, `store add`, `store remove`, `db status`/`migrate`/`downgrade`/`vacuum` **reject it outright** (exit 2) because they aren't store-scoped. An explicit name is always validated — unknown is exit 3, never silently ignored. `document get`'s omitted case can additionally be `invalid_request` (exit 2) if the id exists in more than one store — see below. See [specs/05-surfaces.md §2.2](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md#22-store-scope). |
| `-y, --yes`          | Skip confirmation prompts for destructive operations (`db migrate` legacy rebuild, `db downgrade`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `-h, --help`         | Print help.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `-V, --version`      | Print version.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |

**Environment variable:** `LOCALDB_CONFIG=<path>` is equivalent to `--config <path>`.

---

## Exit codes

Exit codes are stable API. See
[specs/05-surfaces.md §5](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md#5-shared-error-taxonomy)
for the full error taxonomy that drives them.

| Code | Meaning                 | Example trigger                                               |
| ---- | ----------------------- | ------------------------------------------------------------- |
| `0`  | OK                      | Successful command                                            |
| `1`  | Internal error          | Bug or unrecoverable runtime failure                          |
| `2`  | Invalid usage or config | Unknown subcommand, duplicate store, bad config file          |
| `3`  | Not found               | `store remove <name>` — store does not exist                  |
| `4`  | Conflict / locked       | `serve` when a daemon is already running on the same data dir |
| `5`  | Unavailable             | Daemon unreachable (stale socket)                             |

---

## `localdb init`

**Optional bootstrap — never a prerequisite.** Every other command except
`db status`/`migrate`/`downgrade`/`vacuum` scaffolds the config file and data/models/logs
directories implicitly on first use, so you never have to run `init` before `store add`,
`source add`, `index`, or `search`. Run it if you'd rather do that setup explicitly up front: it
prints every resolved path, and `--download-model` lets you pull the embedding model ahead of time
instead of deferring it to the first indexing or search operation (including `source add`'s
auto-index).

```
Optional bootstrap: write the config, create the data/models/logs directories, and print the resolved paths

Usage: localdb init [OPTIONS]

Options:
      --config <PATH>   Path to config file (default: platform data dir / localdb / config.yaml)
      --download-model  Prepare the configured embedder now, downloading a local model up front instead of on the first `index`/`search`
      --json            Emit JSON output instead of human-readable text
  -s, --store <NAME>    Operate on these stores (repeatable); a filter, not a selector
  -y, --yes             Skip confirmation prompts for destructive operations
  -h, --help            Print help (see more with '--help')
  -V, --version         Print version
```

Writes the config file (if it doesn't already exist) and creates the data/models/logs directories,
then prints all four resolved paths. The generated config file is the full commented template with
every key at its default value, not a bare stub — see
[configuration.md#config-is-created-for-you](configuration.md#config-is-created-for-you). It also
creates a store named `default`, unless the database can't be opened (see below).

**`--download-model`:** prepares the configured embedder immediately. For the default local provider
this downloads the ~706 MB model (`pplx-embed-context-v1-0.6b`, from HuggingFace, no API key or
license click-through required) right away instead of deferring it to the first indexing or search
operation (including `source add`'s auto-index). For a hosted provider (`openai-compatible`,
`perplexity`, `voyage`) it just validates that the client can be constructed (e.g. that an API key
is present). When this flag succeeds, `init` omits the "downloads its embedding model on first
index" note from its output, since it's no longer true.

**If the database can't be opened** — most commonly because it needs a schema migration — `init`
prints a `Warning: ...` on stderr and still exits `0`. It still writes the config and creates the
directories; it just skips creating the `default` store. For example:

```
Warning: invalid config: database schema version 5 is behind this build (v6); run 'localdb db migrate' to apply pending migrations
```

**Not store-scoped:** `init` runs before any store exists — the only store it creates is `default`,
which `--store` cannot rename or redirect — so passing `--store` exits `2` rather than being
silently ignored. The check runs first, so a misused flag creates no directories and writes no
config.

**Example (healthy run):**

```
$ localdb init
Initialized localdb at ~/Library/Application Support/localdb
  Config: ~/Library/Application Support/localdb/config.yaml
  Data:   ~/Library/Application Support/localdb/data
  Models: ~/Library/Caches/localdb/models
  Logs:   ~/Library/Logs/localdb

Note: the default 'local' provider downloads its embedding model on first index.
      Hosted providers (openai-compatible, perplexity, voyage) require an API key in config.
Run `localdb store add <name>` to create a store.
```

(The local-model note is omitted when `--download-model` succeeded; the `Run localdb store add` line
is omitted when the default store was skipped because the database couldn't be opened. Paths shown
are the macOS defaults.)

**`--json` output:**

```json
{
  "status": "ok",
  "config_path": "…",
  "data_dir": "…",
  "models_dir": "…",
  "logs_dir": "…",
  "default_store": "ok",
  "model_download": "skipped",
  "warnings": []
}
```

`default_store` and `model_download` are each `"ok"` or `"skipped"`.

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

**Examples** (a scratch database with two stores, `books` and `notes`, each already indexed):

```
$ localdb status
daemon: not running (embedded mode)
stores (2):
  books [libsql] 1 documents, 1 chunks
  notes [libsql] 2 documents, 2 chunks

database: /home/user/localdb/data/localdb.db
  size: 164.0 KB (+ 0 B WAL)
  ~54.7 KB per chunk (3 chunks total)
  largest tables:
    chunks_vec_idx_shadow — 32.0 KB
    sources — 24.0 KB
    resources — 16.0 KB
    chunks — 16.0 KB
    stores — 12.0 KB
```

```
$ localdb status --json
{
  "daemon": "not running (embedded mode)",
  "stores": [
    {
      "name": "books",
      "visibility": "private",
      "backend": "libsql",
      "document_count": 1,
      "chunk_count": 1
    },
    {
      "name": "notes",
      "visibility": "private",
      "backend": "libsql",
      "document_count": 2,
      "chunk_count": 2
    }
  ],
  "database": {
    "path": "/home/user/localdb/data/localdb.db",
    "exists": true,
    "size_bytes": 167936,
    "wal_size_bytes": 0,
    "total_size_bytes": 167936,
    "bytes_per_chunk": 55978,
    "largest_tables": [
      {
        "name": "chunks_vec_idx_shadow",
        "bytes": 32768
      },
      {
        "name": "sources",
        "bytes": 24576
      },
      {
        "name": "resources",
        "bytes": 16384
      },
      {
        "name": "chunks",
        "bytes": 16384
      },
      {
        "name": "stores",
        "bytes": 12288
      }
    ]
  }
}
```

(path shown from a scratch run; `document_count`/`chunk_count` appear per store, and the `database`
block reports on-disk size and a breakdown of the largest tables — useful for deciding whether
`localdb db vacuum` is worth running.)

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

Creates a store backed by libsql. Stores are persisted in the unified database
(`<data_dir>/localdb.db`) and survive restarts.

**Not store-scoped:** the store is named by the `<NAME>` argument, so passing `--store` exits `2`
rather than being silently ignored.

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

**Not store-scoped:** the store is named by the `<NAME>` argument, so passing `--store` exits `2`
rather than being silently ignored. This is checked before the confirmation prompt, so a misused
flag never gets as far as asking you to confirm a deletion.

---

## `localdb source`

Manage sources on a store. With `--store` omitted, `list` and `remove <ULID>` span **every** store —
`-s` is a filter. `add` is the exception: a write has to land in one named place, so it targets the
store named `default` and exits `2` if there isn't one. `remove <path|url>` is the other: the same
path can be a source in several stores, so it requires an explicit `--store` (specs/05-surfaces.md
§2.2).

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
      --config <PATH>          Path to config file (default: platform data dir / localdb / config.yaml)
      --refresh <REFRESH>      Refresh interval for URL and feed sources (e.g. "1h", "30m", "3600")
      --json                   Emit JSON output instead of human-readable text
      --kind <KIND>            Override source-kind classification instead of inferring it from the argument (path vs. `http(s)://` URL). `feed` treats the argument as an Atom/RSS feed URL, which fetches every entry page at index time — pass `--max-entries` to bound that [possible values: path, url, feed]
      --max-entries <N>        Cap on feed entries considered per indexing run (feed sources only)
  -s, --store <NAME>           Operate on these stores (repeatable); a filter, not a selector
      --no-fetch-full-content  For feed sources, index only the feed-supplied summary instead of fetching each entry's full page content (feed sources only)
  -y, --yes                    Skip confirmation prompts for destructive operations
  -h, --help                   Print help (see more with '--help')
  -V, --version                Print version
```

Registers one or more filesystem paths, URLs, or (with `--kind feed`) Atom/RSS feed URLs as sources
for a store. `--store` is repeatable; omit it and the source is added to the store named `default`
(exit `2` if no such store exists) — it is never guessed from whatever stores happen to exist
(specs/05-surfaces.md §2.2).

**`--kind`:** by default the source kind is inferred from the argument (a filesystem path vs. an
`http(s)://` URL). Pass `--kind feed` to treat the argument as an Atom/RSS feed instead of a plain
URL source — indexing then fetches every entry's full page content by default (bound the number of
entries considered with `--max-entries`, or index only the feed-supplied summaries with
`--no-fetch-full-content`).

This is the **one** command where omitting `--store` narrows rather than spans. Everything else
treats `-s` as a filter over all stores; a write can't, because "add this source to every store" is
not what anyone means.

**Note:** path existence is validated at registration time — `source add /does/not/exist` fails
immediately with `invalid request: path '/does/not/exist' does not exist` (exit 2), and the source
is never added.

```
$ localdb source add ~/notes --store notes
Added source 01KTVH6AY4DC84HWW7M2PP4F0X to store 'notes'
Auto-indexing source 01KTVH6AY4DC84HWW7M2PP4F0X ...
Indexing /home/user/notes
  discovered 1 files
  indexed 1 docs, 0 skipped, 0 deleted, 2 chunks
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

Omit `--store` and this lists **every** store's sources; pass `--store` (repeatable) to narrow to
one or more specific stores. A store-name column appears in the output only when more than one store
is in scope (specs/05-surfaces.md §2.2), so a single-store database and an explicit `-s <one-store>`
both keep the original column-free format.

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

A `<ID>` may be the ULID shown by `source list`, or a source's path/URL. The two shapes have
different `--store` rules, because they differ in whether they identify a store on their own
(specs/05-surfaces.md §2.2):

| Argument    | `--store` omitted                                                                            |
| ----------- | -------------------------------------------------------------------------------------------- |
| ULID        | Searches **every** store — a ULID is globally unique, so its owning store is not in question |
| path or URL | Exit `2`, asking for `--store` — the same path can be registered in several stores at once   |

```
$ localdb source remove 01KWEZN72MJ4T8Q1V3XA9BCDEF   # found wherever it lives
Removed source: 01KWEZN72MJ4T8Q1V3XA9BCDEF

$ localdb source remove ~/notes
error: source remove by path/url requires --store; pass --store <name> or use the source ULID
exit: 2
```

An explicit `--store` still hard-filters a ULID removal: if the source exists but lives outside the
named scope, this is `source_not_found` (exit `3`) rather than a silent redirect to its real store.

---

## `localdb add`

Alias for `localdb source add` — same arguments, same options, same store-scoping rule (defaults to
the store named `default`, exit `2` if it doesn't exist). See
[`localdb source add`](#localdb-source-add) above for the full reference; this section only covers
what's different.

```
Alias for `source add`: add one or more sources to a store

Usage: localdb add [OPTIONS] <SOURCES>...

Arguments:
  <SOURCES>...  Source paths or URLs (one or more)

Options:
      --config <PATH>          Path to config file (default: platform data dir / localdb / config.yaml)
      --refresh <REFRESH>      Refresh interval for URL and feed sources (e.g. "1h", "30m", "3600")
      --json                   Emit JSON output instead of human-readable text
      --kind <KIND>            Override source-kind classification instead of inferring it from the argument (path vs. `http(s)://` URL). `feed` treats the argument as an Atom/RSS feed URL, which fetches every entry page at index time — pass `--max-entries` to bound that [possible values: path, url, feed]
      --max-entries <N>        Cap on feed entries considered per indexing run (feed sources only)
  -s, --store <NAME>           Operate on these stores (repeatable); a filter, not a selector
      --no-fetch-full-content  For feed sources, index only the feed-supplied summary instead of fetching each entry's full page content (feed sources only)
  -y, --yes                    Skip confirmation prompts for destructive operations
  -h, --help                   Print help (see more with '--help')
  -V, --version                Print version
```

**On a genuinely first run** (no config file exists yet at the resolved path), `add` — like every
command except `db status`/`migrate`/`downgrade`/`vacuum` — implicitly scaffolds the config file and
data/models/logs directories, then also creates the `default` store, so `localdb add ~/notes` works
immediately with nothing set up beforehand:

```
$ localdb add ~/notes
Added source 01M0WYAT70DK1N87E6VHDJG07Y to store 'default'
Auto-indexing source 01M0WYAT70DK1N87E6VHDJG07Y ...
Indexing /home/user/notes
  discovered 1 files
  indexed 1 docs, 0 skipped, 0 deleted, 1 chunks
```

Once a config file exists, that implicit `default`-store creation no longer happens —
`add`/`source add` then requires an explicit `default` store (or an explicit `--store <name>`), same
as any other run:

```
$ localdb add ~/notes
error: invalid request: no store named 'default'; pass --store <name>
exit: 2
```

(paths and IDs shown from a scratch run)

---

## `localdb document`

Read documents indexed into a store. With `--store` omitted, `list` spans every store like
`source list`; `get` looks up the given document id across every store, disambiguating by scope when
the id exists in more than one — the same "id identifies its own store" idea as
`source remove <ULID>`, except a document id (unlike a ULID) can legitimately exist in more than one
store, so the omitted-`--store` case can be a genuine ambiguity error (specs/05-surfaces.md §2.2).

```text
Read documents indexed into a store

Usage: localdb document [OPTIONS] <COMMAND>

Commands:
  list  List documents across stores
  get   Get a single document by id
  help  Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

### `localdb document list`

```text
List documents across stores

Usage: localdb document list [OPTIONS]

Options:
      --config <PATH>       Path to config file (default: platform data dir / localdb / config.yaml)
      --source <SOURCE_ID>  Limit to documents from a specific source (by ID)
      --json                Emit JSON output instead of human-readable text
  -s, --store <NAME>        Operate on these stores (repeatable); a filter, not a selector
  -y, --yes                 Skip confirmation prompts for destructive operations
  -h, --help                Print help (see more with '--help')
  -V, --version             Print version
```

Omit `--store` and this lists **every** store's documents; pass `--store` (repeatable) to narrow.
`--source` filters to one source's documents — an unknown source id yields an empty list, not an
error. A store-name column appears in the output only when more than one store is in scope, exactly
like `source list` (specs/05-surfaces.md §2.2).

```text
$ localdb document list --store notes
a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8 file:///home/user/notes/meeting.txt

$ localdb document list --store notes --json
{
  "documents": [
    {
      "id": "a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8",
      "uri": "file:///home/user/notes/meeting.txt",
      "title": null,
      "store": {
        "name": "notes"
      },
      "store_id": "01KTVGQ62TQN8X6XN9E5FDZN67",
      "source_id": "01KTVH6AY4DC84HWW7M2PP4F0X",
      "content_hash": "e3732cc41f646a4bc94bc3611b8b6fd9d7f31f1c192748d586f55b8e7e171fd2",
      "fetched_at": "2026-08-17T20:25:09Z"
    }
  ]
}
```

(ids shown from a scratch run)

### `localdb document get`

```text
Get a single document by id

Usage: localdb document get [OPTIONS] <ID>

Arguments:
  <ID>  Document ID

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --text           Include the document's reconstructed full text in the output
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Prints the document's identity and metadata by default; pass `--text` to append its reconstructed
full text (rebuilt from persisted blocks, falling back to joined chunk text — same reconstruction
`core::documents::reconstruct_document_text` uses everywhere). `--json` always includes the text
regardless of `--text` — that flag only governs the human-readable renderer. An unknown id exits
`3`.

`--store` resolves the id's owning store with the same three-way rule as `source remove <ULID>`'s
argument shape, extended for a genuinely-ambiguous id (specs/05-surfaces.md §2.2):

| `--store` passed | Behavior                                                                                                              |
| ---------------- | --------------------------------------------------------------------------------------------------------------------- |
| none             | Looks the id up across every store; `invalid_request` (exit `2`) if it exists in more than one store                  |
| exactly one      | Scopes the lookup to that store unambiguously                                                                         |
| more than one    | Looks the id up unscoped, then checks its store against the given set (`resource_not_found`, exit `3`, if outside it) |

```text
$ localdb document get a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8
id: a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8
uri: file:///home/user/notes/meeting.txt
store_id: 01KTVGQ62TQN8X6XN9E5FDZN67
source_id: 01KTVH6AY4DC84HWW7M2PP4F0X
content_hash: e3732cc41f646a4bc94bc3611b8b6fd9d7f31f1c192748d586f55b8e7e171fd2
fetched_at: 2026-08-17T20:25:09Z
dc.format: text/plain

$ localdb document get a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8 --text
id: a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8
uri: file:///home/user/notes/meeting.txt
store_id: 01KTVGQ62TQN8X6XN9E5FDZN67
source_id: 01KTVH6AY4DC84HWW7M2PP4F0X
content_hash: e3732cc41f646a4bc94bc3611b8b6fd9d7f31f1c192748d586f55b8e7e171fd2
fetched_at: 2026-08-17T20:25:09Z
dc.format: text/plain

Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results.

$ localdb document get doesnotexist
error: resource not found: doesnotexist
exit: 3
```

Only Dublin Core fields actually present are printed (`dc.format` above; a document with richer
metadata would also show `dc.creator`, `dc.subject`, etc. — see specs/02-domain-model.md §7).

(output shown from a scratch run)

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
      --delete              Remove indexed documents that no longer exist at their source
      --refetch             Bypass the feed entry recheck gate and force a full recheck
  -s, --store <NAME>        Operate on these stores (repeatable); a filter, not a selector
  -y, --yes                 Skip confirmation prompts for destructive operations
  -h, --help                Print help (see more with '--help')
  -V, --version             Print version
```

**`--delete`:** off by default, like `rsync --delete` — indexing never removes anything unless you
ask. Without it, documents whose files were deleted (or whose URLs now 404) stay searchable, and the
run reports how many could be pruned. With it, they're actually removed (`docs_deleted` in
`--json`).

**`--refetch`:** a feed discovery entry that is already known, inside its recheck floor
(`max(source.refresh_interval_secs, 24h)`), and whose feed-supplied claim still reproduces its
stored metadata is skipped with no HTTP request at all — see
[specs/04-search-pipeline.md](https://github.com/dokterbob/localdb/blob/main/specs/04-search-pipeline.md)
§1 "Recheck gate". `--refetch` bypasses that floor check for the run and forces a full recheck of a
feed source's entries even when nothing looks stale; it's a no-op for `file`/`url` sources, since
those have no recheck gate to bypass. Deferred entries count in `docs_skipped` and additionally in
`docs_recheck_deferred`, present in `--json` output (default 0) for any store with at least one
source — a store with no sources keeps the legacy
`{"status": "ok", "message": "no sources to index"}` shape instead, which carries no counters at all
— and folded into the human-readable summary as `, N rechecks deferred` only when non-zero — always
0 on a `--refetch` run.

Omit `--store` and every store in the database is indexed; pass `--store` (repeatable) to index only
specific stores. Indexing more than one store prints a `[store]`-prefixed line per store plus a
combined `Total:` line (`--json` wraps into `{"stores": [...], "total": {...}}`); a single store in
scope keeps the original unprefixed output (specs/05-surfaces.md §2.2).

Walks every registered source for the targeted store(s), extracts and chunks documents, and writes
them to the unified libsql database on disk (`<data_dir>/localdb.db`). Progress is printed to
stderr; the final summary goes to stdout (or is omitted from stdout entirely in `--json` mode until
the summary JSON itself).

**Embeddings:** the CLI calls `embed::create_embedder` from the config policy. The default embedder
(`pplx-embed-context-v1-0.6b`, local ONNX) is downloaded automatically on first run (~706 MB). See
[specs/04-search-pipeline.md](https://github.com/dokterbob/localdb/blob/main/specs/04-search-pipeline.md)
for the pipeline.

```
$ localdb index --store notes
Indexing /home/user/notes
Index complete: 3 indexed, 0 skipped, 3 chunks written, 0 unsupported, 0 errors
```

Use `--source <ID>` to re-index a single source without touching others in the same store.

---

## `localdb search`

Hybrid search with citations.

```
Hybrid search with citations

Usage: localdb search [OPTIONS] <QUERY>...

Arguments:
  <QUERY>...  Natural language query; may be given unquoted as multiple words. A query word
              starting with `-` must be protected with `--`

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
      --path <PATH>
          Restrict to resources whose URI starts with this prefix
      --mime <MIME>
          Restrict to resources with this exact MIME type
      --added-after / --added-before <VALUE>
          Bound on when a resource was first indexed
      --updated-after / --updated-before <VALUE>
          Bound on when the store last wrote a resource's stored state
      --modified-after / --modified-before <VALUE>
          Bound on the source's own claimed last-modified time
      --document-after / --document-before <VALUE>
          Bound on a document's own claimed date (Dublin Core dc:date)
  -h, --help
          Print help (see more with '--help')
  -V, --version
          Print version
```

Omit `--store` and every store is searched; pass `--store` (repeatable) to narrow to specific stores
(specs/05-surfaces.md §2.2) — unchanged behavior, listed here for completeness.

### Filter options

`--path` and `--mime` match a literal string, with no date/duration parsing. Each of the eight
date-bound flags accepts a full RFC 3339 datetime, a partial date (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`),
or a relative duration (`7d`, `30m`, `2w`) — a duration always resolves to **now minus the
duration**, for either bound: `--modified-after 7d` means "modified within the last 7 days",
`--modified-before 7d` means "modified more than 7 days ago", never `now + duration`. In the
duration grammar `M` means months and `m` means minutes — both parse successfully, so a mistaken
capital silently produces a bound roughly 44,000 times further out. A malformed value exits 2.

| Flag                                   | Bounds                                                           | NULL rule                                                                             |
| -------------------------------------- | ---------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `--path <PREFIX>`                      | URI starts with `PREFIX` (SQL `LIKE`: `%` and `_` are wildcards) | n/a — no date parsing                                                                 |
| `--mime <TYPE>`                        | MIME type equals `TYPE` (exact string)                           | n/a — no date parsing                                                                 |
| `--added-after`/`--added-before`       | when the resource was first indexed                              | a resource with no value on this axis is **excluded**, regardless of the bound        |
| `--updated-after`/`--updated-before`   | when the store last wrote the resource's stored state            | a resource with no value on this axis is **excluded**, regardless of the bound        |
| `--modified-after`/`--modified-before` | the source's own claim of when the resource was last changed     | a resource with no claimed modification time is **excluded**, regardless of the bound |
| `--document-after`/`--document-before` | the document's own claimed date (Dublin Core `dc:date`)          | a resource with no claimed document date is **excluded**, regardless of the bound     |

`--document-*` coverage: a resource has a document date only when its source carried one — HTML
(JSON-LD or a `meta[dcterms.date\|date]`), Markdown front matter, Office (`dcterms:created`), PDF
(`/CreationDate`, or XMP `xmp:CreateDate` as a fallback), and feed entries (`published`, falling
back to `updated`). Plain text carries none, and any format's metadata may simply omit it. Combined
with the NULL rule above, a corpus whose documents mostly lack the field will narrow sharply under
`--document-*`.

Multiple filters, of any kind and in any combination, always AND together — there is no OR.

> **Flag placement:** flags (`--limit`, `--content-length`, `--store`, `-s`, `--json`) may appear
> either before or after the query words — `localdb search --limit 5 rank fusion` and
> `localdb search rank fusion --limit 5` both work. A token that looks like a flag but isn't a
> recognized one is a parse error (exit 2), not text silently folded into the query. A query word
> that legitimately begins with `-` must be protected with `--`, e.g.
> `localdb search -- -5 degrees`.
>
> This applies to `search` specifically; parse errors are always reported as plain text on stderr,
> even when `--json` is passed, since the failure happens before the flag that would select JSON
> output takes effect.

Runs hybrid BM25 + dense-vector search across the targeted stores and returns ranked citations. The
Citation JSON shape is documented in
[specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
§6.

**Ranking:** hybrid BM25 + dense (RRF fusion). With the default binary-quantized local model,
`dense` is the normalized Hamming similarity (`1.0 - hamming_dist / nbits`); a float32 embedder
yields cosine similarity instead. `fused` is the final RRF score.

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

Filtered example — only notes modified in the last 30 days:

```
$ localdb search --modified-after 30d hybrid search
1. file:///home/user/notes/lancedb-notes.md > LanceDB notes
   LanceDB is an embedded vector database built on the Lance columnar format. It supports hybrid search combining vector similarity with BM25 full-text scoring.

```

`meeting.txt` from the earlier examples is missing here for two possible reasons that look identical
from the output alone: it was modified more than 30 days ago, or its source never claims a
`modified_at` at all — the NULL rule excludes a resource with no value on the filtered axis
regardless of the bound. The same ambiguity applies to `--document-after`/`--document-before`: a
resource carries a document date only if its source supplied one, so an empty result may mean the
documents fell outside the bound or that they never declared a date at all.

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

(The structural fields above — `block`, `chunk_position`, `heading_path`, `location.span`,
`snippet`, `metadata`, `chunk_id`, `resource_id` and `provenance.content_hash` — are captured from a
real indexing run. `score`, `store` and `provenance.fetched_at` are illustrative.)

There is no top-level `document_id`, `block_seq`, `block_kind`, or `span` in the Citation shape —
those are superseded by `resource_id`, the nested `block {seq, kind}`,
`chunk_position {seq_in_block}`, and `location {span, window_block_seqs}` respectively. See
[specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
§6.

---

## `localdb db`

Inspect or migrate the database schema. See [docs/migrations.md](migrations.md) for the full
migration walkthrough and the migration-authoring guide, and
[specs/05-surfaces.md §2.1](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md#21-schema-migrations)
for the design.

```
Inspect or migrate the database schema (specs/05-surfaces.md §2.1)

Usage: localdb db [OPTIONS] <COMMAND>

Commands:
  status     Show schema version, pending migrations, and migration history
  migrate    Apply pending migrations to bring the database up to this binary's head version
  downgrade  Reverse migrations using stored down-SQL (default: one step back)
  vacuum     Reclaim disk space freed by prior migrations/deletes by rewriting the whole database file (SQLite `VACUUM`)
  help       Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Opening a store never migrates it — a version mismatch on open is refused (exit `2`) with a hint
pointing at one of these commands. They are the only surfaces allowed to change a store's schema
version.

**None of the four subcommands are store-scoped.** They operate on the whole database file passed
via `--config`/the default data dir, not a single named store, so `--store`/`-s` is **rejected
outright** — exit `2` — rather than silently ignored (specs/05-surfaces.md §2.2):

```
$ localdb db status --store notes
error: invalid request: `db` commands operate on the whole database file; --store is not applicable
exit: 2
```

**All four subcommands require the daemon to be stopped.** Run against a live daemon they exit `4`
(`daemon_running`), the same as every other daemon-aware write command — the daemon never applies
migrations itself:

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

Read-only. Never refuses — a store newer than this binary, or one that predates the migration
framework entirely, is reportable state, not an error.

```
$ localdb db status
schema version: 6 (this binary's head: 6, baseline: 4)
up to date
history:
  v4 baseline  applied 2026-08-25T17:07:04Z  (not downgradable: baseline schema predates the migration framework; cannot downgrade below v4)
  v5 drop_chunks_block_id_and_retag_resource_metadata  applied 2026-08-25T17:07:04Z  (not downgradable: chunks.block_id cannot be reconstructed; re-index required after downgrade)
  v6 shrink_vector_index  applied 2026-08-25T17:07:04Z  (downgradable)
```

(from a scratch run — all three migrations were applied at store-creation time, since a freshly
created store is built directly at head rather than replayed step by step)

With pending migrations the second line becomes ``2 pending migrations; run `localdb db migrate` ``.
`--json` emits `current_version`, `head_version`, `baseline_version`, `pending`, `legacy`,
`too_new`, `uninitialized`, `table_present`, and a `migrations` history array (per row: `version`,
`name`, `applied_at`, `downgradable`, `down_unsupported_reason`).

An existing-but-uninitialized store — a store file that opens fine but has no schema at all yet
(`PRAGMA user_version` is `0`; a zero-byte file the user pointed at is the common case) — is
reported distinctly, never as "up to date":

```
$ localdb db status
schema version: 0 (this binary's head: 6, baseline: 4)
store exists but is uninitialized (no schema yet); any normal localdb command, or `localdb db migrate`, will initialize it to v6
```

`--json` sets `"uninitialized": true` for this case. `pending` stays `0` rather than reporting
`head_version - 0`: an uninitialized store has no schema to incrementally apply on top of, only a
fresh create (any normal command, or `localdb db migrate`, both of which create it fresh at head) —
so callers should check `uninitialized` before treating `pending == 0` as "nothing to do".

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

Applies every pending migration in ascending order, one transaction per step, with per-step progress
on stderr, then a summary:

```
$ localdb db migrate
note: this migration rebuilds the vector index (~9x smaller) by re-reading the stored embeddings — no re-embedding, but it does one index insert per chunk and can take a long time on a large store.
      it does NOT shrink the file: the space it frees goes to SQLite's free list, so '/home/user/localdb/data/localdb.db' (268.0 KiB) will briefly grow before `localdb db vacuum` reclaims it.
applying 1 pending migration
applying 1/1: shrink_vector_index
applied migration v6 'shrink_vector_index' in 1ms
migrated: v5 -> v6 (1 step applied)
hint: this migration shrank the vector index but freed pages stay in the file until reclaimed — run `localdb db vacuum` to shrink it on disk
```

(from a scratch run; a migration that also rewrites large derived data — like the v6 vector-index
shrink above — prints a cost/space-tradeoff note before applying, and a matching hint pointing at
`db vacuum` afterward)

If nothing is pending it prints `already at head (vN)` and exits `0`. If any applied migration marks
derived data stale (a re-embedding/re-extraction-class migration), it ends with a hint — the
migration itself never re-indexes:

```
hint: run `localdb index` to re-index stale content
```

An ordinary forward migration needs **no confirmation**. A legacy store (schema v1–v3, predating the
migration baseline) is the exception: migrating it is a destructive rebuild — all indexed data is
lost — so it prompts first:

```
$ localdb db migrate
This store's schema (v2) predates the migration baseline (v4); migrating it erases ALL indexed data and rebuilds from scratch. Continue? [y/N] y
rebuilt legacy store: v2 -> v6 (all indexed data erased)
hint: run `localdb index` to re-index stale content
```

(the rebuild drops and recreates the schema directly at this binary's head version — v6 here — not
at the baseline; a legacy rebuild always marks derived data stale, so the re-index hint always
follows it)

Declining leaves the store untouched (prints `Aborted.`, exit `0`). `--yes` skips the prompt; a
non-interactive session (or `--json`) without `--yes` exits `2`
(`this command is destructive; re-run with --yes to confirm`). Exits `2` without touching anything
if the store is newer than this binary (the hint points at `db downgrade` or upgrading localdb).

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

Steps the store's schema back to `--to <VERSION>` (default: one step, i.e. the current version minus
one) by replaying the down-SQL stored in the store's own `schema_migrations` table — not the
compiled-in chain, which is why an _older_ localdb binary can downgrade a store a newer binary
migrated forward. Requires confirmation for every _plausible_ downgrade (`--yes` to skip; same
non-interactive rule as `migrate`):

```
$ localdb db downgrade --to 5
This reverses the store's schema to version 5, replaying stored down-SQL and discarding any data or structure introduced by later migrations. Continue? [y/N] y
downgraded migration v6 'shrink_vector_index' in 1ms
downgraded: v6 -> v5 (1 step)
```

An **impossible** target is checked and refused — exit `2`, store untouched — before that
confirmation prompt is ever shown, so a request that can only fail never asks "are you sure". Three
cases are impossible: already at or below the frozen baseline (v4); a `--to` at or above the current
version (`nothing to downgrade`); or a migration with no down-SQL (irreversible; its row records a
`down_unsupported_reason` instead) somewhere on the path to the target, in which case the error
names the blocking migration and the nearest reachable target instead:

```
$ localdb db downgrade --to 6
error: invalid config: nothing to downgrade: target version 6 must be below the current version 6
exit: 2
```

```
$ localdb db downgrade --to 4
error: invalid config: cannot downgrade past migration 'drop_chunks_block_id_and_retag_resource_metadata' (version 5): chunks.block_id cannot be reconstructed; re-index required after downgrade. Nothing was changed. Downgrade to version 5 instead (`db downgrade --to 5`) to keep it applied and only replay the migrations above it.
exit: 2
```

A store with no migration history yet (`run 'localdb db migrate' first`) is also refused inside
`downgrade_store`, after confirmation.

### `localdb db vacuum`

```
Reclaim disk space freed by prior migrations/deletes by rewriting the whole database file (SQLite `VACUUM`)

Usage: localdb db vacuum [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

A schema migration (e.g. v6 `shrink_vector_index`) or an ordinary bulk delete frees pages onto
SQLite's own free list, but the file itself does not shrink until something rewrites it —
`db vacuum` does that, via SQLite's `VACUUM`. Data-preserving (an interrupted `VACUUM` leaves the
original file untouched), but needs roughly the current file size again in free disk space and can
take minutes on a large store. No confirmation prompt, since it never discards data.

```
$ localdb db vacuum
vacuuming '/home/user/localdb/data/localdb.db': this rewrites the entire database file and needs roughly its current size again in free disk space; large stores can take minutes
vacuumed: 340.0 KiB -> 164.0 KiB (176.0 KiB reclaimed, 0.0s)
```

```
$ localdb db vacuum --json
{
  "status": "ok",
  "size_before_bytes": 167936,
  "size_after_bytes": 167936,
  "bytes_reclaimed": 0,
  "duration_ms": 2
}
```

(the warning above is always printed, even in `--json` mode, since it's progress/cost information
rather than the command's result; path and sizes shown from a scratch run — this second `--json` run
reclaims nothing because an earlier `vacuum` in the same session had already returned the store's
free pages to the OS)

---

## `localdb job`

Manage jobs on a running daemon.

```
Manage running/queued jobs on a daemon

Usage: localdb job [OPTIONS] <COMMAND>

Commands:
  cancel  Request cancellation of a queued or running job
  list    List every job on the daemon's queue, regardless of state or store
  help    Print this message or the help of the given subcommand(s)

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

**Daemon-only: there is no embedded equivalent.** An embedded (non-daemon) job lives and dies within
a single CLI invocation, so there is nothing for `job list`/`job cancel` to look up afterward. Both
subcommands exit `5` (`daemon is unreachable`) if no daemon is running:

```
$ localdb job list
error: daemon is unreachable
exit: 5

$ localdb job cancel some-job-id
error: daemon is unreachable
exit: 5
```

**Neither subcommand is store-scoped.** `--store`/`-s` is **rejected outright** — exit `2` — on
both: `cancel` operates on a job id, which is already globally unique across every store; `list`
spans every job on the queue regardless of which store it belongs to:

```
$ localdb job list --store notes
error: invalid request: `job list` shows every job regardless of store; --store is not applicable
exit: 2
```

### `localdb job list`

```
List every job on the daemon's queue, regardless of state or store

Usage: localdb job list [OPTIONS]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

With no jobs on the queue:

```
$ localdb job list
No jobs.

$ localdb job list --json
[]
```

With a completed job in history (jobs are ephemeral operational records with bounded retention, not
permanent history — see
[specs/05-surfaces.md §3](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md#3-http-api)
for the eviction policy):

```
$ localdb job list
ID                          STORE  STATE  ERROR_CODE  CREATED_AT
01M0WYCSV7HPJY5ADKQP59D8S7  notes  done   -           2026-08-25T17:09:56Z
```

```
$ localdb job list --json
[
  {
    "id": "01M0WYD5119M0MVZ6G9NZFX4JC",
    "store_id": "notes",
    "scope": {
      "type": "store"
    },
    "state": "done",
    "stats": {
      "docs_seen": 3,
      "docs_indexed": 0,
      "docs_skipped": 3,
      "docs_deleted": 0,
      "docs_prunable": 0,
      "chunks_written": 0,
      "unsupported_format_count": 0,
      "error_count": 0,
      "sources_count": 2
    },
    "error": null,
    "error_code": null,
    "created_at": "2026-08-25T17:10:07Z",
    "started_at": "2026-08-25T17:10:07Z",
    "completed_at": "2026-08-25T17:10:08Z"
  }
]
```

(job ids and timestamps shown from a scratch run against a real daemon)

### `localdb job cancel`

```
Request cancellation of a queued or running job

Usage: localdb job cancel [OPTIONS] <ID>

Arguments:
  <ID>  Job ID

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Requests cancellation of a queued or running job by id. Exit codes: `0` cancellation requested (the
daemon accepted it, HTTP 202 — cancellation is asynchronous, so this does not mean the job has
actually stopped yet), `3` the id doesn't match any job, `4` the job already reached a terminal
state (`done`/`failed` — a cancelled job lands in `failed` with `error_code: "job_cancelled"`) and
can no longer be cancelled:

```
$ localdb job cancel not-a-real-job-id
error: job not found: not-a-real-job-id
exit: 3

$ localdb job cancel 01M0WYCSV7HPJY5ADKQP59D8S7   # already 'done'
error: job already reached a terminal state; cannot cancel
exit: 4
```

(job ids shown from a scratch run against a real daemon; the exit-`0`/202-accepted case requires
cancelling a job while it's still queued or running, which needs a slower job than this reference's
scratch fixtures produce — see
[specs/05-surfaces.md §5](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md#5-shared-error-taxonomy)
and `server/src/job_exec.rs` for the cancellation contract)

---

## `localdb serve`

> **Experimental.** The HTTP daemon is an experimental preview. See limitations below.

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

Binds `127.0.0.1:7700` by default (configurable via `server.bind` / `server.port` in `config.yaml`).
Prints an announce line on startup:

```
$ localdb serve
daemon listening on http://127.0.0.1:7700
```

Also creates a Unix socket at `<data_dir>/daemon.sock` that CLI commands use to detect the daemon.

**Not store-scoped:** the daemon serves every store in the database, on `/v1` and `/mcp` alike, so
there is nothing for `--store` to narrow — passing it exits `2` rather than being silently ignored.
The check runs before the daemon binds a port. To limit an _MCP client_ to a subset of stores, scope
the client instead: `localdb mcp --store <name>` (see [docs/mcp.md](mcp.md#store-scoping)).

Exits `4` (`daemon_running`) if a daemon is already running on the same data dir:

```
$ localdb serve
error: daemon is already running
exit: 4
```

For the full HTTP API reference see [docs/http-api.md](http-api.md).

### Known limitations

- **`POST /v1/jobs` runs real ingestion.** ([#187](https://github.com/dokterbob/localdb/issues/187),
  [#208](https://github.com/dokterbob/localdb/issues/208)) The daemon's job endpoint runs the actual
  ingestion pipeline through an async job queue with a configurable worker pool
  (`server.job_workers`, default 1) and a per-store in-flight guard: jobs for _different_ stores run
  concurrently up to the pool size, but same-store jobs are always serialized — a second submission
  for a store already running gets `index_in_progress` (409). When a daemon is running,
  `localdb index` (`cli/src/job_attach.rs`) submits a job and attaches to its live progress over SSE
  (`GET /v1/jobs/{id}/events`, falling back to polling), rendering an identical
  summary/`--json`/`--strict` to embedded mode; `--delete` works daemon-attached too. `--refetch`
  also works daemon-attached, but only against a daemon that advertises the `refetch` capability in
  `GET /v1/status`'s `features` array; against an older daemon that doesn't, the CLI checks before
  submitting and fails with `daemon_capability_unavailable` (exit 5) naming the fix, rather than
  silently submitting a job the older daemon would run ordinarily gated while reporting success.
  Stopping the daemon before `localdb index` is no longer necessary. Daemon-side reads
  (`/v1/search`, `/v1/documents/{id}`, `/v1/status`) see the same data, because the daemon opens the
  same unified database (`<data_dir>/localdb.db`) as the CLI.
- **Stale socket after kill.** If the daemon process is killed without a clean shutdown,
  `daemon.sock` is not removed. Subsequent CLI commands report `daemon: running` but searches fail
  with `exit 5` (`daemon is unreachable`). Fix by removing the stale socket file:

  ```
  rm <data_dir>/daemon.sock
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

Starts a JSON-RPC 2.0 MCP server on stdin/stdout. If no daemon is running it uses embedded mode; if
one is, it proxies to that daemon's `/mcp` route. The server exposes five read-only tools: `search`,
`get_document`, `get_chunks`, `list_documents`, and `list_stores`.

Omitting `--store` exposes every store; pass `--store` (repeatable) to limit the session to those
stores. The limit is enforced in **both** modes, and an unknown name exits `3`:

```
$ localdb mcp --store books --store research   # only these two are reachable
$ localdb mcp --store typo
error: store not found: typo
exit: 3
```

A database with no stores at all is not an error here — the server starts and exposes zero stores,
because an MCP server that exits non-zero at startup reads to its client as broken rather than as
empty.

> **Scoping, not a security boundary.** The daemon's `/mcp` route is loopback and unauthenticated,
> so anything that can open a socket can bypass `localdb mcp` and talk to it unscoped. `--store`
> stops an agent from _accidentally_ reading another project's docs; it does not contain a hostile
> one. See [docs/mcp.md](mcp.md#store-scoping).

`--allow-write` is accepted on the command line for forward compatibility, but v1 registers no
mutating tool at all — the tool set is identical with and without it, and passing it prints a
warning to stderr saying so.

See [docs/mcp.md](mcp.md) for the full tool reference, input schemas, and example JSON-RPC
exchanges.

**Example** (connect via any MCP-capable client, or pipe JSON-RPC by hand):

```
localdb mcp --config ~/notes/localdb-config.yaml
```

The server reads newline-delimited JSON-RPC from stdin and writes responses to stdout. MCP clients
(Claude Desktop, etc.) handle the transport automatically.

---

## `localdb completions`

Generate a shell completion script.

```
Generate a shell completion script on stdout

Usage: localdb completions [OPTIONS] <SHELL>

Arguments:
  <SHELL>  Shell to generate completions for [possible values: bash, elvish, fish, powershell, zsh]

Options:
      --config <PATH>  Path to config file (default: platform data dir / localdb / config.yaml)
      --json           Emit JSON output instead of human-readable text
  -s, --store <NAME>   Operate on these stores (repeatable); a filter, not a selector
  -y, --yes            Skip confirmation prompts for destructive operations
  -h, --help           Print help (see more with '--help')
  -V, --version        Print version
```

Pure codegen: it prints a completion script for the named shell to stdout and exits — no config
load, no daemon probe, and it works before `init` (nothing needs to exist yet). Install by sourcing
the output into your shell's completion path:

```sh
localdb completions zsh > "${fpath[1]}/_localdb"
localdb completions bash >> ~/.bash_completion
localdb completions fish > ~/.config/fish/completions/localdb.fish
```

---

## Typical workflow

```sh
# 1. Create a runtime store
localdb store add notes

# 2. Register a source directory
localdb source add ~/notes --store notes

# 3. Index
localdb index --store notes

# 4. Search
localdb search "how does rust handle errors"

# 5. Search with JSON output for scripting (flags before the query)
localdb search --store notes --json "hybrid search"
```

---

## Config validation errors

Bad config files exit `2` with a path-precise message. Common cases:

| Config problem                | Error message                                                                                                                       |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Unknown top-level key         | `invalid config: unknown field 'bogus_key', expected one of 'version', 'server', 'paths', 'defaults', 'providers'`                  |
| Wrong version                 | `invalid config: unsupported config version 2; only version 1 is supported. Hint: add 'version: 1' at the top of your config file.` |
| Source missing required field | `invalid config: stores[0].sources[0].root: required for kind 'path'`                                                               |
| Config file not found         | `invalid config: cannot read config file '/path/to/config.yaml': No such file or directory`                                         |
| Not valid YAML                | `invalid config: invalid type: map, expected field identifier at line 1 column 2`                                                   |
