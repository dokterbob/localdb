//! The migration runner: walks a [`Migration`] chain forward from a
//! database's current `PRAGMA user_version` to the chain's head, one
//! transaction per step, and seeds `schema_migrations` bookkeeping for
//! freshly created databases that start life already at head.
//!
//! ## Foreign-key enforcement during migrations
//!
//! `PRAGMA foreign_keys` cannot be toggled inside a transaction — SQLite
//! silently ignores the attempt — so this runner does not try. Every step
//! runs with whatever FK enforcement the connection already has configured
//! (`ON` in production; see `connection.rs`). A migration that needs to
//! restructure a table in a way that would normally rely on toggling
//! `foreign_keys=OFF` (the classic SQLite "12-step" table rebuild) must
//! instead use a rebuild pattern that stays valid with FK enforcement ON
//! throughout, or wait for a future runner extension that manages a
//! dedicated FK-off connection/step.
//!
//! ## Applying steps
//!
//! Each pending migration is applied in its own `BEGIN IMMEDIATE` transaction:
//! the rendered "up" SQL (or `RustStep::apply`) runs, the rendered "down" SQL
//! (or unsupported reason) is persisted as a `schema_migrations` row, and
//! `PRAGMA user_version` is advanced — all before committing. On any failure
//! the transaction is rolled back **explicitly** rather than relying on
//! `Drop`, since drop-rollback semantics for local libsql connections were
//! unconfirmed at the time this runner was written (see the partial-failure
//! test below, which is the gate for that assumption).

use std::time::{Duration, Instant};

use libsql::{Connection, TransactionBehavior};
use localdb_core::Error;

use super::chain::{head_version, validate_chain, BASELINE_VERSION};
use super::checksum::migration_checksum;
use super::progress::{MigrationProgressEvent, MigrationProgressSink};
use super::table::{self, MigrationRow};
use super::{Down, Migration, MigrationContext, Up};
use crate::connection::map_libsql_err;
use crate::schema::get_schema_version;

/// One successfully-applied migration step, for caller-side reporting (the
/// CLI prints a line per step from this later).
#[derive(Debug, Clone)]
pub struct AppliedStep {
    pub version: i64,
    pub name: String,
    pub duration: Duration,
}

/// The result of a call to [`apply_pending`]: every step that was actually
/// run, in the order applied (ascending version). Empty if the database was
/// already at head.
#[derive(Debug, Clone, Default)]
pub struct AppliedReport {
    pub applied: Vec<AppliedStep>,
}

/// Apply every migration in `chain` with `version` greater than the
/// database's current `PRAGMA user_version`, in ascending order, one
/// transaction per step.
///
/// Preconditions checked here: `chain` must be contiguous (see
/// [`validate_chain`]), and the database's current version must be at least
/// [`BASELINE_VERSION`] — legacy (pre-migration-framework) upgrade handling
/// lives elsewhere and must run before this function is called.
pub async fn apply_pending(
    conn: &Connection,
    chain: &[Migration],
    ctx: &MigrationContext,
) -> Result<AppliedReport, Error> {
    apply_pending_with_progress(conn, chain, ctx, None).await
}

/// Same as [`apply_pending`], but emits [`MigrationProgressEvent`]s into
/// `progress` (if given) so a long-running caller can render a live
/// indicator. Purely observational — see `progress`'s module doc comment;
/// `progress: None` behaves identically to [`apply_pending`].
pub async fn apply_pending_with_progress(
    conn: &Connection,
    chain: &[Migration],
    ctx: &MigrationContext,
    progress: Option<&MigrationProgressSink>,
) -> Result<AppliedReport, Error> {
    validate_chain(chain)?;

    table::ensure_table(conn).await.map_err(map_libsql_err)?;
    table::ensure_baseline_row(conn)
        .await
        .map_err(map_libsql_err)?;

    let current = get_schema_version(conn).await.map_err(map_libsql_err)?;
    if current < BASELINE_VERSION {
        return Err(Error::Internal {
            message: format!(
                "migration runner invoked on a database at schema version {current}, which is \
                 below the frozen baseline version {BASELINE_VERSION}; a legacy upgrade must \
                 bring it to baseline before the migration chain can run"
            ),
            correlation_id: "libsql_migrations_below_baseline".to_string(),
        });
    }

    let pending: Vec<&Migration> = chain.iter().filter(|m| m.version > current).collect();
    let total = pending.len();
    if let Some(cb) = progress {
        cb(MigrationProgressEvent::Started {
            total_pending: total,
        });
    }

    let mut report = AppliedReport::default();

    for (i, migration) in pending.into_iter().enumerate() {
        if let Some(cb) = progress {
            cb(MigrationProgressEvent::ApplyingStep {
                index: i + 1,
                total,
                version: migration.version,
                name: migration.name.to_string(),
            });
        }
        let started = Instant::now();

        // Future migrations touching the DiskANN index `chunks_vec_idx`
        // should start their up-SQL with `DROP INDEX IF EXISTS
        // chunks_vec_idx` before recreating it: whether libsql/SQLite fully
        // unwinds partial ANN-index construction on transaction rollback is
        // unverified, so an explicit drop-first keeps a retried migration
        // safe regardless of that answer.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(map_libsql_err)?;

        if let Err(e) = apply_one(&tx, migration, ctx).await {
            if let Err(rollback_err) = tx.rollback().await {
                tracing::error!(
                    migration = migration.name,
                    version = migration.version,
                    error = %rollback_err,
                    "rollback after failed migration step also failed"
                );
            }
            return Err(Error::Internal {
                message: format!(
                    "migration '{name}' (version {version}) failed and was rolled back: {e}",
                    name = migration.name,
                    version = migration.version,
                ),
                correlation_id: "libsql_migrations_apply_failed".to_string(),
            });
        }

        tx.commit().await.map_err(map_libsql_err)?;

        let duration = started.elapsed();
        tracing::info!(
            version = migration.version,
            name = migration.name,
            duration_ms = duration.as_millis() as u64,
            "applied migration"
        );
        eprintln!(
            "applied migration v{version} '{name}' in {duration_ms}ms",
            version = migration.version,
            name = migration.name,
            duration_ms = duration.as_millis(),
        );

        report.applied.push(AppliedStep {
            version: migration.version,
            name: migration.name.to_string(),
            duration,
        });
    }

    Ok(report)
}

/// Apply one migration's up-step, persist its down-step as a
/// `schema_migrations` row, and advance `PRAGMA user_version` — all against
/// `tx`. Callers own the transaction's commit/rollback.
async fn apply_one(
    tx: &Connection,
    migration: &Migration,
    ctx: &MigrationContext,
) -> Result<(), libsql::Error> {
    match &migration.up {
        Up::Sql(render) => {
            // Executed statement-by-statement, NOT via execute_batch: the
            // rendered strings may contain trigger/FTS5 bodies with embedded
            // semicolons that naive batch-splitting would mangle.
            for stmt in render(ctx) {
                tx.execute(&stmt, ()).await?;
            }
        }
        Up::Rust(step) => {
            step.apply(tx, ctx).await?;
        }
    }

    let (down_sql, down_unsupported_reason) = render_down(migration, ctx);
    let row = MigrationRow {
        version: migration.version,
        name: migration.name.to_string(),
        applied_at: localdb_core::ingestion::now_rfc3339(),
        down_sql,
        down_unsupported_reason,
        checksum: migration_checksum(migration, ctx),
    };
    table::insert_row(tx, &row).await?;

    // PRAGMAs may return rows; use query() not execute() (see
    // baseline::set_user_version).
    tx.query(
        &format!(
            "PRAGMA user_version = {version}",
            version = migration.version
        ),
        (),
    )
    .await?;

    Ok(())
}

fn render_down(
    migration: &Migration,
    ctx: &MigrationContext,
) -> (Option<Vec<String>>, Option<String>) {
    match &migration.down {
        Down::Sql(render) => (Some(render(ctx)), None),
        Down::Unsupported(reason) => (None, Some(reason.to_string())),
    }
}

/// Seed `schema_migrations` for a database that was just created fresh at
/// head version (via `schema::create_schema`), rather than upgraded from an
/// older one — and stamp `PRAGMA user_version` to the chain's head version as
/// the LAST statement of this function's own seeding transaction.
///
/// For every entry in `chain` this inserts a `MigrationRow` carrying its
/// rendered down-SQL (or unsupported reason) and checksum, with
/// `applied_at` set to now — but executes none of the chain's up-SQL, since
/// the fresh schema is already at head. This makes a freshly created,
/// newer-than-some-other-binary store downgradable by that older binary:
/// it can read these rows' down-SQL without ever having run the
/// corresponding up-SQL itself.
///
/// Stamping `user_version` here — as the final operation inside the same
/// transaction that inserts the seed rows — rather than in `create_schema`,
/// is deliberate crash-safety ordering: `create_schema` no longer touches
/// `user_version` at all (see its doc comment). If the process is
/// interrupted between `create_schema` finishing and this function's
/// transaction committing, `user_version` is still `0`, which
/// `connection.rs`'s `classify_version` reads as `Fresh` rather than
/// `AtHead` — so the next open simply re-runs `create_schema` (idempotent:
/// every statement is `CREATE ... IF NOT EXISTS`) and this function again.
/// That retry is safe because this function's own transaction is atomic: an
/// interruption mid-transaction rolls back the whole thing (no partial rows,
/// `user_version` unchanged), so the retry starts from the same clean slate
/// rather than fighting a half-seeded table. Contrast this with the old
/// ordering (stamp-then-seed), where an interruption left a store that
/// *read* as "at head" but was missing rows — failing
/// `checksum::verify_checksums`'s completeness check with no recovery path
/// short of manual editing.
pub async fn seed_for_fresh_create(
    conn: &Connection,
    chain: &[Migration],
    ctx: &MigrationContext,
) -> Result<(), Error> {
    validate_chain(chain)?;

    table::ensure_table(conn).await.map_err(map_libsql_err)?;
    table::ensure_baseline_row(conn)
        .await
        .map_err(map_libsql_err)?;

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(map_libsql_err)?;

    if let Err(e) = seed_all_and_stamp(&tx, chain, ctx).await {
        if let Err(rollback_err) = tx.rollback().await {
            tracing::error!(error = %rollback_err, "rollback after failed seed also failed");
        }
        return Err(Error::Internal {
            message: format!("seeding schema_migrations for fresh create failed: {e}"),
            correlation_id: "libsql_migrations_seed_failed".to_string(),
        });
    }

    tx.commit().await.map_err(map_libsql_err)?;
    Ok(())
}

async fn seed_all(
    tx: &Connection,
    chain: &[Migration],
    ctx: &MigrationContext,
) -> Result<(), libsql::Error> {
    for migration in chain {
        let (down_sql, down_unsupported_reason) = render_down(migration, ctx);
        let checksum = migration_checksum(migration, ctx);

        // Two processes can race to create the same brand-new store: both
        // observe `PRAGMA user_version == 0` (in `connection.rs`'s `open`,
        // outside any transaction) and both reach here to seed the same
        // chain. `BEGIN IMMEDIATE` serializes their transactions but does
        // not stop the loser from re-deriving and re-inserting rows the
        // winner already committed — `table::ensure_baseline_row` already
        // tolerates exactly this race for the baseline row via its own
        // `INSERT OR IGNORE`. Do the same here, but only when the existing
        // row's checksum matches what this seed would have produced: a
        // mismatch means some *other* row already occupies this version —
        // genuine corruption, not a benign duplicate — and must still fail
        // loudly rather than being silently papered over.
        if let Some(existing) = table::find_row(tx, migration.version).await? {
            if existing.checksum == checksum {
                continue;
            }
            return Err(libsql::Error::SqliteFailure(
                0,
                format!(
                    "schema_migrations already has a row for version {version} \
                     ('{existing_name}') that doesn't match migration '{name}' being seeded \
                     (checksum {existing_checksum} != {checksum}); this is not the benign \
                     concurrent-fresh-create race seeding tolerates, refusing to overwrite it",
                    version = migration.version,
                    existing_name = existing.name,
                    existing_checksum = existing.checksum,
                    name = migration.name,
                ),
            ));
        }

        let row = MigrationRow {
            version: migration.version,
            name: migration.name.to_string(),
            applied_at: localdb_core::ingestion::now_rfc3339(),
            down_sql,
            down_unsupported_reason,
            checksum,
        };
        table::insert_row(tx, &row).await?;
    }
    Ok(())
}

/// `seed_all` plus the final `PRAGMA user_version` stamp, run as one
/// fallible unit so `seed_for_fresh_create` rolls both back together on
/// failure — see that function's doc comment for why the stamp must be
/// last.
async fn seed_all_and_stamp(
    tx: &Connection,
    chain: &[Migration],
    ctx: &MigrationContext,
) -> Result<(), libsql::Error> {
    seed_all(tx, chain, ctx).await?;

    let head = head_version(chain);
    tx.query(&format!("PRAGMA user_version = {head}"), ())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::chain::migrations as real_migrations;
    use crate::migrations::{baseline, RustStep};
    use libsql::Builder;
    use localdb_core::VectorEncoding;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn ctx() -> MigrationContext {
        MigrationContext {
            embedding_dim: 4,
            encoding: VectorEncoding::Float32,
        }
    }

    async fn open_test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
        (dir, conn)
    }

    async fn open_baseline_db() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = open_test_db().await;
        baseline::create_baseline_schema(&conn, &ctx())
            .await
            .unwrap();
        (dir, conn)
    }

    // -- Fixture chain: v5 creates `toys`, v6 adds a column, v7 is
    // unsupported-down. -----------------------------------------------------

    fn v5_up(_ctx: &MigrationContext) -> Vec<String> {
        vec![
            "CREATE TABLE toys (id INTEGER PRIMARY KEY, label TEXT)".to_string(),
            "CREATE INDEX idx_toys_label ON toys(label)".to_string(),
        ]
    }
    fn v5_down(_ctx: &MigrationContext) -> Vec<String> {
        vec![
            "DROP INDEX idx_toys_label".to_string(),
            "DROP TABLE toys".to_string(),
        ]
    }

    fn v6_up(_ctx: &MigrationContext) -> Vec<String> {
        vec!["ALTER TABLE toys ADD COLUMN color TEXT".to_string()]
    }
    fn v6_down(_ctx: &MigrationContext) -> Vec<String> {
        vec!["ALTER TABLE toys DROP COLUMN color".to_string()]
    }

    fn v7_up(_ctx: &MigrationContext) -> Vec<String> {
        vec!["CREATE TABLE gizmos (id INTEGER PRIMARY KEY)".to_string()]
    }

    fn fixture_chain() -> Vec<Migration> {
        vec![
            Migration {
                version: BASELINE_VERSION + 1,
                name: "create_toys",
                summary: "fixture: creates the toys table",
                up: Up::Sql(v5_up),
                down: Down::Sql(v5_down),
                needs_reindex: false,
            },
            Migration {
                version: BASELINE_VERSION + 2,
                name: "add_toy_color",
                summary: "fixture: adds toys.color",
                up: Up::Sql(v6_up),
                down: Down::Sql(v6_down),
                needs_reindex: false,
            },
            Migration {
                version: BASELINE_VERSION + 3,
                name: "add_gizmos_unsupported_down",
                summary: "fixture: irreversible add of gizmos",
                up: Up::Sql(v7_up),
                down: Down::Unsupported("fixture migration has no down path"),
                needs_reindex: false,
            },
        ]
    }

    /// A fixture whose up-SQL has a valid first statement and an invalid
    /// second one, for the partial-failure rollback gate test.
    fn failing_chain() -> Vec<Migration> {
        fn up(_ctx: &MigrationContext) -> Vec<String> {
            vec![
                "CREATE TABLE will_be_rolled_back (id INTEGER PRIMARY KEY)".to_string(),
                "THIS IS NOT VALID SQL".to_string(),
            ]
        }
        fn down(_ctx: &MigrationContext) -> Vec<String> {
            vec!["DROP TABLE will_be_rolled_back".to_string()]
        }
        vec![Migration {
            version: BASELINE_VERSION + 1,
            name: "broken_migration",
            summary: "fixture: second statement is invalid SQL",
            up: Up::Sql(up),
            down: Down::Sql(down),
            needs_reindex: false,
        }]
    }

    /// A Rust-step fixture that inserts data then fails, for the
    /// Rust-step-transactionality test.
    struct FailingRustStep {
        insert_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RustStep for FailingRustStep {
        async fn apply(
            &self,
            conn: &Connection,
            _ctx: &MigrationContext,
        ) -> Result<(), libsql::Error> {
            conn.execute("CREATE TABLE rust_step_marks (id INTEGER PRIMARY KEY)", ())
                .await?;
            conn.execute("INSERT INTO rust_step_marks (id) VALUES (1)", ())
                .await?;
            self.insert_count.fetch_add(1, Ordering::SeqCst);
            Err(libsql::Error::SqliteFailure(
                1,
                "fixture rust step intentionally fails".to_string(),
            ))
        }

        fn checksum_repr(&self) -> &'static str {
            "fixture_failing_rust_step_v1"
        }
    }

    fn rust_step_chain(insert_count: Arc<AtomicUsize>) -> Vec<Migration> {
        vec![Migration {
            version: BASELINE_VERSION + 1,
            name: "failing_rust_step",
            summary: "fixture: rust step that fails after writing",
            up: Up::Rust(Box::new(FailingRustStep { insert_count })),
            down: Down::Unsupported("fixture never succeeds, so never needs a down path"),
            needs_reindex: false,
        }]
    }

    async fn normalized_master_rows(conn: &Connection) -> Vec<(String, String, String)> {
        let mut rows = conn
            .query(
                "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE 'chunks_fts_%' \
                 AND name != 'schema_migrations' \
                 ORDER BY type, name",
                (),
            )
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            out.push((
                row.get::<String>(0).unwrap(),
                row.get::<String>(1).unwrap(),
                row.get::<String>(2).unwrap(),
            ));
        }
        out
    }

    async fn user_version(conn: &Connection) -> i64 {
        get_schema_version(conn).await.unwrap()
    }

    // 1. Incremental application equals the same DDL applied cumulatively by
    // hand (plan test 5).
    #[tokio::test]
    async fn apply_pending_matches_cumulative_ddl_and_advances_bookkeeping() {
        let (_dir_a, conn_a) = open_baseline_db().await;
        let chain = fixture_chain();
        let report = apply_pending(&conn_a, &chain, &ctx()).await.unwrap();
        assert_eq!(report.applied.len(), 3);
        assert_eq!(
            report.applied.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![
                BASELINE_VERSION + 1,
                BASELINE_VERSION + 2,
                BASELINE_VERSION + 3
            ]
        );

        let (_dir_b, conn_b) = open_baseline_db().await;
        for stmt in v5_up(&ctx()) {
            conn_b.execute(&stmt, ()).await.unwrap();
        }
        for stmt in v6_up(&ctx()) {
            conn_b.execute(&stmt, ()).await.unwrap();
        }
        for stmt in v7_up(&ctx()) {
            conn_b.execute(&stmt, ()).await.unwrap();
        }

        assert_eq!(
            normalized_master_rows(&conn_a).await,
            normalized_master_rows(&conn_b).await,
            "incrementally-applied schema must match cumulatively-applied schema"
        );

        assert_eq!(user_version(&conn_a).await, BASELINE_VERSION + 3);

        let rows = table::list_rows_desc_above(&conn_a, BASELINE_VERSION - 1)
            .await
            .unwrap();
        let mut versions: Vec<i64> = rows.iter().map(|r| r.version).collect();
        versions.sort();
        assert_eq!(
            versions,
            vec![
                BASELINE_VERSION,
                BASELINE_VERSION + 1,
                BASELINE_VERSION + 2,
                BASELINE_VERSION + 3
            ],
            "schema_migrations must have rows for baseline..=head"
        );
    }

    // 2. Up-then-down restores the prior schema (plan test 6), exercised both
    // over the fixture chain and (trivially, since it's empty today) the
    // real registry.
    // Shared by both `up_then_down_restores_prior_schema_*` tests below.
    //
    // Applying a *prefix* of `chain` (rather than a lone middle element) at
    // each step matters: `apply_pending` calls `validate_chain` on whatever
    // slice it's given, which requires the slice to start at
    // `BASELINE_VERSION + 1`. A prefix always satisfies that; a lone
    // mid-chain element wouldn't. Passing the growing prefix each time is
    // also exactly how a real caller would use `apply_pending` — the
    // already-applied entries are filtered out internally by comparing
    // against `PRAGMA user_version`, so only entry `i` actually runs.
    async fn assert_up_then_down_restores_schema(chain: &[Migration]) {
        for i in 0..chain.len() {
            if matches!(&chain[i].down, Down::Unsupported(_)) {
                continue; // nothing to replay
            }

            let (_dir, conn) = open_baseline_db().await;
            if i > 0 {
                apply_pending(&conn, &chain[..i], &ctx()).await.unwrap();
            }
            let before = normalized_master_rows(&conn).await;

            apply_pending(&conn, &chain[..=i], &ctx()).await.unwrap();

            let rows = table::list_rows_desc_above(&conn, BASELINE_VERSION + i as i64)
                .await
                .unwrap();
            let down_stmts = rows[0].down_sql.clone().expect("down_sql must be Some");
            for stmt in &down_stmts {
                conn.execute(stmt, ()).await.unwrap();
            }
            table::delete_row(&conn, chain[i].version).await.unwrap();
            conn.query(
                &format!("PRAGMA user_version = {}", BASELINE_VERSION + i as i64),
                (),
            )
            .await
            .unwrap();

            let after = normalized_master_rows(&conn).await;
            assert_eq!(
                before, after,
                "replaying down_sql for '{}' should restore the prior schema",
                chain[i].name
            );
        }
    }

    #[tokio::test]
    async fn up_then_down_restores_prior_schema_fixture_chain() {
        assert_up_then_down_restores_schema(&fixture_chain()).await;
    }

    #[tokio::test]
    async fn up_then_down_restores_prior_schema_real_registry() {
        // Currently empty; this loop runs zero times but exercises real
        // entries the moment consumers append them.
        assert_up_then_down_restores_schema(&real_migrations()).await;
    }

    // 3. Data preservation across an ALTER TABLE (plan test 7).
    #[tokio::test]
    async fn data_survives_alter_table_migration() {
        let (_dir, conn) = open_baseline_db().await;
        let chain = fixture_chain();

        apply_pending(&conn, &chain[..1], &ctx()).await.unwrap();
        conn.execute("INSERT INTO toys (id, label) VALUES (1, 'robot')", ())
            .await
            .unwrap();

        apply_pending(&conn, &chain[..2], &ctx()).await.unwrap();

        let mut rows = conn
            .query("SELECT id, label, color FROM toys WHERE id = 1", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().expect("row must survive ALTER");
        let id: i64 = row.get(0).unwrap();
        let label: String = row.get(1).unwrap();
        let color: Option<String> = row.get(2).unwrap();
        assert_eq!(id, 1);
        assert_eq!(label, "robot");
        assert_eq!(color, None, "new column should be NULL on preexisting rows");
    }

    // 4. Partial-failure rollback (plan test 8) — THE GATE for the
    // Drop-vs-explicit-rollback question: proves that after a step's second
    // statement fails, the first statement's effect, the schema_migrations
    // row, and user_version are all rolled back together.
    #[tokio::test]
    async fn partial_failure_rolls_back_whole_step() {
        let (_dir, conn) = open_baseline_db().await;
        let chain = failing_chain();

        let before_version = user_version(&conn).await;
        let result = apply_pending(&conn, &chain, &ctx()).await;
        assert!(result.is_err(), "the broken migration must fail");

        assert_eq!(
            user_version(&conn).await,
            before_version,
            "user_version must be unchanged after a failed migration"
        );

        let rows = table::list_rows_desc_above(&conn, BASELINE_VERSION)
            .await
            .unwrap();
        assert!(
            rows.is_empty(),
            "no schema_migrations row should exist for the failed migration"
        );

        let mut check = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='will_be_rolled_back'",
                (),
            )
            .await
            .unwrap();
        assert!(
            check.next().await.unwrap().is_none(),
            "the first (valid) statement's effect must be rolled back along with the second \
             (invalid) statement's failure — this is the explicit-rollback gate: if this \
             assertion fails, Transaction's Drop alone isn't rolling back local libsql \
             connections and the runner must fall back to raw BEGIN/COMMIT/ROLLBACK strings"
        );
    }

    // 5. Rust-step transactionality: a Rust step's DML rolls back with the
    // rest of the transaction when it returns an error.
    #[tokio::test]
    async fn rust_step_failure_rolls_back_its_own_dml() {
        let (_dir, conn) = open_baseline_db().await;
        let insert_count = Arc::new(AtomicUsize::new(0));
        let chain = rust_step_chain(insert_count.clone());

        let result = apply_pending(&conn, &chain, &ctx()).await;
        assert!(result.is_err());
        assert_eq!(
            insert_count.load(Ordering::SeqCst),
            1,
            "the step should have run (and inserted) before failing"
        );

        let mut check = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='rust_step_marks'",
                (),
            )
            .await
            .unwrap();
        assert!(
            check.next().await.unwrap().is_none(),
            "the rust step's CREATE TABLE + INSERT must be rolled back with the transaction"
        );
    }

    // 6. seed_for_fresh_create backfills bookkeeping (down-SQL/checksums per
    // chain entry) without running any up-SQL, and stamps user_version to
    // head as the last step of its own transaction (the atomic-stamp
    // contract itself is pinned separately by
    // `seed_for_fresh_create_stamps_head_atomically` and
    // `seed_for_fresh_create_failure_leaves_user_version_untouched` below) —
    // this test's focus is the bookkeeping-rows contract.
    #[tokio::test]
    async fn seed_for_fresh_create_backfills_rows_without_running_up_sql() {
        // Build a "fresh head" DB by hand: baseline DDL plus the fixture
        // chain's DDL applied cumulatively (create_schema no longer stamps
        // user_version itself, so this test builds its own "fresh at head"
        // fixture rather than reusing schema::create_schema directly).
        let (_dir, conn) = open_baseline_db().await;
        let chain = fixture_chain();
        let c = ctx();
        for stmt in v5_up(&c) {
            conn.execute(&stmt, ()).await.unwrap();
        }
        for stmt in v6_up(&c) {
            conn.execute(&stmt, ()).await.unwrap();
        }
        for stmt in v7_up(&c) {
            conn.execute(&stmt, ()).await.unwrap();
        }
        let head = BASELINE_VERSION + chain.len() as i64;

        seed_for_fresh_create(&conn, &chain, &c).await.unwrap();

        assert_eq!(
            user_version(&conn).await,
            head,
            "seed_for_fresh_create stamps user_version to the chain's head"
        );

        let rows = table::list_rows_desc_above(&conn, BASELINE_VERSION - 1)
            .await
            .unwrap();
        let mut versions: HashSet<i64> = rows.iter().map(|r| r.version).collect();
        assert!(versions.remove(&BASELINE_VERSION), "baseline row missing");
        for migration in &chain {
            assert!(
                versions.remove(&migration.version),
                "row for chain entry '{}' missing",
                migration.name
            );
        }
        assert!(versions.is_empty(), "unexpected extra rows: {versions:?}");

        for migration in &chain {
            let row = rows
                .iter()
                .find(|r| r.version == migration.version)
                .unwrap();
            assert_eq!(row.checksum, migration_checksum(migration, &c));
            match &migration.down {
                Down::Sql(render) => {
                    assert_eq!(row.down_sql.as_deref(), Some(render(&c).as_slice()));
                    assert!(row.down_unsupported_reason.is_none());
                }
                Down::Unsupported(reason) => {
                    assert!(row.down_sql.is_none());
                    assert_eq!(row.down_unsupported_reason.as_deref(), Some(*reason));
                }
            }
        }
    }

    // 6a. Codex review #152 fix 1: seed_for_fresh_create stamps
    // user_version to head as the LAST operation of its own transaction,
    // atomically with the seed rows — on a bare (never-touched, version 0)
    // database, exactly like `connection.rs`'s `Fresh` branch hands it.
    #[tokio::test]
    async fn seed_for_fresh_create_stamps_head_atomically() {
        let (_dir, conn) = open_test_db().await;
        let chain = fixture_chain();
        let c = ctx();

        assert_eq!(user_version(&conn).await, 0, "precondition: untouched db");

        seed_for_fresh_create(&conn, &chain, &c).await.unwrap();

        let head = BASELINE_VERSION + chain.len() as i64;
        assert_eq!(
            user_version(&conn).await,
            head,
            "seed_for_fresh_create must stamp user_version to head"
        );

        let rows = table::list_rows_desc_above(&conn, BASELINE_VERSION - 1)
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            chain.len() + 1,
            "baseline row plus one row per chain entry should exist: {rows:?}"
        );
    }

    // 6b. If seeding fails partway, the whole transaction — including the
    // user_version stamp, which is the LAST statement in it — rolls back
    // together, leaving user_version at 0 rather than partially advanced.
    //
    // Force the failure with a *mismatching* pre-existing row at the first
    // chain entry's version, not just any collision: `seed_all` tolerates a
    // collision whose checksum matches what it would have produced itself
    // (the benign concurrent-fresh-create race, see
    // `seed_all_tolerates_a_matching_concurrent_seed_race` below) — only a
    // checksum mismatch (this row's `checksum: "bogus"` can never match a
    // real `migration_checksum` output) is treated as genuine corruption and
    // still fails.
    #[tokio::test]
    async fn seed_for_fresh_create_failure_leaves_user_version_untouched() {
        let (_dir, conn) = open_test_db().await;
        let chain = fixture_chain();
        let c = ctx();

        table::ensure_table(&conn).await.unwrap();
        table::insert_row(
            &conn,
            &MigrationRow {
                version: chain[0].version,
                name: "colliding_row".to_string(),
                applied_at: "2024-01-01T00:00:00Z".to_string(),
                down_sql: Some(vec!["SELECT 1".to_string()]),
                down_unsupported_reason: None,
                checksum: "bogus".to_string(),
            },
        )
        .await
        .unwrap();

        assert_eq!(user_version(&conn).await, 0, "precondition: untouched db");

        let result = seed_for_fresh_create(&conn, &chain, &c).await;
        assert!(
            result.is_err(),
            "a mismatching pre-existing row should make seeding fail"
        );

        assert_eq!(
            user_version(&conn).await,
            0,
            "a failed seed must leave user_version at 0, not partially stamped"
        );
    }

    // 6c. The concurrent-fresh-create race this whole checksum-comparison
    // exists for: two processes race to create the same brand-new store, so
    // by the time the second one's `seed_all` runs, rows for every chain
    // entry (seeded by the first, from the exact same compiled chain and
    // context) already exist. That must be tolerated as a no-op per row, not
    // surfaced as an internal error — this is what distinguishes it from
    // `seed_for_fresh_create_failure_leaves_user_version_untouched` above.
    #[tokio::test]
    async fn seed_for_fresh_create_tolerates_a_matching_concurrent_seed_race() {
        let (_dir, conn) = open_test_db().await;
        let chain = fixture_chain();
        let c = ctx();

        // Simulate the "winner": seed the chain once, successfully.
        seed_for_fresh_create(&conn, &chain, &c).await.unwrap();
        let head = BASELINE_VERSION + chain.len() as i64;
        assert_eq!(user_version(&conn).await, head);

        // Simulate the "loser" re-running seed_all's row-by-row logic
        // directly against the same now-already-seeded connection (standing
        // in for a second process's transaction observing the same rows
        // after `BEGIN IMMEDIATE` unblocks it) — every row it would produce
        // already exists with a matching checksum, so this must succeed as
        // a no-op rather than erroring on the primary-key collision.
        seed_all(&conn, &chain, &c)
            .await
            .expect("re-seeding identical rows for a concurrent-race loser must be a no-op");

        // Untouched: still exactly one row per chain entry plus baseline,
        // still at head.
        assert_eq!(user_version(&conn).await, head);
        let rows = table::list_rows_desc_above(&conn, BASELINE_VERSION - 1)
            .await
            .unwrap();
        assert_eq!(rows.len(), chain.len() + 1);
    }

    // 7. apply_pending is a no-op once the database is already at head.
    #[tokio::test]
    async fn apply_pending_is_noop_at_head() {
        let (_dir, conn) = open_baseline_db().await;
        let chain = fixture_chain();
        apply_pending(&conn, &chain, &ctx()).await.unwrap();

        let before_master = normalized_master_rows(&conn).await;
        let before_rows = table::list_rows_desc_above(&conn, BASELINE_VERSION - 1)
            .await
            .unwrap();

        let report = apply_pending(&conn, &chain, &ctx()).await.unwrap();
        assert!(report.applied.is_empty());

        assert_eq!(normalized_master_rows(&conn).await, before_master);
        assert_eq!(
            table::list_rows_desc_above(&conn, BASELINE_VERSION - 1)
                .await
                .unwrap(),
            before_rows
        );
    }

    // 8. Drift guard, real registry (plan test 4): a database built by
    // `schema::create_schema` (the "always current" helper) must match one
    // built by frozen baseline DDL plus the *compiled, real* migration
    // chain applied on top.
    //
    // This is the write-twice rule: every schema change must be landed BOTH
    // as a chain entry in `chain::migrations()` AND folded into
    // `schema::create_schema` (see docs/migrations.md). If you add a chain
    // entry without updating `create_schema` to match — or update
    // `create_schema` without adding a chain entry — this test fails. It
    // supersedes `baseline::baseline_schema_matches_current_create_schema_verbatim`,
    // which only held while the chain was empty.
    #[tokio::test]
    async fn drift_guard_create_schema_equals_baseline_plus_chain() {
        let (_dir_a, conn_a) = open_test_db().await;
        crate::schema::create_schema(&conn_a, 4, VectorEncoding::Float32)
            .await
            .unwrap();

        let (_dir_b, conn_b) = open_baseline_db().await;
        apply_pending(&conn_b, &real_migrations(), &ctx())
            .await
            .unwrap();

        assert_eq!(
            normalized_master_rows(&conn_a).await,
            normalized_master_rows(&conn_b).await,
            "schema::create_schema must produce the same DDL as baseline + the real chain \
             (write-twice rule — see docs/migrations.md)"
        );
    }
}
