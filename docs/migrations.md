# localdb — Schema Migrations

> Covers the schema-migrations framework landed for issue
> [#127](https://github.com/dokterbob/localdb/issues/127). Design rationale lives in
> [specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
> §9 and [specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md)
> §2.1 — this document is the behavior/how-to layer on top of those, in the same spirit as
> [docs/architecture.md](architecture.md).

`store-libsql` tracks its own schema version in a `schema_migrations` table (source of truth) plus
`PRAGMA user_version` (a cheap, kept-in-lockstep marker — never authoritative on its own). Opening a
store **never** changes its schema version, in either direction, on any surface. A version mismatch
is always a refusal with an actionable hint, not an automatic fix.

---

## What you'll see on a version mismatch

Every surface — CLI, HTTP daemon, MCP — hits the same `LibsqlDb::open` refusal path
(`store-libsql/src/connection.rs`) and returns `invalid_config` (exit 2). There are three cases:

| Case        | When                                                                  | Hint                                                                                                                                                                                                      |
| ----------- | --------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Legacy**  | `0 < version < 4` — predates the migration framework entirely (v1–v3) | `database schema version {v} predates the migration baseline (v4); run 'localdb db migrate' to erase and rebuild it (all indexed data is lost, then re-run 'localdb index'), or delete the database file` |
| **Pending** | `4 <= version < head` — behind this binary's compiled migration chain | `database schema version {v} is behind this build (v{head}); run 'localdb db migrate' to apply pending migrations`                                                                                        |
| **Too new** | `version > head` — store was migrated by a newer binary               | `database schema version {v} is newer than this build (v{head}); run 'localdb db downgrade' with this binary to step it back, or upgrade localdb`                                                         |

A fresh (`version == 0`) or already-at-head store opens normally — no CLI action needed.

---

## Adding a source kind is a floor-version event too

Schema migrations aren't the only thing that can make a store unreadable by an older binary.
`sources.ingestor_kind` is decoded via a hard match over `IngestorKind`'s known variants
([specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
§2) — an unrecognized kind is a hard error for the whole `list_sources`/`index` call, not just the
one source carrying it.

The Atom/RSS feed connector (`kind = 'feed'`,
[#116](https://github.com/dokterbob/localdb/issues/116)) needed **no schema migration at all** —
`sources.config_json` already existed at baseline (v4) and simply gained a new key shape
(`{"max_entries", "fetch_full_content"}`), so `localdb db status` / `db migrate` report nothing
unusual for a feed-bearing store; its schema version is unaffected. But the moment a store has one
`kind = 'feed'` source, any older binary that predates the Feed ingestor can no longer open that
store for `source list` or `index` at all — even for its non-feed sources. There is no
`db downgrade` for this: the incompatibility lives in a data row, not the schema version, so the
migration framework's version-gated refusal never sees it coming. See
[docs/architecture.md#known-gaps](architecture.md#known-gaps) (item 12) for the tracked follow-up —
graceful degradation so an old binary skips unknown source kinds instead of failing the whole store.

---

## `db status` / `db migrate` / `db downgrade`

Four CLI-only maintenance subcommands (`localdb db status` / `migrate` / `downgrade` / `vacuum`),
specced in
[specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md) §2.1.
They are the _only_ surfaces allowed to touch a store's schema version — the HTTP daemon and MCP
never migrate, they only ever surface the refusal-with-hint above.

### `localdb db status`

Read-only, never refuses — reports state even for a too-new or legacy store:

```
$ localdb db status
schema version: 4 (this binary's head: 6, baseline: 4)
2 pending migrations; run `localdb db migrate`
history:
  v4 baseline  applied 2026-01-01T00:00:00Z  (not downgradable: baseline schema predates the migration framework; cannot downgrade below v4)
```

`--json` emits `current_version`, `head_version`, `baseline_version`, `pending`, `legacy`,
`too_new`, `table_present`, and the full `migrations` history array (each row: `version`, `name`,
`applied_at`, `downgradable`, `down_unsupported_reason`).

### `localdb db migrate`

Before applying anything, `db migrate` re-verifies the store's _existing_ `schema_migrations` rows
(completeness, checksum, and payload — the same checks `LibsqlDb::open` runs, bounded to the
already-applied prefix) and refuses, untouched, if that history is drifted or incomplete — a store
with corrupted bookkeeping never gets new migrations layered on top of it. Only once that passes
does it apply every pending migration in order, one transaction per step, with per-step progress on
stderr (`applied migration v5 'create_auth_tables' in 12ms`). If already at head:

```
$ localdb db migrate
already at head (v6)
```

If the store is a **legacy** (pre-baseline, v1–v3) store, migrating it means an unconditional
rebuild — all indexed data is lost — so the CLI prompts before doing anything:

```
This store's schema (v2) predates the migration baseline (v4); migrating it erases ALL indexed
data and rebuilds from scratch. Continue? [y/N]
```

Answering anything but `y`/`yes` aborts (prints `Aborted.` to stderr, exit 0, store untouched).
`--yes` skips the prompt; a non-interactive session without `--yes` exits 2 rather than hanging on
stdin. An ordinary forward migration (baseline or later) needs **no** confirmation — it's additive
by construction (see [Down::Sql rules](#down-rules) below for why the reverse direction can still be
destructive).

If any applied migration is `needs_reindex: true` (weight class 3, below), `db migrate` prints:

```
hint: run `localdb index` to re-index stale content
```

### `localdb db downgrade [--to N]`

Reverses migrations using **stored** down-SQL (never the compiled chain — see
[Old-binary downgrade story](#old-binary-downgrade-story)). Defaults to one step back if `--to` is
omitted (the CLI resolves `current_version - 1` itself; the library's own `downgrade_store` default,
used when `--to` is omitted at the call-site rather than the CLI, is the frozen baseline instead — a
reasonable default for a program calling the library directly, but not what the CLI exposes). Always
requires confirmation:

```
$ localdb db downgrade --to 5
This reverses the store's schema to version 5, replaying stored down-SQL and discarding any data
or structure introduced by later migrations. Continue? [y/N]
```

```
$ localdb db downgrade --to 5
downgraded: v6 -> v5 (1 step)
```

If a migration on the path to `--to` has `down_unsupported_reason` set, the whole downgrade is
refused up front — before touching anything — naming the blocking migration and suggesting the
nearest reachable target. For example, on the current real chain (baseline v4, irreversible v5, head
v6 — see the [CLI reference](cli.md#localdb-db-status) for the full listing), trying to downgrade
past v5 (`drop_chunks_block_id_and_retag_resource_metadata`, which is `Down::Unsupported`) to v4:

```
$ localdb db downgrade --to 4
error: invalid config: cannot downgrade past migration
'drop_chunks_block_id_and_retag_resource_metadata' (version 5): chunks.block_id cannot be
reconstructed; re-index required after downgrade. Nothing was changed. Downgrade to version 5
instead (`db downgrade --to 5`) to keep it applied and only replay the migrations above it.
```

### Daemon must be stopped

All four subcommands refuse with `daemon_running` (exit 4) while `localdb serve` is up — the same
way every other daemon-aware write command does. The daemon itself never applies migrations, so
there's nothing to route to it; the refusal exists purely to prevent a concurrent-write race against
a running daemon's connection.

<a id="old-binary-downgrade-story"></a>

### Old-binary downgrade story

Every migration's "down" SQL is **rendered once, at apply time**, and persisted as data in the
`schema_migrations.down_sql` column (a JSON array of statements — not a `;`-joined string, since
statements like trigger bodies can contain embedded semicolons). This is deliberate: it lets an
**older binary**, one that has never heard of a given migration, still step a store back past it.
`downgrade_store` takes no compiled chain at all — only a `&Path` — because accepting one would make
this impossible (an old binary's chain simply has no entry for a migration it doesn't know about).

A freshly created store is seeded with a `schema_migrations` row — including down-SQL — for _every_
chain entry, even though none of their up-SQL actually ran (the fresh schema is already at head). So
a brand-new store built by the latest binary is downgradable by an older one too.

Migrations that are irreversible, or expressed as a `RustStep` (see below), record
`down_unsupported_reason` instead of `down_sql`. `db downgrade` refuses cleanly the moment it would
have to replay past such a row — it pre-scans the whole planned path before running anything —
naming the migration, the stored reason, and the nearest `--to` target that keeps that step applied.

`db downgrade` also pre-scans for a **contiguous** history: it requires exactly one
`schema_migrations` row per version in `(target, current]` before replaying anything. A missing row
(a corrupted or partially-written store) is refused up front — naming the missing version(s) and
touching nothing — rather than silently skipping that migration's down-SQL or, in the worst case,
finding zero rows and reporting a no-op "success" while `PRAGMA user_version` stays put.

### Behavior change vs. pre-0.x

Older `store-libsql` builds — before this framework — **silently erased and reinitialized** a
version-mismatched store on open: "localdb will detect the version mismatch on open, print a
warning, and silently wipe and reinitialise every table" was the old default (see the
`refactor/ 117-parser-ingestor-wiring` branch's history for the exact prior wording, since
superseded). That is gone. Every surface now refuses on open; the only way to change a store's
schema version is the explicit `db migrate` / `db downgrade` commands above, and the one remaining
destructive path (the legacy v1–v3 rebuild) requires confirmation rather than running silently.

---

## Authoring guide

Read `store-libsql/src/migrations/mod.rs` for the full type vocabulary (`Migration`, `Up`, `Down`,
`RustStep`, `MigrationContext`) before writing a migration — this section is a guide to using it,
not a substitute for the doc comments.

### The write-twice rule

Every schema change is written **twice**: once as a chain entry
(`store-libsql/src/migrations/chain.rs`'s `migrations()`), and once folded directly into
`schema::create_schema` (the "current schema" helper every fresh-create path calls). These are not
allowed to diverge — a CI test, `drift_guard_create_schema_equals_baseline_plus_chain`
(`store-libsql/src/migrations/runner.rs`), builds one database via `schema::create_schema` and
another via `baseline::create_baseline_schema`

- `apply_pending(&chain)`, and asserts their normalized `sqlite_master` contents are byte-identical.
  Add a chain entry without updating `create_schema` (or vice versa) and this test fails — that's
  the point. It supersedes an earlier, narrower drift check
  (`baseline::baseline_schema_matches_current_create_schema_verbatim`) that only worked while the
  chain was empty.

### Picking the next version

`chain::BASELINE_VERSION` is frozen at `4`. The chain must be contiguous, starting at
`BASELINE_VERSION + 1` — `chain::validate_chain` checks this (`Error::Internal`, correlation id
`libsql_migrations_invalid_chain`, naming the offending entry and its expected version).
`chain::head_version_current()` tells you the current head; your new migration's `version` is
`head_version_current() + 1`.

**Renumbering when racing another branch:** two branches can add a migration concurrently against
the same head. Whoever's PR lands second is responsible for renumbering their entry (and any
tests/fixtures that hardcode its version) to stay contiguous with whatever landed first — bump the
version, keep everything else. If you forget, `validate_chain` (exercised by
`real_migrations_registry_passes_validation` and, transitively, by the drift-guard test) fails in CI
with a message naming exactly which entry is out of place and what version it should be. A botched
renumber can't silently merge.

### `Up::Sql` vs `Up::Rust`

- **`Up::Sql(fn(&MigrationContext) -> Vec<String>)`** — the default choice. Plain DDL/DML, rendered
  from context (e.g. baking in the embedding dimension) and executed statement-by-statement (not
  `execute_batch`, since rendered strings may contain trigger/FTS5 bodies with embedded semicolons
  that naive batch-splitting would mangle).
- **`Up::Rust(Box<dyn RustStep>)`** — for changes plain SQL can't express (row-by-row data
  transforms). Read the `RustStep` authoring rules in `mod.rs` verbatim before writing one; in
  summary:
  - Runs inside **one transaction the runner owns** — never call `BEGIN`/`COMMIT`/`ROLLBACK`
    yourself.
  - **DB-effects only.** No filesystem or network side effects — only a DB transaction rolls back on
    failure, so anything else a step does survives a failed migration.
  - **Never calls the ingestion/reindex pipeline.** The embedder and extractors live above
    `store-libsql` (`specs/01-architecture.md` §1: no domain logic in surface crates, and no
    reaching _up_ out of this crate either). If a change makes existing chunks/embeddings stale,
    mark it (`needs_reindex`, `policy_version`/`extractor_version` bump, truncate the now-invalid
    rows) and let a later `localdb index` do the actual work.
  - Provide a stable `checksum_repr()` string and **bump it whenever the step's behavior changes** —
    Rust code has no canonical rendering, so this string stands in for it in the drift-detection
    checksum (`checksum::migration_checksum`). Forgetting to bump it after editing a shipped step's
    logic is exactly the kind of drift the checksum exists to catch — don't rely on catching it by
    inspection.

<a id="down-rules"></a>

### `Down::Sql` rules

`Down::Sql(fn(&MigrationContext) -> Vec<String>)` is rendered **once, at apply time**, and stored as
data (JSON array) in `schema_migrations.down_sql` — never re-derived from the compiled chain at
downgrade time. That's what lets an older binary downgrade a store a newer one migrated (see
[Old-binary downgrade story](#old-binary-downgrade-story)). Consequences for how you write one:

- It must be **pure SQL** an arbitrary older binary can replay verbatim — no reference to Rust
  types, no calling back into this migration's own `Up::Rust` step.
- It must actually restore the prior schema. The runner's own tests
  (`up_then_down_restores_prior_schema_*`) apply a migration, replay its stored down-SQL, and assert
  the resulting `sqlite_master` matches what it was before — write your migration so this property
  holds; there's no separate mechanism that enforces it beyond your own up-then-down test (see
  below).

Use **`Down::Unsupported(reason)`** instead when the migration discards information a down-SQL
statement can't reconstruct (e.g. dropping a column) or is a `RustStep` with no clean inverse. The
`reason` is a human-readable `&'static str` — it's shown verbatim in the `db downgrade` refusal
message, so write it for the person hitting that refusal, not for yourself. Downgrade past an
`Unsupported` step is refused cleanly (pre-scanned before anything runs), naming the migration and
suggesting the nearest reachable `--to` target.

### `needs_reindex`

Set `true` when applying the migration invalidates already-derived data (chunks, embeddings) in a
way this migration doesn't itself repair. `db migrate` checks this flag across every step it
actually applied and, if any is `true`, prints the `localdb index` hint after finishing. This is
migration weight class 3 — see below.

### The three weight classes

From `specs/02-domain-model.md` §9 — pick based on what the change needs and what's acceptable to
run synchronously inside `db migrate`:

1. **Fast schema DDL.** Ordinary `CREATE TABLE`/`ALTER TABLE`/`CREATE INDEX` — an ordinary
   transactional runner step. The common case.
2. **In-DB rebuilds.** FTS5 rebuild, DiskANN index drop+recreate — single-statement runner steps
   that may take minutes. Acceptable because `db migrate` is explicit, not run silently on open, and
   reports per-step progress as it goes.

   The shipped example is **v6 `shrink_vector_index`** (issues #179, #177), and it's worth reading
   before writing another class-2 step because it illustrates the two things that make this class
   safe:
   - _It is class 2, not class 3._ `CREATE INDEX` on a vector index returns `CREATE_OK` rather than
     `CREATE_OK_SKIP_REFILL`, so SQLite runs its normal refill and re-inserts every row straight
     from `chunks.embedding`. No embedder is involved, so `needs_reindex` stays `false`. Don't
     assume "the index is derived data, therefore re-embedding" — check whether the source column is
     still there. (`store-libsql/tests/vector_index_cost.rs` asserts the refill actually happens; an
     index that silently rebuilt _empty_ would leave a store unsearchable after a "successful"
     migration, which no schema-shape test would catch.)
   - _Its rendered SQL depends on `MigrationContext`._ v6 emits different statements for `Binary`
     than for `Float32` (and none at all for the latter, whose tuning was already correct). That is
     supported — the runner renders per-context and the checksum is computed from the rendered
     result — but it has a consequence: opening a store with the _wrong_ encoding now produces a
     checksum mismatch as well as a column mismatch. `LibsqlDb::open` therefore validates the
     embedding column _before_ verifying checksums, so the user gets "embedding schema mismatch"
     rather than a misleading "migration drift" error. If you write another context-dependent
     migration, keep that ordering in mind.

   Class-2 steps free pages onto SQLite's free list without shrinking the file, so a migration that
   reclaims significant space should leave the user pointed at `localdb db vacuum` — `db migrate`
   does this automatically when `shrink_vector_index` applies to a binary store.

3. **Re-embedding / re-extraction.** Not runnable by the store itself — the embedder and extractors
   live above `store-libsql`, and a migration step must not reach up into them (see the `RustStep`
   rules above). Instead, the migration only _marks_ the work: bump
   `policy_version`/`extractor_version`, truncate now-invalid derived rows, set
   `needs_reindex: true`. The existing staleness machinery and incremental `localdb index` do the
   actual re-embedding/re-extraction, resumably and with progress.

If your migration doesn't touch chunks/embeddings at all, it's class 1. If it does but the store
itself can finish the work in one transaction (a schema-only index rebuild with no external model
dependency), it's class 2. If it needs the embedder or extractors, it's class 3 — don't try to force
it into class 1/2 by, say, calling out to an HTTP embedding API from inside a `RustStep`; that
violates the no-network-side-effects rule above and the no-reaching-up-into-surface-crates
architecture invariant.

### `baseline.rs` is frozen

`store-libsql/src/migrations/baseline.rs` is a byte-for-byte copy of the v4 DDL, frozen forever.
**Never edit it** — new schema changes are chain entries (plus, per the write-twice rule, folded
into `schema::create_schema`), never edits to `baseline.rs`. Its purpose is purely as a fixture
source: `baseline::create_baseline_schema` builds an "old database" for tests — call it, then apply
your migration chain on top, exactly like a real pre-migrations database would be upgraded in
production. `store-libsql/tests/real_migrations.rs` is a worked example of this pattern end to end.

### DiskANN caveat

A migration touching the `chunks_vec_idx` DiskANN vector index should start its up-SQL with
`DROP INDEX IF EXISTS chunks_vec_idx` before recreating it. Whether libsql/SQLite fully unwinds
partial ANN-index construction on transaction rollback is unverified (see the comment in
`runner.rs`'s `apply_pending`) — an explicit drop-first keeps a retried migration safe regardless of
that answer.
