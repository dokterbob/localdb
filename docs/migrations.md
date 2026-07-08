# localdb — Schema Migrations

> Covers the schema-migrations framework landed for issue [#127](https://github.com/dokterbob/localdb/issues/127).
> Design rationale lives in [specs/02-domain-model.md](../specs/02-domain-model.md) §9 and
> [specs/05-surfaces.md](../specs/05-surfaces.md) §2.1 — this document is the behavior/how-to layer
> on top of those, in the same spirit as [docs/architecture.md](architecture.md).

`store-libsql` tracks its own schema version in a `schema_migrations` table (source of truth) plus
`PRAGMA user_version` (a cheap, kept-in-lockstep marker — never authoritative on its own). Opening
a store **never** changes its schema version, in either direction, on any surface. A version
mismatch is always a refusal with an actionable hint, not an automatic fix.

---

## What you'll see on a version mismatch

Every surface — CLI, HTTP daemon, MCP — hits the same `LibsqlDb::open` refusal path
(`store-libsql/src/connection.rs`) and returns `invalid_config` (exit 2). There are three cases:

| Case | When | Hint |
|---|---|---|
| **Legacy** | `0 < version < 4` — predates the migration framework entirely (v1–v3) | `database schema version {v} predates the migration baseline (v4); run 'localdb db migrate' to erase and rebuild it (all indexed data is lost, then re-run 'localdb index'), or delete the database file` |
| **Pending** | `4 <= version < head` — behind this binary's compiled migration chain | `database schema version {v} is behind this build (v{head}); run 'localdb db migrate' to apply pending migrations` |
| **Too new** | `version > head` — store was migrated by a newer binary | `database schema version {v} is newer than this build (v{head}); run 'localdb db downgrade' with this binary to step it back, or upgrade localdb` |

A fresh (`version == 0`) or already-at-head store opens normally — no CLI action needed.

---

## `db status` / `db migrate` / `db downgrade`

Three CLI-only maintenance subcommands (`localdb db …`), specced in
[specs/05-surfaces.md](../specs/05-surfaces.md) §2.1. They are the *only* surfaces allowed to touch
a store's schema version — the HTTP daemon and MCP never migrate, they only ever surface the
refusal-with-hint above.

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

Before applying anything, `db migrate` re-verifies the store's *existing* `schema_migrations` rows
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
`--yes` skips the prompt; a non-interactive session without `--yes` exits 2 rather than hanging
on stdin. An ordinary forward migration (baseline or later) needs **no** confirmation — it's
additive by construction (see [Down::Sql rules](#down-rules) below for why the reverse direction
can still be destructive).

If any applied migration is `needs_reindex: true` (weight class 3, below), `db migrate` prints:

```
hint: run `localdb index` to re-index stale content
```

### `localdb db downgrade [--to N]`

Reverses migrations using **stored** down-SQL (never the compiled chain — see
[Old-binary downgrade story](#old-binary-downgrade-story)). Defaults to one step back if `--to` is
omitted (the CLI resolves `current_version - 1` itself; the library's own `downgrade_store`
default, used when `--to` is omitted at the call-site rather than the CLI, is the frozen baseline
instead — a reasonable default for a program calling the library directly, but not what the CLI
exposes). Always requires confirmation:

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
nearest reachable target:

```
cannot downgrade past migration 'drop_chunks_block_id_and_retag_resource_metadata' (version 7):
chunks.block_id cannot be reconstructed; re-index required after downgrade. Nothing was changed.
Downgrade to version 7 instead (`db downgrade --to 7`) to keep it applied and only replay the
migrations above it.
```

### Daemon must be stopped

All three subcommands refuse with `daemon_running` (exit 4) while `localdb serve` is up — the
same way every other daemon-aware write command does. The daemon itself never applies migrations,
so there's nothing to route to it; the refusal exists purely to prevent a concurrent-write race
against a running daemon's connection.

### Old-binary downgrade story {#old-binary-downgrade-story}

Every migration's "down" SQL is **rendered once, at apply time**, and persisted as data in the
`schema_migrations.down_sql` column (a JSON array of statements — not a `;`-joined string, since
statements like trigger bodies can contain embedded semicolons). This is deliberate: it lets an
**older binary**, one that has never heard of a given migration, still step a store back past it.
`downgrade_store` takes no compiled chain at all — only a `&Path` — because accepting one would
make this impossible (an old binary's chain simply has no entry for a migration it doesn't know
about).

A freshly created store is seeded with a `schema_migrations` row — including down-SQL — for
*every* chain entry, even though none of their up-SQL actually ran (the fresh schema is already at
head). So a brand-new store built by the latest binary is downgradable by an older one too.

Migrations that are irreversible, or expressed as a `RustStep` (see below), record
`down_unsupported_reason` instead of `down_sql`. `db downgrade` refuses cleanly the moment it
would have to replay past such a row — it pre-scans the whole planned path before running
anything — naming the migration, the stored reason, and the nearest `--to` target that keeps that
step applied.

`db downgrade` also pre-scans for a **contiguous** history: it requires exactly one
`schema_migrations` row per version in `(target, current]` before replaying anything. A missing row
(a corrupted or partially-written store) is refused up front — naming the missing version(s) and
touching nothing — rather than silently skipping that migration's down-SQL or, in the worst case,
finding zero rows and reporting a no-op "success" while `PRAGMA user_version` stays put.

### Behavior change vs. pre-0.x

Older `store-libsql` builds — before this framework — **silently erased and reinitialized** a
version-mismatched store on open (see `docs/migrations/v4-to-v5.sql` in the
`refactor/117-parser-ingestor-wiring` branch for the exact wording of that old default: "localdb
will detect the version mismatch on open, print a warning, and silently wipe and reinitialise
every table"). That is gone. Every surface now refuses on open; the only way to change a store's
schema version is the explicit `db migrate` / `db downgrade` commands above, and the one
remaining destructive path (the legacy v1–v3 rebuild) requires confirmation rather than running
silently.

---

## Authoring guide

Read `store-libsql/src/migrations/mod.rs` for the full type vocabulary
(`Migration`, `Up`, `Down`, `RustStep`, `MigrationContext`) before writing a migration — this
section is a guide to using it, not a substitute for the doc comments.

### The write-twice rule

Every schema change is written **twice**: once as a chain entry
(`store-libsql/src/migrations/chain.rs`'s `migrations()`), and once folded directly into
`schema::create_schema` (the "current schema" helper every fresh-create path calls). These are
not allowed to diverge — a CI test,
`drift_guard_create_schema_equals_baseline_plus_chain` (`store-libsql/src/migrations/runner.rs`),
builds one database via `schema::create_schema` and another via `baseline::create_baseline_schema`
+ `apply_pending(&chain)`, and asserts their normalized `sqlite_master` contents are byte-identical.
Add a chain entry without updating `create_schema` (or vice versa) and this test fails — that's
the point. It supersedes an earlier, narrower drift check
(`baseline::baseline_schema_matches_current_create_schema_verbatim`) that only worked while the
chain was empty.

### Picking the next version

`chain::BASELINE_VERSION` is frozen at `4`. The chain must be contiguous, starting at
`BASELINE_VERSION + 1` — `chain::validate_chain` checks this (`Error::Internal`,
correlation id `libsql_migrations_invalid_chain`, naming the offending entry and its expected
version). `chain::head_version_current()` tells you the current head; your new migration's
`version` is `head_version_current() + 1`.

**Renumbering when racing another branch:** two branches can add a migration concurrently against
the same head. Whoever's PR lands second is responsible for renumbering their entry (and any
tests/fixtures that hardcode its version) to stay contiguous with whatever landed first — bump the
version, keep everything else. If you forget, `validate_chain` (exercised by
`real_migrations_registry_passes_validation` and, transitively, by the drift-guard test) fails in
CI with a message naming exactly which entry is out of place and what version it should be. A
botched renumber can't silently merge.

### `Up::Sql` vs `Up::Rust`

- **`Up::Sql(fn(&MigrationContext) -> Vec<String>)`** — the default choice. Plain DDL/DML,
  rendered from context (e.g. baking in the embedding dimension) and executed
  statement-by-statement (not `execute_batch`, since rendered strings may contain trigger/FTS5
  bodies with embedded semicolons that naive batch-splitting would mangle).
- **`Up::Rust(Box<dyn RustStep>)`** — for changes plain SQL can't express (row-by-row data
  transforms). Read the `RustStep` authoring rules in `mod.rs` verbatim before writing one; in
  summary:
  - Runs inside **one transaction the runner owns** — never call `BEGIN`/`COMMIT`/`ROLLBACK`
    yourself.
  - **DB-effects only.** No filesystem or network side effects — only a DB transaction rolls back
    on failure, so anything else a step does survives a failed migration.
  - **Never calls the ingestion/reindex pipeline.** The embedder and extractors live above
    `store-libsql` (`specs/01-architecture.md` §1: no domain logic in surface crates, and no
    reaching *up* out of this crate either). If a change makes existing chunks/embeddings stale,
    mark it (`needs_reindex`, `policy_version`/`extractor_version` bump, truncate the now-invalid
    rows) and let a later `localdb index` do the actual work.
  - Provide a stable `checksum_repr()` string and **bump it whenever the step's behavior
    changes** — Rust code has no canonical rendering, so this string stands in for it in the
    drift-detection checksum (`checksum::migration_checksum`). Forgetting to bump it after
    editing a shipped step's logic is exactly the kind of drift the checksum exists to catch —
    don't rely on catching it by inspection.

### `Down::Sql` rules {#down-rules}

`Down::Sql(fn(&MigrationContext) -> Vec<String>)` is rendered **once, at apply time**, and stored
as data (JSON array) in `schema_migrations.down_sql` — never re-derived from the compiled chain at
downgrade time. That's what lets an older binary downgrade a store a newer one migrated (see
[Old-binary downgrade story](#old-binary-downgrade-story)). Consequences for how you write one:

- It must be **pure SQL** an arbitrary older binary can replay verbatim — no reference to Rust
  types, no calling back into this migration's own `Up::Rust` step.
- It must actually restore the prior schema. The runner's own tests
  (`up_then_down_restores_prior_schema_*`) apply a migration, replay its stored down-SQL, and
  assert the resulting `sqlite_master` matches what it was before — write your migration so this
  property holds; there's no separate mechanism that enforces it beyond your own up-then-down
  test (see below).

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

1. **Fast schema DDL.** Ordinary `CREATE TABLE`/`ALTER TABLE`/`CREATE INDEX` — an
   ordinary transactional runner step. The common case.
2. **In-DB rebuilds.** FTS5 rebuild, DiskANN index drop+recreate — single-statement runner steps
   that may take minutes. Acceptable because `db migrate` is explicit, not run silently on open,
   and reports per-step progress as it goes.
3. **Re-embedding / re-extraction.** Not runnable by the store itself — the embedder and
   extractors live above `store-libsql`, and a migration step must not reach up into them (see
   the `RustStep` rules above). Instead, the migration only *marks* the work: bump
   `policy_version`/`extractor_version`, truncate now-invalid derived rows, set
   `needs_reindex: true`. The existing staleness machinery and incremental `localdb index` do the
   actual re-embedding/re-extraction, resumably and with progress.

If your migration doesn't touch chunks/embeddings at all, it's class 1. If it does but the store
itself can finish the work in one transaction (a schema-only index rebuild with no external
model dependency), it's class 2. If it needs the embedder or extractors, it's class 3 — don't try
to force it into class 1/2 by, say, calling out to an HTTP embedding API from inside a `RustStep`;
that violates the no-network-side-effects rule above and the no-reaching-up-into-surface-crates
architecture invariant.

### `baseline.rs` is frozen

`store-libsql/src/migrations/baseline.rs` is a byte-for-byte copy of the v4 DDL, frozen forever.
**Never edit it** — new schema changes are chain entries (plus, per the write-twice rule, folded
into `schema::create_schema`), never edits to `baseline.rs`. Its purpose is purely as a fixture
source: `baseline::create_baseline_schema` builds an "old database" for tests — call it, then
apply your migration chain on top, exactly like a real pre-migrations database would be upgraded
in production. `store-libsql/tests/real_migrations.rs` is a worked example of this pattern end to
end (see the consumer recipes below).

### DiskANN caveat

A migration touching the `chunks_vec_idx` DiskANN vector index should start its up-SQL with
`DROP INDEX IF EXISTS chunks_vec_idx` before recreating it. Whether libsql/SQLite fully unwinds
partial ANN-index construction on transaction rollback is unverified (see the comment in
`runner.rs`'s `apply_pending`) — an explicit drop-first keeps a retried migration safe regardless
of that answer.

---

## Consumer recipe: `auth` branch

The `auth` branch (issue #98) currently hand-rolls its own migration runner in
`store-libsql/src/schema.rs`: a `Migration { from, to, run }` struct, a `MIGRATIONS: &[Migration]`
table, and a `run_migrations` function that walks it — plus an auto-migrate branch inside its own
`connection.rs`'s `LibsqlDb::open` (`if version != 0 && version < schema::SCHEMA_VERSION { …
schema::run_migrations(...).await?; }`). Adopting this framework means deleting all of that and
re-expressing the same two schema changes as chain entries.

### 1. Delete the ad-hoc runner

Remove from `store-libsql/src/schema.rs`: the `Migration` struct, `MigrationFn` type alias,
`MIGRATIONS` constant, and `run_migrations` function (roughly lines 465–580 as of the branch tip).
Remove the auto-migrate branch in `connection.rs`'s `LibsqlDb::open` (the `version <
schema::SCHEMA_VERSION` arm that calls `schema::run_migrations`) — `LibsqlDb::open` never migrates
under this framework; it only refuses.

### 2. `v5`: `create_auth_tables` as a chain entry

`auth`'s `create_auth_tables` (schema.rs, ~line 314) creates the 7 auth tables: `users`,
`auth_tokens`, `oauth_clients`, `auth_codes`, `store_grants`, `invites`, `access_requests`. As a
chain entry:

- `Up::Sql`: the v5-era DDL — the same statements, **without** `access_requests.collected_at`
  (that column arrives in v6 below; including it here would make the v6 `ALTER TABLE ADD COLUMN`
  fail with "duplicate column name" on a store that runs v5 and v6 back-to-back). See
  `store-libsql/tests/real_migrations.rs`'s `v5_create_auth_tables_up` for the exact statements —
  it mirrors this branch's DDL verbatim (minus the `oauth_clients` seed INSERT, which is DML, not
  schema, and irrelevant to the migration machinery).
- `Down::Sql`: `DROP TABLE` the 7 tables (children before parents is not strictly required here —
  every FK is `CASCADE`/`SET NULL` — but is good practice) plus any indexes SQLite doesn't drop
  automatically with their table (`DROP TABLE` does drop a table's own indexes, so no separate
  `DROP INDEX` statements are needed for `idx_auth_tokens_user`, `idx_auth_tokens_family`,
  `idx_store_grants_user`, `idx_access_requests_invite`).
- `needs_reindex: false` — purely additive, doesn't touch chunks/embeddings.

### 3. `v6`: `collected_at` as a chain entry

- `Up::Sql`: `ALTER TABLE access_requests ADD COLUMN collected_at TEXT`.
- `Down::Sql`: `ALTER TABLE access_requests DROP COLUMN collected_at`.
- The `pragma_table_info` guard in the current `add_access_requests_collected_at_column`
  (checking whether the column already exists before running the `ALTER TABLE`) becomes
  **unnecessary** as a chain entry: that guard exists in the current code because the same
  function runs from two different starting points (a store already at v5 pre-ticket, vs. a store
  jumping straight from v4 through a `create_auth_tables` that, at HEAD, already includes
  `collected_at`). A chain entry doesn't have that ambiguity — the chain guarantees this step runs
  exactly once, from the exact v5 predecessor fixed in step 2 above (which never has
  `collected_at`). Drop the guard.
- `needs_reindex: false`.

### 4. Replace the test fixture

`create_pre_auth_schema_v4_for_test` (in `schema.rs`'s `#[cfg(test)]` module) built a v4-only
database by calling the same per-table DDL helpers `create_schema` uses, minus the auth tables.
Replace every call site with `store_libsql::migrations::baseline::create_baseline_schema` — that's
now the canonical "old v4 database" fixture, shared across the whole workspace instead of
duplicated per-branch.

### 5. Update the tests

- `open` no longer auto-migrates: any test asserting that opening a v4 (or v5) store transparently
  upgrades it now asserts the opposite — a refusal (`Error::InvalidConfig`, message containing
  `db migrate` and `predates`/`behind`), with the store's `sqlite_master`/`user_version`/
  `schema_migrations` state provably unchanged before vs. after (see `connection.rs`'s
  `reopen_with_legacy_schema_version_is_refused_without_mutation` for the pattern). To actually
  advance a test fixture, call `store_libsql::migrate_store` directly instead of relying on
  `open`.
- Any test whose failure message asserted the old v1–v3 "no migration path" wording is now
  instead the `db migrate` hint (`"database schema version {v} predates the migration baseline
  (v4); run 'localdb db migrate' to erase and rebuild it..."`). This is a small, sanctioned
  amendment to D13 ("never destructive silently") rather than a reversal of it: the rebuild path
  still exists, is still the only way to bring a legacy store forward, and is still gated behind
  an explicit confirmation prompt (`cli/src/cmds/db.rs`'s `run_db_migrate_async`) — it just isn't
  triggered implicitly by `open` anymore.

`store-libsql/tests/real_migrations.rs` mirrors this exact v5/v6 pair (as fixture migrations, not
the real chain entries) end-to-end — forward application, checking the 7 tables and the
`collected_at` column land correctly, and backward (down to v5, down to baseline) restoring
exactly a fresh-baseline schema while preserving pre-seeded v4 data. Treat it as the executable
version of this recipe.

---

## Consumer recipe: PR #151

PR #151 / branch `refactor/117-parser-ingestor-wiring` ships a manual `docs/migrations/v4-to-v5.sql`
script (run by hand with `sqlite3 ... < docs/migrations/v4-to-v5.sql` *before* upgrading the
binary) instead of an in-binary migration. Adopting this framework means converting that script
into a chain entry and deleting it.

### 1. Renumber

The script targets `v4 -> v5`. Under this framework that version is likely already taken — by the
`auth` branch's `v5` (`create_auth_tables`), if it lands first. Renumber to the next free version:
**v7** if `auth` lands first (its v5 and v6 already claim those slots), **v5** if this PR lands
first instead. Whoever lands second is responsible for the renumber — see
[Renumbering when racing another branch](#picking-the-next-version) above.

### 2. Convert the script to a chain entry

The script's three DDL/DML steps become one migration's `Up::Sql`, verbatim minus the sqlite3
dot-command (`.bail on`) and the transaction wrapper (`BEGIN IMMEDIATE` / `COMMIT` /
`PRAGMA user_version = 5`) — the runner owns the transaction and the version stamp itself
(`runner.rs`'s `apply_one`):

```rust
fn up(_ctx: &MigrationContext) -> Vec<String> {
    vec![
        "ALTER TABLE chunks DROP COLUMN block_id".to_string(),
        "DROP INDEX IF EXISTS idx_chunks_store_resource".to_string(),
        "CREATE INDEX IF NOT EXISTS idx_chunks_store_resource_pos \
         ON chunks(store_id, resource_id, block_seq, seq_in_block)".to_string(),
        "UPDATE resources SET metadata_json = json_set( \
             metadata_json, '$.kind', 'document', \
             '$.page_count', NULL, '$.word_count', NULL) \
         WHERE json_valid(metadata_json) \
           AND json_extract(metadata_json, '$.kind') IS NULL".to_string(),
    ]
}
```

Delete `docs/migrations/v4-to-v5.sql` and its usage instructions once the chain entry lands — the
manual pre-upgrade step it documented no longer exists; `localdb db migrate` replaces it.

### 3. Down is unsupported

`chunks.block_id` is dropped and can't be reconstructed (the new block reference model is
`(store_id, resource_id, block_seq)`, not a `blocks.rowid` foreign key — there's no way to
recompute the old column's values from what remains). Use:

```rust
down: Down::Unsupported(
    "chunks.block_id cannot be reconstructed; re-index required after downgrade",
),
```

### 4. The textsplitter policy bump needs no migration code

The script's chunking-policy bump (`textsplitter-md-v3` -> `textsplitter-md-v4`) is **not** part of
this migration's SQL — it's an ordinary `policy_version` change the existing staleness machinery
already detects and handles on the next `localdb index` (class-3 weight, per the guide above, but
one the *indexing* code already implements, not something this migration needs to run). The
migration entry does, however, need `needs_reindex: true` set — that's what makes `db migrate`
print the `localdb index` hint after applying it, since the `chunks.block_id` drop and the
`metadata_json` retag both leave existing chunks in a state a re-index should refresh.

### 5. Whoever lands second renumbers

Same rule as the `auth` recipe: if PR #151 and `auth` land in the same window, whichever merges
second is responsible for shifting its migration's `version` to stay contiguous with the other's,
and re-running `store-libsql`'s test suite (the drift guard and `validate_chain` will catch a
botched renumber immediately).

`store-libsql/tests/real_migrations.rs`'s `v7` fixture (`drop_chunks_block_id_and_retag_resource_metadata`)
mirrors this exact migration end-to-end, including a forward-application check against seeded
v4-era data (metadata retagged correctly, `block_id` gone, index swapped, row counts preserved)
and a downgrade-refusal test proving the `Unsupported` reason and the suggested `--to` target are
exactly as described above. Treat it as the executable version of this recipe — the real chain
entry that eventually lands in `chain::migrations()` supersedes it.
