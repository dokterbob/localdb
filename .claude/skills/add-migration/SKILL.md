---
name: add-migration
description:
  Step-by-step checklist for adding a schema migration to store-libsql's migration chain (chain.rs +
  create_schema write-twice rule, weight-class choice, drift guard). Use when a change to localdb's
  unified database schema is needed.
---

## When to use

Use this whenever a change touches `store-libsql`'s on-disk schema: a new/altered table or column,
an index change, a DiskANN/FTS5 rebuild, or a change that invalidates existing chunks/embeddings.
Full design background: [docs/migrations.md](../../../docs/migrations.md),
[specs/02-domain-model.md](../../../specs/02-domain-model.md) §9,
[specs/05-surfaces.md](../../../specs/05-surfaces.md) §2.1.

**Never touch `store-libsql/src/migrations/baseline.rs`.** It is a frozen, byte-for-byte copy of the
v4 DDL, used only as an "old database" fixture source for tests. New schema changes are chain
entries — never edits to that file.

## Checklist

1. **Pick the next version.** `store_libsql::head_version_current()` (re-exported; internally
   `chain::head_version(&chain::migrations())`) is the current head. Your migration's `version` is
   `head_version_current() + 1`. The chain must stay contiguous from `chain::BASELINE_VERSION + 1`
   (`= 5`) — `chain::validate_chain` enforces this in CI.

   **Racing another branch:** if another in-flight branch also adds a migration against the same
   head, whoever's PR lands second renumbers their entry (and any tests/fixtures hardcoding that
   version) to stay contiguous with whatever landed first. Forgetting this is not a silent failure —
   `validate_chain` (and the drift-guard test below, which calls it transitively) fails CI with a
   message naming the offending entry and its expected version.

2. **Decide the weight class** (see `docs/migrations.md` "The three weight classes" for the full
   guidance):
   - **Class 1 — fast DDL.** Ordinary `CREATE TABLE`/`ALTER TABLE`/`CREATE INDEX`. Default choice.
   - **Class 2 — in-DB rebuild.** FTS5 rebuild, DiskANN (`chunks_vec_idx`) drop+recreate — a
     single-statement step that may take minutes; acceptable because `db migrate` is explicit and
     reports per-step progress. If your migration touches `chunks_vec_idx`, start its up-SQL with
     `DROP INDEX IF EXISTS chunks_vec_idx` before recreating it (rollback-safety belt-and-braces —
     see the comment in `runner.rs`'s `apply_pending`).
   - **Class 3 — re-embedding/re-extraction.** The migration can't do this work itself (the
     embedder/extractors live above `store-libsql`, and a step must never call up into them).
     Instead: bump `policy_version`/`extractor_version`, truncate now-invalid derived rows, and set
     `needs_reindex: true` so `db migrate` prints the `localdb index` hint. The actual
     re-embedding/re-extraction happens via the existing staleness machinery on the next
     `localdb index`.

3. **Write the `Migration` entry** in `store-libsql/src/migrations/chain.rs`'s `migrations()`:
   - `version`: from step 1.
   - `name`: stable snake_case identifier (used in `schema_migrations.name`, error messages, and
     `db status` history).
   - `summary`: free-text human description.
   - `up`: `Up::Sql(fn(&MigrationContext) -> Vec<String>)` for plain DDL/DML (the default), or
     `Up::Rust(Box<dyn RustStep>)` for changes SQL can't express. If `Up::Rust`, follow the
     authoring rules in `store-libsql/src/migrations/mod.rs`'s `RustStep` doc comment exactly: no
     own transaction, DB-effects only (no filesystem/network side effects — only DB writes roll back
     on failure), never call the ingestion/reindex pipeline, and provide a `checksum_repr()` that
     you bump whenever the step's behavior changes.
   - `down`: `Down::Sql(fn(&MigrationContext) -> Vec<String>)` — pure SQL, rendered once at apply
     time and stored as data so an _older_ binary can replay it without knowing this migration — or
     `Down::Unsupported("human-readable reason")` if the change is irreversible (e.g. it drops
     data). The reason string is shown verbatim in the `db downgrade` refusal message.
   - `needs_reindex`: `true` only for class 3 (see step 2).

4. **Fold the identical change into `schema::create_schema`** (in `store-libsql/src/schema.rs`) so a
   fresh-created database's DDL matches baseline + chain exactly. This is the write-twice rule — do
   not skip it.

5. **Add an up-then-down test for your migration**, and rely on the drift guard for the rest:
   - Follow the pattern in `runner.rs`'s `assert_up_then_down_restores_schema`: apply your
     migration, replay its stored `down_sql`, and assert the resulting `sqlite_master` matches what
     it was before.
   - The drift-guard test, `drift_guard_create_schema_equals_baseline_plus_chain`
     (`store-libsql/src/migrations/runner.rs`), automatically re-runs against your entry once it's
     in `chain::migrations()` — it asserts `schema::create_schema`'s output equals
     `baseline::create_baseline_schema` + the full compiled chain applied on top. You don't add
     anything for this yourself; just don't let step 4 drift from step 3.

6. **Run the store-libsql test suite:**

   ```sh
   cargo test -p store-libsql
   ```

   Also run `cargo test --workspace` before opening a PR — other crates' tests (e.g. anything
   building fixture databases via `baseline::create_baseline_schema`) can be affected by chain
   changes.

7. **Update specs if the change is user-visible.** `specs/02-domain-model.md` §9 (schema versioning
   design) and `specs/05-surfaces.md` §2.1 (CLI-visible migration behavior) are the design authority
   — update them if your migration changes what a user sees (new refusal wording, new weight-class
   example, etc.), per this repo's "fix the spec first if it's wrong" rule.

## Quick reference

| Question                                 | Answer                                                                              |
| ---------------------------------------- | ----------------------------------------------------------------------------------- |
| Where does the version come from?        | `head_version_current() + 1`; contiguous from `BASELINE_VERSION + 1`                |
| Where do I add the entry?                | `store-libsql/src/migrations/chain.rs`, `migrations()`                              |
| What else must change?                   | `schema::create_schema` (write-twice rule)                                          |
| Can I edit `baseline.rs`?                | No — never                                                                          |
| Touching `chunks_vec_idx`?               | Start up-SQL with `DROP INDEX IF EXISTS chunks_vec_idx`                             |
| Migration invalidates chunks/embeddings? | `needs_reindex: true`; don't call the embedder/extractors from the migration itself |
| Irreversible migration?                  | `Down::Unsupported("reason")`, not `Down::Sql`                                      |
