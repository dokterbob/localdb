# Spec 05 — Surfaces: CLI, HTTP API, MCP

> Status: accepted draft, revised 2026-08-12. All three surfaces sit on the same `core`
> ([01-architecture.md](01-architecture.md) §1) and return the same Citation shape
> ([02-domain-model.md](02-domain-model.md) §6) and error taxonomy (§5).

## 1. Process-model behavior shared by CLI and MCP

Every command/tool first probes the daemon socket ([01-architecture.md](01-architecture.md) §3):
daemon present → thin client over its HTTP API; absent → embedded mode (open store in-process). The
client's base URL for the HTTP API comes from the daemon's recorded discovery URL (§3), not a
hardcoded default, so this works for any configured bind address or port. The behavior difference
per command is noted below; users should rarely need to care.

## 2. CLI

Single binary, subcommand tree. Global flags: `--config`, `--json`, `--store <name>` (repeatable).

| Command                                                    | Purpose                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | Daemonless (embedded)                                                                                     | Daemon-attached                                                                                                                                                                                                                                                                                       |
| ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `init [--download-model]`                                  | Optional bootstrap: scaffolds config + data/models/logs dirs and prints every resolved path, ensures a `default` store exists (skipped with a warning if the database cannot be opened — e.g. it needs a schema migration), and with `--download-model` prepares the configured embedder now instead of on the first `index`/`search`. Every other command except `db status`/`migrate`/`downgrade`/`vacuum` scaffolds implicitly on first use (§2.5) — `init` is **never a prerequisite**. Not store-scoped, `-s` is rejected, exit 2 (§2.2) | full                                                                                                      | n/a (never contacts the daemon)                                                                                                                                                                                                                                                                       |
| `serve`                                                    | Run the daemon (HTTP API, watching, refresh, socket); serves every store regardless, so `-s` is rejected, exit 2 (§2.2)                                                                                                                                                                                                                                                                                                                                                                                                                       | becomes the daemon                                                                                        | error `daemon_running`                                                                                                                                                                                                                                                                                |
| `mcp`                                                      | Run MCP server on stdio; exposes all stores if `-s` is omitted, and `-s` genuinely narrows the exposed set in **both** modes (§4.2)                                                                                                                                                                                                                                                                                                                                                                                                           | embedded core                                                                                             | thin client                                                                                                                                                                                                                                                                                           |
| `status`                                                   | Stores, resource/chunk counts, policy staleness, daemon state, unified database file size and largest tables (§2.4); all stores if `-s` is omitted (§2.2)                                                                                                                                                                                                                                                                                                                                                                                     | reads directly                                                                                            | queries daemon                                                                                                                                                                                                                                                                                        |
| `store add/list/remove`                                    | Manage runtime-owned stores; `list` spans all stores if `-s` is omitted, `add`/`remove` name their store as an argument so `-s` is rejected, exit 2 (§2.2)                                                                                                                                                                                                                                                                                                                                                                                    | direct write                                                                                              | routed to daemon                                                                                                                                                                                                                                                                                      |
| `source add/list/remove`                                   | Manage sources on a store; `list` and `remove <ULID>` span all stores if `-s` is omitted, `add` defaults to the store named `default` (exit 2 if absent), `remove <path\|url>` requires `-s` (§2.2)                                                                                                                                                                                                                                                                                                                                           | direct write                                                                                              | `add`/`list`/`remove` all routed to daemon                                                                                                                                                                                                                                                            |
| `add <path\|url>...`                                       | Alias for `source add` — add one or more sources to a store; same `default`-store rule as `source add` (§2.2)                                                                                                                                                                                                                                                                                                                                                                                                                                 | direct write                                                                                              | routed to daemon                                                                                                                                                                                                                                                                                      |
| `document list [--store S]... [--source ID]`               | List documents across stores; all stores if `-s` is omitted, `-s` is a filter (§2.2); `--source` narrows to one source's documents; a store-name column appears whenever more than one store is in scope                                                                                                                                                                                                                                                                                                                                      | direct read                                                                                               | `GET /v1/stores/{name}/documents` per resolved store                                                                                                                                                                                                                                                  |
| `document get <id> [--store S]... [--text]`                | Look up one document by id; identity + metadata by default, `--text` appends the reconstructed full text (`--json` always includes it); unknown id is exit 3; `-s` resolves 0/1/many against the id's owning store (§2.2)                                                                                                                                                                                                                                                                                                                     | direct read                                                                                               | `GET /v1/documents/{id}?store=` (repeatable)                                                                                                                                                                                                                                                          |
| `index [--store S]... [--source ID] [--strict] [--delete]` | One-shot scan & index; submits an `IndexJob` per resolved store through the shared async job engine (`server::job_exec::run_job`); all stores if `-s` is omitted (§2.2); `--delete` works in both modes                                                                                                                                                                                                                                                                                                                                       | submits to a local, in-process job queue; live progress to stderr as the job's own progress events arrive | submits `POST /v1/jobs` (with `deletion_policy`), then attaches via `GET /v1/jobs/{id}/events` (SSE) for live progress, falling back to polling `GET /v1/jobs/{id}` every 500ms if the stream can't be established or drops; identical summary/`--json`/`--strict` output to embedded mode either way |
| `search <query>... [--limit N] [--content-length N]`       | Hybrid search with citations; `--content-length` is a **soft cap** on human-readable snippet chars (default 1000; JSON output always full text) — see §4 for the snapping behavior shared with MCP                                                                                                                                                                                                                                                                                                                                            | embedded read                                                                                             | via API                                                                                                                                                                                                                                                                                               |
| `db status`                                                | Inspect schema state: current version, head version, pending/unsupported steps. Never refuses, even on a store newer than the binary; not store-scoped, `-s` is rejected, exit 2 (§2.2)                                                                                                                                                                                                                                                                                                                                                       | reads directly                                                                                            | error `daemon_running`                                                                                                                                                                                                                                                                                |
| `db migrate`                                               | Apply pending migrations with per-step progress; legacy v1–v3 rebuild and any other destructive step require confirmation; prints a `localdb index` hint when a weight-class-3 migration ran; not store-scoped, `-s` is rejected, exit 2 (§2.2)                                                                                                                                                                                                                                                                                               | direct write                                                                                              | error `daemon_running`                                                                                                                                                                                                                                                                                |
| `db downgrade [--to N]`                                    | Reverse migrations down to version `N` (default: one step) using stored down-SQL; requires confirmation; refuses cleanly on a step with `down_unsupported_reason`; not store-scoped, `-s` is rejected, exit 2 (§2.2)                                                                                                                                                                                                                                                                                                                          | direct write                                                                                              | error `daemon_running`                                                                                                                                                                                                                                                                                |
| `db vacuum`                                                | Reclaim disk space a prior migration or bulk delete freed onto SQLite's free list but never returned to the file (e.g. after `db migrate` runs the v6 `shrink_vector_index` step) by running `VACUUM`; data-preserving, no confirmation prompt, but warns that it needs roughly the store's current size again in free disk space and can take minutes on a large store; not store-scoped, `-s` is rejected, exit 2 (§2.2)                                                                                                                    | direct write                                                                                              | error `daemon_running`                                                                                                                                                                                                                                                                                |
| `job cancel <id>`                                          | Request cancellation of a queued or running job on a daemon's job queue (issue #218); daemon-only — no embedded equivalent, so it always requires a running daemon and `-s` is rejected, exit 2 (§2.2). Exit 0 cancellation requested (`202` + the job's snapshot), exit 3 unknown job id, exit 4 job already reached a terminal state                                                                                                                                                                                                        | n/a — exit 5 (`daemon_unreachable`) without a running daemon                                              | `DELETE /v1/jobs/{id}`                                                                                                                                                                                                                                                                                |
| `job list`                                                 | List every job on a daemon's job queue, regardless of state or store; daemon-only like `job cancel` — `-s` is rejected, exit 2 (§2.2). Table columns: id, store, state, error_code, created_at; `--json` emits the raw `IndexJob[]` array `GET /v1/jobs` returns                                                                                                                                                                                                                                                                              | n/a — exit 5 (`daemon_unreachable`) without a running daemon                                              | `GET /v1/jobs`                                                                                                                                                                                                                                                                                        |
| `completions <shell>`                                      | Generate a shell completion script on stdout (`bash`, `zsh`, `fish`, `elvish`, `powershell`); pure codegen — no config load, no daemon probe, works before `init`. Unknown shell is a usage error, exit 2. Also the entry point Homebrew's `generate_completions_from_executable` calls at install time                                                                                                                                                                                                                                       | full (no config/store needed)                                                                             | same (never contacts the daemon)                                                                                                                                                                                                                                                                      |

Output: human-readable by default (citations as `uri:heading_path` + snippet), `--json` emits the
canonical structures for scripting. The CLI is **command-oriented**; interactive browse is a roadmap
item with the web UI.

**stdout/stderr contract (issue #260).** In `--json` mode, stdout is the _only_ channel guaranteed
to be pure, parseable JSON — on success (the command's own output) and on every failure localdb
itself raises (`{"error": <code>, "message": <text>}`, written by `cli::normalize::exit_err`, or the
richer partial-results shape in §2.2 for a multi-item batch). stderr carries diagnostics only —
progress lines, warnings — in both human and `--json` modes, and is never a promised source of
structured data; a caller must not parse it. This holds regardless of what emits to stderr (a
local-model download's progress bar included), because nothing there is ever expected to be
machine-readable in the first place. Two argument-parsing outcomes sit outside the envelope, in
`--json` mode as much as without it, because they are resolved before any localdb code runs: a
malformed invocation (unknown subcommand, missing argument, bad enum value) prints clap's usage text
to stderr with empty stdout and exit 2, and `--help` / `--version` print human-readable text to
stdout with exit 0.

### 2.1 Schema migrations

All schema-version mismatches on open — on every surface, CLI, HTTP daemon, and MCP alike — map to
`invalid_config` / exit 2 with an actionable hint (§5); no surface auto-migrates on open.
`db migrate` and `db downgrade` are **CLI-only**: the HTTP daemon and MCP never apply migrations,
they only ever surface the refusal-with-hint. Both commands require the daemon to be stopped — run
against a live daemon they fail the same way every other daemon-aware write command does, error
`daemon_running`, exit 4. Destructive paths (the legacy v1–v3 rebuild inside `db migrate`, and
`db downgrade`) require explicit confirmation before touching the store. See
[02-domain-model.md](02-domain-model.md) §9 for the `schema_migrations` table and the
migration-weight-class design.

A schema migration that rebuilds a large on-disk structure (e.g. v6 `shrink_vector_index`, issue
#177) frees pages onto SQLite's own free list without shrinking the database file — only `VACUUM`

(`db vacuum`) returns that space to the filesystem, and it's a separate, explicit step rather than
something `db migrate` runs automatically (it needs roughly the store's current size again in free
disk space and can take minutes). When `db migrate` applies a migration that actually freed pages,
its completion summary points the user at `db vacuum`; `db vacuum` itself is data-preserving (an
interrupted run leaves the original file untouched), so unlike the destructive paths above it warns
rather than requiring `--yes` confirmation.

### 2.2 Store scope

`--store <name>` is **repeatable**; every name passed is validated and resolved, not just the first.
An unknown name is `store_not_found`, exit 3. When explicit, `-s` always wins over any default
below. When `-s` is omitted, the default depends on the command:

| Command                                                                          | `-s` omitted                                    | Rationale                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| -------------------------------------------------------------------------------- | ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `search`, `status`, `store list`, `source list`, `document list`, `index`, `mcp` | **all stores**                                  | `-s` is a _filter_; only the _default_ changes — an explicit name is still validated and resolved (unknown → `store_not_found`, exit 3), never silently ignored                                                                                                                                                                                                                                                                                                                   |
| `source remove <ULID>`                                                           | **all stores**                                  | a ULID identifies its owning store on its own; scoping it to `default` makes a valid id fail                                                                                                                                                                                                                                                                                                                                                                                      |
| `document get <id>`                                                              | **all stores**                                  | a document id identifies its owning store on its own, like a source ULID — but unlike a ULID it can legitimately exist in more than one store (the same content indexed twice); omitted `-s` looks the id up across every store and is a cross-store ambiguity error (`invalid_request`, exit 2) if more than one store holds it, one `-s` scopes the lookup unambiguously, and more than one `-s` resolves unscoped then checks the found document's store against the given set |
| `source remove <path\|url>`                                                      | n/a — **exit 2**                                | a path/url can exist in several stores; this one really is a guess                                                                                                                                                                                                                                                                                                                                                                                                                |
| `source add`, `add` alias                                                        | **store named `default`**; **exit 2** if absent | the one write that must pick a single target, and must not pick it by guessing                                                                                                                                                                                                                                                                                                                                                                                                    |
| `store add`, `store remove`, `init`, `serve`                                     | n/a — **exit 2 if `-s` is passed**              | the store is named by the command's own argument, or there is no store concept yet, or the daemon serves every store regardless                                                                                                                                                                                                                                                                                                                                                   |
| `db status`/`migrate`/`downgrade`/`vacuum`                                       | n/a — **exit 2 if `-s` is passed**              | not store-scoped                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |

Every "exit 2 if `-s` is passed" row above exists for one reason: silently ignoring a flag the user
believed in is the #178 failure mode again.

Additional rules:

- An empty resolved store set under an all-stores policy (no stores configured at all) is exit 2,
  not a silent no-op — for `status`, `store list`, `source list`, `source remove` and `index`.
  `search` and `mcp` are the two exceptions: they resolve an empty scope and still succeed (`search`
  prints no results, exit 0; `mcp` starts and serves zero stores). A retrieval query against a fresh
  install has a correct answer — "nothing" — and an MCP server that exits non-zero at startup reads
  as a broken server to its client rather than as an empty one.
- **`source remove` has two implicit-scope rules, keyed on the shape of its argument.** A ULID spans
  all stores when `-s` is omitted (it is globally unique, so there is nothing to guess); a path/url
  exits 2 demanding `-s` (the same path can be a source in several stores at once). The argument
  shape is decided before scope resolution, so the two never interact.
- `source add`'s (and the `add` alias's) error text when the `default` store is missing is
  `no store named 'default'; pass --store <name>`. This fires even when exactly one store exists
  under a different name — predictability wins over guessing the sole store. `source list` and
  `source remove` no longer produce this error at all: they span every store instead.
- Output gains a store-name column only when more than one store is in scope; a single store in
  scope keeps the pre-existing output format so existing scripts don't break. This keys off the
  _size of the resolved scope_, not off which policy resolved it, so a bare `source list` picks up
  the column exactly when the database holds more than one store:

  ```
  $ localdb source list -s books          # 1 store in scope — unchanged
  01KWEZN72M... [path] /Volumes/Archive/books

  $ localdb source list                   # no -s — every store, so the column appears
  books    01KWEZN72M... [path] /Volumes/Archive/books
  default  01KWEXGA9Y... [path] nextcloud

  $ localdb source list -s books -s default   # >1 in scope — column appears
  books    01KWEZN72M... [path] /Volumes/Archive/books
  default  01KWEXGA9Y... [path] nextcloud
  ```

- `index` across multiple stores emits one summary per store plus a combined total. `--json` for a
  **single** resolved store stays the pre-existing flat object (unchanged, no wrapping, no `store`
  field); for **more than one** resolved store it wraps into
  `{"stores": [<per-store object, each with a "store" name field>, ...], "total": <same-shaped object>}`
  — not a bare array. `--strict` exits 2 if **any** store reported errors, but every store still
  runs to completion first — consistent with the "`--strict` never aborts mid-run" rule (see
  "`localdb index --strict`" under §5).
- **Known limitation (#188):** daemon-routed `source remove` validates `--store` only
  _syntactically_ (name shape, traversal safety) — it cannot confirm the source actually belongs to
  the named store, because `DELETE /v1/sources/{id}` is store-agnostic. Embedded (daemonless) mode
  does enforce this, since it resolves the source through the named store's own row.
- **Daemon-routed scope resolution asks the daemon, not the local database.**
  `source add`/`list`/`remove` (via their daemon branches) and `index` resolve `--store` scope by
  paginating `GET /v1/stores` to exhaustion, because a running daemon may point at a different data
  directory than the one this process would otherwise open (`LOCALDB_DAEMON_URL`) and is the
  authority on which stores exist either way. The same omitted-vs-explicit distinction from the
  table above still holds for every command that asks the daemon: an omitted `-s` resolving against
  a daemon with no store named `default` is `invalid_request`, exit 2, with the same
  `no store named 'default'; pass --store <name>` message; an _explicit_ `--store default` the
  daemon does not have is `store_not_found`, exit 3, same as any other explicit unknown name — the
  implicit default and an explicit request for that same name are not the same failure.
- **Daemon-routed `index --source ID` always verifies ownership.** `/v1/jobs` does not validate
  `source_id` (it only checks `store_name`), so submitting the same source id to every store in a
  multi-store scope would silently create one job per store, only one of which is meaningful — and
  submitting it to a single resolved store with no check at all would silently accept an id that
  store doesn't actually own. So regardless of how many stores are in the resolved scope, the CLI
  always walks `GET /v1/stores/{name}/sources` (paginating each store to exhaustion) over the scoped
  stores to find the id's owner first, submitting exactly one job. A source found in no scoped store
  is `source_not_found`, exit 3 — reproducing embedded mode's hard-filter rule that an explicit
  `--store` scope excluding the owner does not silently redirect to it, and its "unknown source id"
  rule for a single-store scope too.
- A multi-item `--json` batch (`source add`/`add` across more than one `--store`, local or
  daemon-routed) that fails partway through — after at least one earlier item already succeeded —
  does not discard the buffered results: it prints one JSON document

  ```text
  {"status": "error", "error": {"code": <error code>, "message": <text>}, "results": [<items completed so far>]}
  ```

  to stdout, then exits with the failing error's normal exit code, rather than the bare
  `{"error", "message"}` shape a single-command failure gets (§2, above). The buffered `results` are
  output data, not merely an error message, so — like every other `--json` document — they belong on
  stdout.

- `source add`'s per-item output — `{"id": ..., "store": {"name": ...}, "kind": ...}` per source,
  wrapped as described above — is identical whether the source was persisted locally or via a
  daemon; the daemon transport never echoes its own raw persisted-record response. Text mode is
  likewise identical either way (`Added source <id> to store '<name>'`, no daemon-specific suffix).

### 2.3 Feed sources

Both `localdb add <url>...` and `localdb source add <url>...` accept `--kind <path|url|feed>` to
override auto-classification. `--kind feed` requires an `http(s)://` argument — anything else is
`invalid_request`, exit 2. Two more flags apply only when the effective kind is `feed`:
`--max-entries <N>` (`0` is rejected) and `--no-fetch-full-content` (selects single-document mode,
[02-domain-model.md](02-domain-model.md) §2). Passing either flag when the effective kind is not
`feed` is `invalid_request`, exit 2 — not silently ignored. `--help` for `add`/`source add` notes
that feed ingestion in the default (discovery) mode fetches every entry's linked page and recommends
`--max-entries` to bound that.

`source list` (human) renders feed rows as `{id} [feed] {url} (max_entries=…, full_content=on|off)`
— `…` is the configured integer or `unbounded`. `--json` adds parsed `max_entries` (`null` or
integer) and `fetch_full_content` (bool), reconstructed from `config_json` (never the raw column),
and now also surfaces `refresh` for both `url` and `feed` sources. The **human** rendering's
store-name column still follows §2.2's scope rule — prepended only when more than one store is in
_the resolved scope_, independent of which of those stores actually contributed a row to the output
(a scope of two stores where only one has any sources still gets the column on that one row — issue
#187 review, finding 1). The **`--json`** `store` field (`{"name": ...}`) is different: it is

emitted **unconditionally**, on every row regardless of how many stores are in scope, matching the
pre-existing embedded behavior — there never was a single-store special case on the `--json` path,
and the feed detail above composes with it either way. A top-level, sibling `store_id` field (the
owning store's ULID, not its name) is emitted unconditionally alongside `store` on every row too
(issue #187 review, finding 2) — pre-existing embedded behavior, also documented in `docs/cli.md`'s
`source list --json` example.

### 2.4 `status` output

`status` exists to catch runaway disk usage before it becomes a surprise 45 GB (issue #179) or 350
GB (issue #177) database — nothing else in the CLI reports a store's size or chunk count. Per-store
scope follows §2.2 (`-s` omitted means all stores); each store's entry gets its
`RetrievalStore::stats()` figures (`document_count`, `chunk_count` — `core/src/store.rs`), already
the same struct the HTTP daemon and MCP `list_stores` surface. A store whose `stats()` call itself
fails reports `document_count`/`chunk_count` as `null` (human: "stats unavailable") rather than
aborting the whole command — one broken store must not blank out the report on the others.

All stores share one physical `localdb.db` file (specs/03-config.md) — file size is a property of
that file, not of any one store, so it is reported once, in a top-level `database` section, never
attached to a store entry:

- `size_bytes` — bytes in `localdb.db` itself; `null` if the file doesn't exist yet (e.g. before the
  first `store add`/`index`) or a stat of any kind fails. `status` never fails just because this is
  unknown.
- `wal_size_bytes` — bytes in the `-wal` sidecar, if one exists. WAL-mode SQLite (the mode `open`
  always sets) defers committed pages there until the next checkpoint, so on a store with recent
  writes a large share of genuine on-disk usage can live in the WAL rather than the main file.
- `total_size_bytes` — `size_bytes + wal_size_bytes` (missing components treated as 0). This, not
  `size_bytes` alone, is what the disk actually has allocated to the database right now — omitting
  the WAL would understate exactly the kind of silent growth this diagnostic exists to catch.
- `bytes_per_chunk` — `total_size_bytes` divided by the sum of every in-scope store's `chunk_count`;
  `null` when there are no chunks to divide by. This is the single number that makes an over-sized
  index obvious at a glance — a `chunk_count` in the thousands next to a `bytes_per_chunk` in the
  hundreds of KB is the signature both #179 and #177 would have shown from the start.
- `largest_tables` — up to 5 `{name, bytes}` rows, the largest on-disk tables (own pages plus every
  index built on them) via SQLite's `dbstat` virtual table, descending. Best-effort: if `dbstat`
  querying fails for any reason, this is an empty array rather than a command failure.

`--json` extends the pre-existing shape — `daemon` and `stores[].{name,visibility,backend}` are
unchanged — by adding `stores[].{document_count,chunk_count}` and the top-level `database` object
above; no existing field is renamed or removed.

### 2.5 Implicit initialization

`localdb init` is no longer a prerequisite. Every command whose config-load path is
`load_config_scaffolded` (strict) or `load_config_lenient` (lenient) — `store add`/`remove`,
`source add`/`list`/`remove`, `index`, `mcp`, `serve` (strict); `search`, `status`, `store list`
(lenient) — creates the config file, `paths.data`/`models`/`logs`, and a `default` store on a
genuine first run, exactly as `localdb init` does explicitly (specs/03-config.md §8). `init` itself
is now that same scaffolding step run explicitly, with repair semantics on top: it re-checks and
ensures the `default` store exists — even against a config file another command already scaffolded
or a user hand-wrote — unless the database cannot be opened, in which case it skips that step and
warns instead (below).

`db status`/`migrate`/`downgrade`/`vacuum` are the deliberate exception: they exist to inspect or
repair an _existing_ store's schema, so they keep failing (exit 2) on a fresh install rather than
scaffolding one into existence underneath themselves — their config-load path
(`load_config_for_maintenance`) never scaffolds.

Scaffolding fires only when the resolved config path does not exist at all; a present-but-malformed
file is left untouched, and the command's normal strict/lenient load surfaces the same parse error
it always did (§5, `invalid_config`). Exit codes are unchanged: a scaffolding failure — an explicit
`--config` whose parent directory doesn't exist, or an I/O failure creating the directories or
writing the config file — maps through the same `Error::InvalidConfig` -> exit 2 path every other
config error already used.

Commands forced to a daemon via `LOCALDB_DAEMON_URL` never touch the local database during
scaffolding: they still write the config file on a first run, but the `default` store belongs to
whichever store registry the command actually acts on, and that daemon may not share the local
`localdb.db` at all. That first run therefore leaves the install config-present but DB-absent; it is
not stranded — the local `default` store is created by the next locally-routed command that finds
the config still byte-identical to the scaffolded template (i.e. carrying no user intent yet) and no
`localdb.db`. A hand-written or edited config never triggers this seeding, and once any store has
ever been created (the DB file exists) a removed `default` store stays removed. `serve` always
counts as locally routed: it starts a local daemon regardless of `LOCALDB_DAEMON_URL`, so the
variable never suppresses its seeding.

`init` does two things no implicit path does: it prints every resolved path (config, data, models,
logs) even when nothing changed, and with `--download-model` prepares the configured embedder,
downloading a local model up front. If the database cannot be opened — most commonly because it
needs a schema migration (§2.1) — `init` still scaffolds config + directories and reports them, but
skips creating the `default` store and prints the open error as a warning. It exits 0 either way:
`init` never fails merely because an existing store needs maintenance.

## 3. HTTP API

**Decision:** **REST + JSON, the canonical surface for external integrators.** Served only by the
daemon. **Rejected:** gRPC (worse curl-ability and browser story for a local tool; can be added
later if a consumer demands it).

- **Bind & trust:** `127.0.0.1` by default, **no auth in local mode** — documented trust assumption:
  anything that can reach the bind address is trusted, same boundary as the files themselves. Any
  bind address is accepted; the daemon does not refuse to start based on it. Binding to a specific
  non-loopback address (e.g. a LAN or VPN IP) is treated as a deliberate trust decision by the user
  and starts silently. Binding to all interfaces (`0.0.0.0`, `::`, or any other address form the OS
  resolves to the unspecified address) logs a warning at startup — checked against the address the
  OS actually bound, not the raw config string, so aliases the string form can't see are still
  caught — since it makes the unauthenticated daemon reachable from any network the machine is on
  and is the one case a user could plausibly not realize how exposed this makes them. The daemon
  also records its client-reachable base URL (loopback substituted for a wildcard bind) in a
  discovery file so CLI/MCP clients can find it regardless of bind address or port
  ([01-architecture.md](01-architecture.md) §3).
- **Resources** (`/v1`): `GET/POST /stores`, `GET/PATCH/DELETE /stores/{name}`,
  `GET/POST /stores/{name}/sources`, `GET /stores/{name}/documents`, `POST /search` (body: query,
  store filter, metadata filters, limit; citations carry full `Metadata`),
  `GET /documents/{id}?store=` (repeatable; response includes `metadata: Metadata`),
  `GET/POST /jobs` (the former lists every job regardless of state or store; the latter submits an
  index request), `GET/DELETE /jobs/{id}` (the latter cancels, issue #218), `GET /jobs/{id}/events`
  (SSE, below), `GET /status`, `GET /config` (resolved config). **Jobs are ephemeral operational
  records with bounded retention, not history**: the registry keeps every `pending`/`running` job,
  but caps how many terminal (`done`/`failed`) jobs it retains at `MAX_TERMINAL_JOBS` (200,
  `server::job_queue`) — once a terminal write pushes the terminal count over the cap, the oldest
  terminal jobs by `completed_at` are evicted first, so `GET /jobs` never grows unbounded in a
  long-running daemon. Jobs terminal for less than a retention grace
  (`TERMINAL_RETENTION_GRACE_SECS`, 60s) are never evicted, even over the cap — a client that just
  received its id from `POST /jobs` must always be able to resolve it on its first
  `GET /jobs/{id}`/`GET /jobs/{id}/events` request, even if the job completed (and a burst of other
  completions landed) before that request arrived; the cap is therefore a target the registry
  returns to as entries age past the grace — trimmed on the next terminal write or `GET /jobs` read,
  whichever comes first — not a hard ceiling during a burst. No pagination on `GET /jobs` this round
  — the response stays bounded anyway: the non-terminal set is capped by the per-store in-flight
  guard (at most one `pending`/`running` job per store), and the terminal set by the cap plus at
  most one grace window's worth of burst; this may be revisited if the cap itself is ever made
  configurable/larger. Store records (`GET/POST /stores`, `GET /stores/{name}`) include `id`
  alongside `name`/`visibility`/`backend`. Despite the `{name}` path param, stores are still looked
  up and returned with their `id` intact — `{name}` is only how the route addresses _which_ store,
  not a claim that `id` is dropped from the shape.
- **Document reads:** `GET /stores/{name}/documents?source=&cursor=&limit=` lists every document
  registered in a store, in the same paginated envelope as `GET /stores/{name}/sources` (below).
  `?source=` is a pure filter, not a lookup — an unknown source id yields an empty page rather than
  an error, matching `StoreBackend::list_documents`'s "no error on a miss" contract for read paths;
  an unknown store name is `store_not_found`, 404. `GET /documents/{id}?store=` (repeatable) looks
  up a single document by id: the response is a `DocumentRecord` (`id`, `uri`, `title`, `store_id`,
  `source_id`, `content_hash`, `fetched_at`, `normalized_text`, `metadata`) — `normalized_text` is
  the document's reconstructed full text, always present in the response (there is no daemon-side
  equivalent of the CLI's `--text` flag; that's purely a rendering choice on the CLI side, §2).
  `?store=` gives the same 0/1/many scoping as the CLI's `-s` on `document get` (§2.2): omitted
  looks the id up across every store (`invalid_request`, 400, if more than one store holds it),
  exactly one name scopes the lookup unambiguously, and more than one name resolves unscoped then
  checks the found document's store against the given set; an unknown store name is
  `store_not_found`, 404. This store-name disambiguation exists in the daemon from day one — unlike
  #188's known gap on daemon-routed `source remove --store` (syntactic-only validation), `?store=`
  here is resolved by the handler itself against the real store registry, so the CLI's `-s`
  semantics on `document get` hold identically whether attached to a daemon or running embedded.
- **Feed sources:** `POST /stores/{name}/sources` accepts
  `{kind: "feed", spec: {url, max_entries, fetch_full_content}, preset, refresh}` — `spec` mirrors
  `SourceSpec::Feed` ([02-domain-model.md](02-domain-model.md) §2). Validation failures
  (`max_entries: 0`, a non-`http(s)` `url`, etc.) are `invalid_request`, 400. `GET .../sources`
  reconstructs a clean `spec` object per source from `config_json` (never the raw column) and now
  surfaces `refresh` for both `url` and `feed` sources. Feed's `refresh` is persisted and validated
  the same as `url`'s, but only `url`-source scheduled refresh is actually live: the daemon's
  URL-refresh scheduler (`server::scheduler`) polls every 60s and submits a real job through the
  same job engine `POST /jobs` uses for any `url` source past its `refresh` interval. Feed-source
  scheduled refresh is not yet wired — the scheduler only ever registers `SourceKind::Url` sources,
  so a feed source's `refresh` is persisted and round-tripped but has no effect until a manual
  `POST /jobs` (or CLI `index`) runs.
- **Long-running work:** indexing is a **job resource**, and `POST /jobs` runs the real ingestion
  pipeline (`server::job_exec::run_job`) through an async job queue with a configurable worker pool
  (`server.job_workers`, default 1, issue #208) — not a stub. Body:
  `{store_name, source_id?, deletion_policy?}`; `store_name` is required, `source_id` narrows the
  job to one source (omit to index the whole store), `deletion_policy` is `"retain"` (default —
  nothing is ever removed) or `"delete"` (prunes documents no longer present at their source,
  mirroring CLI `index --delete`) — any other value is `invalid_request`, 400. `POST /jobs` →
  `202` + the created `IndexJob`. A second `POST /jobs` for a store that already has a job queued or
  running is rejected with `index_in_progress`, 409 (§5) — same-store submissions always conflict,
  regardless of worker count; a per-store in-flight guard is reserved atomically at submit time,
  before the job is created, so two concurrent submissions for the same store can never both
  proceed. Jobs for _different_ stores run concurrently, up to `server.job_workers` workers sharing
  one queue (issue #208) — all workers pull from the same channel, so cross-store jobs genuinely
  overlap while the per-store guard keeps same-store jobs serialized no matter how many workers are
  configured. Embedded (non-daemon) CLI indexing always runs its own single-worker queue and never
  reads `server.job_workers`. The URL-refresh scheduler submits jobs through this same engine, not a
  separate code path. Clients poll `GET /jobs/{id}` for the current `IndexJob` (state
  `pending`/`running`/`done`/`failed`, `stats`, `error`, `error_code`, timestamps) or stream
  `GET /jobs/{id}/events` for live progress (below). Because job records are ephemeral with bounded
  retention (above), `GET /jobs/{id}` for a job id that has aged out past the terminal-job cap
  returns `404 job_not_found` — the same response as an id that never existed; a client that stops
  polling a terminal job and comes back much later should not assume a `404` means the id was
  invalid. `error_code` (issue #187 review, finding 3) is the failing `core::Error`'s stable
  `code()` string (§5) when the job's `Failed` state came from a typed error — `null`/absent for a
  synthetic queue-level failure (the queue itself full/closed, or the job's task panicking) that
  never had one, and always absent on `done`. `error_code` + `error` round-trip through the same
  `code -> Error` mapping a daemon HTTP error body's `code` field does (§5), so a daemon-attached
  CLI client reconstructs the original typed error and exits with the same code an equivalent
  embedded failure would, instead of collapsing every job failure to a generic internal error.
  `#[serde(default)]`, so a daemon predating this field omits the key entirely rather than sending
  `null`.
- **`DELETE /jobs/{id}`** (issue #218): requests cancellation of a queued or running job. `202` +
  the job's snapshot at the moment cancellation was requested — not a guarantee it has already
  stopped; poll `GET /jobs/{id}` or watch `GET /jobs/{id}/events` for the eventual terminal state.
  `404 job_not_found` for an unknown job id. `409 job_already_terminal` for a job that already
  reached `done` or `failed` — cancellation must never overwrite a recorded outcome, so
  `JobQueue::cancel` checks the registry's terminal state before ever touching the job's
  cancellation token. Pending jobs cancel without ever starting; a running job's task is aborted and
  its teardown (issue #217's transaction rollback) is awaited before the per-store in-flight guard
  is released, so a cancelled write is data-safe the same way a crash mid-write already was.
  **Deliberate design decision:** cancellation does not add a fifth `IndexJobState` — it reuses the
  existing `Failed` terminal state with `error_code: "job_cancelled"`, reconstructed via
  `Error::from_code` (§5) exactly like any other typed job failure. This means every surface that
  already renders a `Failed` job (attach polling, SSE, `--json`) needs no changes to display a
  cancellation; the CLI's `job cancel` gets its own exit code (4) only because
  `core::Error::JobCancelled`'s `exit_code()` says so, not because of any special-casing. **A `202`
  is not a guarantee the job was actually interrupted**: `409` is returned only when
  `JobQueue::cancel`'s own two registry reads (before and after triggering the token) observe the
  job already terminal for a reason unrelated to this call — in that case the recorded outcome is
  left untouched. Otherwise the call returns `202` and triggers the cancellation token, but the job
  can still go on to reach `done`, or `failed` for an unrelated reason, in the moment immediately
  after the response is sent — the HTTP response and the job's eventual terminal state are decided
  independently, not atomically together. Callers must always inspect the job's actual terminal
  state (`GET /jobs/{id}` or `GET /jobs/{id}/events`) rather than assume a `202` implies the job
  stopped; only `error_code: "job_cancelled"` on that terminal state confirms cancellation actually
  took effect. Cancellation takes effect at the task's next `.await` yield point, not instantly — a
  CPU-bound phase (parsing, embedding inference) runs to the end of its current operation before the
  worker observes the cancellation, so a `202` may precede the terminal state by roughly the length
  of that operation; deeper preemption of a blocking phase is a known follow-up, not implemented
  here.
- **`GET /jobs/{id}/events`** (SSE, issue #83): streams the job's live progress as
  `text/event-stream`. Each in-flight update is an `event: progress` frame whose `data:` is one
  JSON-serialized `core::ProgressEvent` (internally tagged `type`: `source_started`, `discovered`,
  `document_started`, `document_finished`, `source_finished`). The stream always ends with exactly
  one `event: job` frame carrying the terminal `IndexJob` (state `done` or `failed`) as `data:`,
  after which the connection closes — there is no further `progress` frame after the `job` frame. A
  subscriber that connects after the job has already reached a terminal state (or after its live
  channel has already been torn down) gets _only_ that terminal `job` event, immediately — it never
  sees the `progress` events it missed; progress delivery is lossy/best-effort by design (a lagging
  subscriber skips buffered events rather than stalling the stream or growing memory unboundedly),
  but the terminal event is always guaranteed exactly once. Unknown `job_id` → `job_not_found`, 404.
  `GET /jobs/{id}` and `GET /jobs/{id}/events` report the identical terminal `IndexJob` shape.
- **`GET /status`** returns, beyond the pre-existing `daemon`/`store_count`/`source_count`/
  `job_count`: a `stores[]` array with one entry per store (`name`, `visibility`, `backend`,
  `document_count`, `chunk_count` — the latter two `null` if that store's `RetrievalStore::stats()`
  call itself failed, mirroring the embedded CLI's `status`, specs §2.4) and a top-level `database`
  object (`path`, `exists`, `size_bytes`, `wal_size_bytes`, `total_size_bytes`, `bytes_per_chunk`,
  `largest_tables`) describing the one shared `localdb.db` file — same shape and same fields as the
  embedded CLI's `status --json` (§2.4), so daemon-routed and embedded `status` render identically.
  - **Per-store source listing is best-effort, exactly like the adjacent per-store stats call**
    (issue #187 review, finding F7): a store whose source listing fails (e.g. a corrupt or
    mid-migration store) still gets an entry in `stores[]` with `document_count`/`chunk_count`
    reflecting whatever `RetrievalStore::stats()` managed, but contributes nothing to `source_count`
    rather than failing the whole response — one broken store must not blank out the report on every
    other store, the same rule the stats call already followed.
  - **`?store=` (repeatable) scopes the response to specific stores**, mirroring CLI `--store`
    (§2.2): `GET /status?store=a&store=b` gathers and reports only stores `a` and `b`; stores
    outside the requested set are never queried (neither their sources nor their
    `RetrievalStore::stats()`) — the same "gather only what's in scope" behavior the embedded CLI's
    `resolve_store_scope_inner` already gives `status --store`. `store_count`, `source_count`, and
    the `database` object's `bytes_per_chunk` (derived from the scoped stores' chunk counts) are
    computed over the subset; `job_count` and the rest of `database` (the shared file's own
    size/table figures) are unaffected by scope — jobs and the on-disk file aren't per-store
    resources. An unknown store name is `store_not_found`, HTTP 404 — parity with the embedded CLI's
    exit code 3 for an unresolvable explicit `--store` name. A name that resolves to a real but
    broken store degrades best-effort like the unscoped case, it does not 404. Omitting `?store=`
    entirely is the pre-existing all-stores behavior.
  - **Scope resolution reads raw store rows and does not require every store's indexing policy to
    parse** (Codex review, issue #187 PR #212 finding G2): resolving `?store=` (or listing every
    store when it's omitted) reads `name`/`id`/`visibility`/`backend` straight off the DB-backed
    store row — `status` never reads a store's parsed indexing policy, so it must not fail just
    because one is malformed. A malformed `indexing_policy` — on the requested store itself, on a
    store outside the requested scope, or on any store in the unscoped, all-stores case — must never
    fail the request; it degrades best-effort exactly like a store whose source listing or
    `RetrievalStore::stats()` call fails.
- **Pagination:** cursor-based (`?cursor=`, `?limit=`) on list endpoints from day one.
- **`POST /search` limit clamp and cursor overflow (issue #187 review, finding G3):** `limit` is
  silently clamped to a maximum of 100, matching the MCP `search` tool's own cap — the same
  `SEARCH_MAX_LIMIT` idiom (§4, `mcp/src/tools.rs::resolve_search_limit`): a request for more than
  100 gets 100 back, not an error. A `cursor`/`limit` combination whose page end (`cursor + limit`)
  overflows `usize` is rejected as `invalid_request`, HTTP 400, rather than panicking or silently
  wrapping — a client cannot use an out-of-range cursor to crash or confuse pagination.
  `GET /stores` and `GET /stores/{name}/sources` compute the same `offset + limit` internally but
  treat an overflow there as end-of-list (`next_cursor: null`) instead of an error, since an
  offset/limit pair that large can never address a real page of either list.
- **`?limit=0` is rejected, not clamped:** `GET /stores`, `GET /stores/{name}/sources`, and
  `GET /stores/{name}/documents` all reject an explicit `limit=0` as `invalid_request`, HTTP 400
  (`server::handlers::parse_limit`) — a zero limit would otherwise truncate every page to empty
  while `next_cursor` keeps advancing by the unchanged offset, so a client following cursors would
  loop forever on the same empty page. Matches the MCP `list_documents`/`get_chunks` tools' own
  `resolve_limit`, which rejects the same input for the same reason rather than clamping it up to 1.
- **`GET /stores/{name}/documents` paginates in the backend query, not in memory:** `limit`/`offset`
  are pushed into `StoreBackend::list_documents`'s SQL (`LIMIT`/`OFFSET`), and the paginated
  envelope's `total` comes from a separate `StoreBackend::count_documents` query, rather than
  loading and deserializing every document in the store per page request. `document list`'s embedded
  CLI path and the MCP `list_documents` tool push their own pagination down the same way.
- **`localdb search` clamps identically in embedded and daemon mode (issue #187 review, finding
  1):** the 100-item cap above is not a `/v1/search`-only concern — `localdb search --limit <huge>`
  must return the same number of results whether or not a daemon happens to be running, since that
  is this PR's whole point. The cap lives in exactly one place,
  `localdb_core::search::SEARCH_MAX_LIMIT` (plus its `clamp_search_limit` helper), and every surface
  that accepts a client-supplied result count clamps to it: `POST /v1/search`
  (`server::search_service::clamp_search_limit`, now a thin re-export), the MCP `search` tool (§4,
  `mcp::tools::resolve_search_limit`, which clamps its own `Option<i64>` against the same constant),
  and the CLI's embedded `search` command (`cli::cmds::search::SearchCmd::run_embedded`, which
  clamps `self.limit` before it becomes `QueryRequest::top_n`). The CLI's daemon-attached branch
  sends its raw, unclamped `limit` in the request body — `/v1/search` clamps it there, so no second
  clamp is needed on the way out.

## 4. MCP

**Decision:** v1 MCP is **read-only**: tools `search` (args: query, optional store names, limit,
optional content_length → Citation list as structured content; each citation carries full
`Metadata`), `get_document` (id or uri, optional store → block texts + `metadata: Metadata`),
`get_chunks` (resource_id, optional store, optional offset/limit, or optional
anchor_chunk_id/anchor_block_seq (§4.1) → the resource's chunks in order, paginated), `list_stores`
(names, visibility, counts), `list_documents` (args: store required, optional source, optional
offset/limit → `{store: {id, name}, total, offset, limit, returned, documents: DocumentInfo[]}`,
paginated the same way as `get_chunks`). **Mutating tools** (`add_source`, `reindex`, …) are a
follow-up behind an explicit opt-in flag (`localdb mcp --allow-write`), never on by default.

Because v1 registers no mutating tool at all, `--allow-write` currently has **no effect**: the tool
set is byte-identical with and without it. Passing it prints a non-fatal stderr warning saying so,
rather than exiting 2 the way a misapplied `-s` does. The asymmetry is deliberate — `-s` failing
open would silently _widen_ access, whereas `--allow-write` failing closed can only withhold a
capability the caller notices immediately as a missing tool, so refusing to start an MCP server over
it would be disproportionate.

**Rationale:** the dominant agent use case is retrieval; a read-only surface has a trivially
auditable blast radius, and write semantics through agents deserve their own design pass.
**Rejected:** full CRUD via MCP in v1.

Citations cross MCP as structured tool results (the JSON shape from
[02-domain-model.md](02-domain-model.md) §6), with a short text rendering alongside for
non-structured clients (text rendering includes `creator · date` where present). Resources/prompts:
none in v1; resources are reachable via `get_document` / `get_chunks`.

**Store disambiguation (#144).** `get_document` and `get_chunks` both accept an optional `store`
argument: a store **id or name**, resolved the same way as `search`'s `stores` argument. A `search`
citation carries the store it came from (`store.id` / `store.name`), so a client can round-trip a
citation back into either tool without ambiguity. Unknown store → `store_not_found`. When `store` is
**omitted**, both tools scan every available store and return the first match — the pre-#144
behavior, retained for backward compatibility. Omitting it when the same document id exists in more
than one store is therefore a coin flip; pass `store` whenever the id's origin is known.

### 4.1 `get_chunks`

Returns a resource's chunks in storage order — `(block_seq, seq_in_block)` — with pagination. Args:
`resource_id` (required), `offset` (integer ≥ 0, default 0), `limit` (integer 1..=200, default 50).
Like `get_document`, `uri`-based lookup is not supported in v1 — callers must use a resource ID
obtained from a prior `search` or `get_document` call. Unknown `resource_id` → `resource_not_found`.
An `offset` past the end of the chunk list returns an empty `chunks` array, not an error — this is
not a usage mistake worth surfacing as one.

**Anchor-relative pagination (#146):** as an alternative to `offset`, `get_chunks` accepts
`anchor_chunk_id` (string) or `anchor_block_seq` (integer ≥ 0). `offset`, `anchor_chunk_id`, and
`anchor_block_seq` are mutually exclusive — passing more than one of the three is a tool-level
`invalid_request` error, not a silent precedence rule.

Anchor resolution runs over the resource's full chunk list, sorted the same way as the
plain-`offset` path — `(block_seq, seq_in_block)`:

- `anchor_chunk_id` resolves to the chunk with that exact `chunk_id`. Unknown `anchor_chunk_id` →
  `chunk_not_found`.
- `anchor_block_seq` resolves via lower-bound: the first chunk with `block_seq >= anchor_block_seq`,
  tie-broken by the lowest `seq_in_block` at that `block_seq`. If `anchor_block_seq` is past every
  block in the resource (no chunk satisfies the lower-bound), this is also `chunk_not_found`.

Once an anchor resolves to a position in the full chunk list, the response window is `limit` chunks
**centered** on that position — the anchor sits at, or as close as possible to, the middle of the
returned page — clamped at the start/end of the resource's chunk list. The window never shrinks
below `limit` chunks purely because the anchor is near an edge (it shifts toward the interior
instead); it only returns fewer than `limit` chunks when the resource has fewer than `limit` chunks
in total. The response's `offset` field reports the effective offset the returned window corresponds
to (as if the caller had passed that `offset` directly), and a new `anchor_index` field reports the
0-based index of the anchor chunk within the returned `chunks` array — `null` when the request used
plain `offset` pagination instead of an anchor.

Response shape (plain `offset` pagination):

```json
{
  "resource_id": "...",
  "uri": "...",
  "title": "...",
  "store": { "id": "...", "name": "..." },
  "total_chunks": 0,
  "offset": 0,
  "limit": 0,
  "returned": 0,
  "anchor_index": null,
  "chunks": [
    {
      "chunk_id": "...",
      "block_seq": 0,
      "seq_in_block": 0,
      "block_kind": "...",
      "span": { "start": 0, "end": 0 },
      "heading_path": ["..."],
      "text": "..."
    }
  ]
}
```

`span` is block-relative, not a partition of the block: adjacent chunks' spans are not guaranteed
contiguous — see [02-domain-model.md](02-domain-model.md) §"Span semantics".

**Anchor example:** a resource with 20 chunks (`block_seq` 0–19, one chunk per block), requested
with `anchor_chunk_id` set to the `block_seq = 10` chunk and `limit: 5`. With an odd `limit`,
centering puts 2 chunks before the anchor and 2 after, so the returned window covers `block_seq`
8–12, `offset` is 8 (the position of the first returned chunk in the full ordered list), and the
anchor is the 3rd of the 5 returned chunks (`anchor_index: 2`):

Request:

```json
{ "resource_id": "...", "anchor_chunk_id": "...", "limit": 5 }
```

Response:

```json
{
  "resource_id": "...",
  "uri": "...",
  "title": "...",
  "store": { "id": "...", "name": "..." },
  "total_chunks": 20,
  "offset": 8,
  "limit": 5,
  "returned": 5,
  "anchor_index": 2,
  "chunks": [
    {
      "chunk_id": "...",
      "block_seq": 8,
      "seq_in_block": 0,
      "block_kind": "...",
      "span": { "start": 0, "end": 0 },
      "heading_path": ["..."],
      "text": "..."
    },
    {
      "chunk_id": "...",
      "block_seq": 9,
      "seq_in_block": 0,
      "block_kind": "...",
      "span": { "start": 0, "end": 0 },
      "heading_path": ["..."],
      "text": "..."
    },
    {
      "chunk_id": "...",
      "block_seq": 10,
      "seq_in_block": 0,
      "block_kind": "...",
      "span": { "start": 0, "end": 0 },
      "heading_path": ["..."],
      "text": "..."
    },
    {
      "chunk_id": "...",
      "block_seq": 11,
      "seq_in_block": 0,
      "block_kind": "...",
      "span": { "start": 0, "end": 0 },
      "heading_path": ["..."],
      "text": "..."
    },
    {
      "chunk_id": "...",
      "block_seq": 12,
      "seq_in_block": 0,
      "block_kind": "...",
      "span": { "start": 0, "end": 0 },
      "heading_path": ["..."],
      "text": "..."
    }
  ]
}
```

If the same `anchor_chunk_id` (`block_seq = 10`) were requested with `limit: 30` against this
20-chunk resource, the window would clamp to the whole list: `offset: 0`, `returned: 20`,
`anchor_index: 10`.

`content_length` (default 400) is a **soft cap**, not a hard truncation point: the JSON citation
payload always carries the full, untruncated snippet — only the human-readable text rendering is
shortened. The text rendering snaps its cut point to the nearest natural boundary at or below the
cap, checked in priority order: paragraph break (`\n\n`) → sentence terminator (`.`/`!`/`?`,
optionally followed by a closing quote/bracket, then whitespace or end-of-text) → word boundary
(last whitespace at or before the cap) → hard UTF-8 char-boundary cut as a last resort. A bounded
overshoot (up to ~20% over the cap) is allowed so a paragraph/sentence boundary just past the cap is
preferred over a mid-word hard cut; word/char fallback never overshoots. An ellipsis (`…`) is
appended whenever the snippet was actually shortened. This logic lives in `core`
(`localdb_core::snippet::truncate_snippet`) and also backs the CLI's `--content-length` (§2) — the
CLI additionally collapses whitespace before truncating, which removes `\n\n` paragraph breaks, so
only sentence/word snapping applies on that path. `context_sentences` (an alternative
sentence-count-based unit) is out of scope for this design.

### 4.2 Transports and process model

MCP is served over two transports, built on the official `rmcp` SDK:

- **Stdio** (`localdb mcp`): if no daemon is running, the CLI opens the store(s) embedded in-process
  and serves them directly. If a daemon is already running (detected the same way every other
  daemon-aware CLI command detects it, §1), `localdb mcp` instead **proxies** every request to that
  daemon's own `/mcp` HTTP route below, rather than opening the store a second time. Absent
  `--store`, the proxy is a verbatim relay and the stdio caller cannot tell which mode is in effect
  except by behavior: it exposes whatever store set the daemon had at its own startup.
- **HTTP** (`/mcp`, mounted on the daemon alongside its own `/v1` routes): a startup-time snapshot
  of stores, not rebuilt per session — a store added later via `/v1/stores` is invisible over MCP
  until the daemon restarts (see `mcp::http::build_streamable_http_service`'s doc comment). HTTP MCP
  sessions always run with `allow_write = false`.

Tool registration (the five read-only tools) and business logic are identical on both transports and
in both stdio modes — only the code path serving the request differs.

### 4.2.1 `--store` scoping over stdio

`localdb mcp -s <name>` (repeatable) narrows the store set the MCP session can reach, in **both**
stdio modes. Omitted, it means all stores (§2.2); an unknown name is `store_not_found`, exit 3,
before the server ever starts serving. A database with no stores at all is _not_ an error here — the
server starts and exposes zero stores (§2.2's empty-scope exception).

- **Embedded mode** resolves the scope against the local database and builds the handler over
  exactly those stores. Nothing else is reachable, because nothing else is open.
- **Proxied mode** enforces the scope per request, at the _tool-argument_ level, because that is the
  only channel that exists. rmcp's `StreamableHttpService` takes a synchronous
  `Fn() -> Result<S, io::Error>` service factory with no access to the HTTP request, so neither
  `/mcp?store=x` nor a custom header can select a scoped handler on the daemon side. The tool
  arguments already carry store scope, though — `search.stores`, `get_document.store`,
  `get_chunks.store`, `list_documents.store` — so `ProxyHandler` validates and injects them on each
  relayed `tools/call`, differently per tool depending on whether its `store` is optional or
  required: an explicit store argument outside the scope is always a tool-level `invalid_request`
  naming the allowed set, regardless of tool. For `get_document`/`get_chunks`, whose `store` is
  optional, an absent one is tried against each scoped store in turn, keeping the first non-error
  result, which preserves each tool's documented "omitted store scans every available store, first
  match wins" behavior narrowed to the scope. `list_documents.store` is required, not optional, so
  the proxy never injects it: an absent (or wrong-typed) `store` is relayed unmodified, and the
  upstream's own missing-required-argument error surfaces exactly as it would in embedded mode or an
  unscoped proxy. `list_stores`' response is filtered so an agent cannot even enumerate stores it
  may not read. While the tool set is fixed at five read-only tools, a scoped proxy relays _only_
  those five and rejects any other name with `invalid_request` — a future mutating tool must be
  given an explicit scoping rule before it can pass through.

**This is scoping, not a security boundary.** The daemon's `/mcp` is loopback and unauthenticated,
so anything that can open a socket can bypass `localdb mcp` and talk to the unscoped endpoint
directly. It stops an agent from _accidentally_ reading another project's docs; it does not contain
a hostile one. Real containment needs daemon-side auth, which is out of scope for v1.

### 4.3 Error model

MCP failures split into exactly two tiers, by whether the request could be _routed_ to a tool at
all:

- **Protocol-level** (a JSON-RPC error): the tool name itself is unregistered. `rmcp`'s
  macro-generated dispatch returns `ErrorCode::INVALID_PARAMS` ("tool not found") for any name not
  in the tool router. This is the one case a caller cannot recover from within the tool result.
- **Tool-level** (`CallToolResult { isError: true, .. }`): everything else — including cases one
  might expect to be protocol-level. A missing or wrong-typed _required_ argument (e.g. `search`'s
  `query`, `get_chunks`'s `resource_id`) fails `rmcp`'s `Parameters<T>` deserialization, which
  itself produces a protocol-level `ErrorData::invalid_params` — but `rmcp` 1.8.0's tool router
  downgrades that specific case to a tool-level result via `into_tool_argument_error`, so the
  caller's MCP client can render it like any other tool result. This is a real behavior difference
  from what an initial reading of the `rmcp` API might suggest; it was verified empirically
  (`mcp/tests/mcp_protocol.rs`), not assumed. Our own semantic validation (empty strings,
  out-of-range `limit`/`offset`, unknown store names, not-found lookups) is always tool-level,
  carrying a `{"error": {"code", "message"}}` JSON body as its text content.

Proxied stdio mode forwards whichever tier the daemon's own `/mcp` route returns unchanged — the
proxy never re-tiers an error it received an answer for. A failure of the proxy hop itself (the
daemon unreachable, the connection dropped mid-request) is a distinct case: there is no upstream
answer to relay a tier from, so it surfaces as a fresh protocol-level error instead. A scope
rejection (§4.2.1) is a third case, and the only one the proxy authors itself: the request never
reaches the upstream, and it is tool-level `invalid_request`, matching the tier the upstream's own
store validation would have used.

## 5. Shared error taxonomy

One enum in `core`; every surface maps it mechanically (HTTP status / CLI exit code + stderr / MCP
tool error). Codes are stable API:

| Code                                                                                                | Meaning                                                                                                                                                                                                                                                                                                         | HTTP                                     |
| --------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------- |
| `store_not_found` / `source_not_found` / `resource_not_found` / `job_not_found` / `chunk_not_found` | Unknown entity                                                                                                                                                                                                                                                                                                  | 404                                      |
| `runtime_state_locked`                                                                              | Unified database locked by another process (busy timeout exceeded)                                                                                                                                                                                                                                              | 409                                      |
| `daemon_running` / `daemon_unreachable`                                                             | Process-model conflicts                                                                                                                                                                                                                                                                                         | 409 / 502                                |
| `invalid_config`                                                                                    | Config failed validation (path-precise message)                                                                                                                                                                                                                                                                 | 422                                      |
| `invalid_request`                                                                                   | Bad arguments/body                                                                                                                                                                                                                                                                                              | 400                                      |
| `unsupported_format`                                                                                | Extraction can't handle the file type (informational in job stats)                                                                                                                                                                                                                                              | 422                                      |
| `extraction_failed`                                                                                 | Recognized, supported format whose contents could not be extracted (corrupt/truncated). Counted in `error_count` in job stats; produces a WARN per file.                                                                                                                                                        | 422                                      |
| `provider_unavailable`                                                                              | External embedding endpoint down/misconfigured                                                                                                                                                                                                                                                                  | 502                                      |
| `model_missing`                                                                                     | Local model not yet downloaded; message includes the fix                                                                                                                                                                                                                                                        | 503                                      |
| `rate_limited`                                                                                      | Retries against an upstream host exhausted (429/5xx/timeout); not _our_ rate limit — an upstream one — but grouped with the other "upstream not currently servable" codes and 502 for that reason                                                                                                               | 502                                      |
| `index_in_progress`                                                                                 | Conflicting job already running for the scope                                                                                                                                                                                                                                                                   | 409                                      |
| `job_already_terminal`                                                                              | `DELETE /v1/jobs/{id}` requested for a job already `done`/`failed`; cancellation never overwrites a recorded outcome (issue #218)                                                                                                                                                                               | 409                                      |
| `job_cancelled`                                                                                     | A job's `Failed` state whose cause was cancellation via `DELETE /v1/jobs/{id}` (issue #218); not a request's own error — it appears as the job's `error_code`, and a daemon-attached CLI (e.g. `localdb index`) reconstructs it via `Error::from_code` to exit 4 the same way a direct `job cancel` caller does | n/a (job outcome, never a live response) |
| `internal`                                                                                          | Bug; includes correlation id, logged with backtrace                                                                                                                                                                                                                                                             | 500                                      |

CLI exit codes: `0` ok, `1` internal, `2` invalid usage/config, `3` not found, `4` conflict/locked,
`5` unavailable (daemon/provider/model/rate-limited upstream).

`chunk_not_found` is MCP-local, not part of the core taxonomy above: it is authored directly by the
`get_chunks` tool's anchor-resolution paths (§4.1) rather than a `core::Error` variant, so it never
appears on an HTTP response body or a job's `error_code`, and has no CLI exit code of its own.

`core::Error::from_code(code, message)` is the one mapping back from a `{code, message}` pair to a
typed `Error` (the inverse of `code()` above), shared by every surface that receives an error this
way rather than as the enum itself: a daemon HTTP error body's `code` field
(`cli::daemon_client::decode_daemon_error`) and a failed `IndexJob`'s `error_code`/`error` fields
(`cli::job_attach::finish_job`, §3's `POST /v1/jobs` entry — issue #187 review, finding 3). Both
call sites fall back to `internal` for a code `from_code` doesn't recognize (an unknown/newer code,
or one of the three variants — `internal`, `unsupported_format`, `extraction_failed` — whose fields
don't fit a single `message` string).

Producers pair `message` with `from_code`'s expectations: for the 9 codes `from_code` rebuilds from
a single field (the four `*_not_found` codes, plus `invalid_config` / `invalid_request` /
`provider_unavailable` / `model_missing` / `rate_limited`), `message` is that bare field with no
`Display` prefix — a daemon HTTP error body (`server::error::ApiError`) and a failed `IndexJob`'s
`error` field both store `Error::raw_message()`, not `Error::to_string()`, so `from_code` can re-add
the prefix once on reconstruction instead of doubling it. Every other code's `message` is the full
`Display` string, since `from_code` decodes those either to a fixed no-message variant or not at
all.

### `localdb index --strict`

By default `index` is **best-effort**: unsupported files are silently counted; extraction failures
produce a per-file WARN but the run continues and exits `0`. Pass `--strict` to exit `2` when any
resource failed (`error_count > 0`). The run always completes — `--strict` never aborts mid-run; it
only affects the final exit code and JSON `"status"` field.

### `localdb index --delete`

`index` **never removes anything** unless `--delete` is passed, following `rsync --delete`. A
document whose file was deleted, or whose URL now returns 404/410, stays indexed and searchable; the
run reports the count as `docs_prunable` (text:
`N no longer at source (kept; use --delete to remove)`) so nothing goes stale silently. With
`--delete`, those documents are removed and counted in `docs_deleted`.

`--delete` is a request, not an override: the enumeration guards in
[04-search-pipeline.md](04-search-pipeline.md) §1 still suppress the sweep for a source whose
contents could not be observed, and warn when they do. Documents a guard is protecting are _not_
counted as prunable — `--delete` would not remove them either.

Both counters appear in `--json` output as `docs_deleted` and `docs_prunable`.

`--delete` works identically against a running daemon (maintainer decision D6, issue #187):
`POST /v1/jobs` carries a `deletion_policy` field (`"retain"` default, `"delete"` for `--delete`;
§3), so the CLI sends the real policy and the daemon's job engine honors it exactly as the embedded
path does. Stopping the daemon before indexing is no longer required for `--delete` or for `index`
in general.
