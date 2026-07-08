//! `db downgrade` and `db status`'s read-only backend.
//!
//! `downgrade_store` replays only the *stored* down-SQL rows in a store's own
//! `schema_migrations` table, never the compiled migration chain — see its
//! doc comment for why that asymmetry matters. `inspect_schema` is the
//! never-refuses read used by `db status`.

use std::path::Path;
use std::time::{Duration, Instant};

use libsql::{Connection, TransactionBehavior};
use localdb_core::Error;

use super::chain::{self, BASELINE_VERSION};
use super::maintenance::open_for_maintenance;
use super::table::{self, MigrationRow};
use crate::connection::map_libsql_err;
use crate::schema;

/// One migration stepped back during a [`downgrade_store`] run.
#[derive(Debug, Clone)]
pub struct DowngradeStep {
    pub version: i64,
    pub name: String,
    pub duration: Duration,
}

/// The result of one `downgrade_store` run.
#[derive(Debug, Clone)]
pub struct DowngradeReport {
    pub from_version: i64,
    pub to_version: i64,
    /// Steps actually replayed, newest-first (the order they were applied
    /// in).
    pub steps: Vec<DowngradeStep>,
}

/// A store's schema-migration state, for `db status` — read-only, and never
/// refuses (unlike `migrate_store`/`downgrade_store`), even for a store
/// newer than this binary or one that predates the migration framework.
#[derive(Debug, Clone)]
pub struct SchemaStatus {
    pub current_version: i64,
    /// This binary's head version (`chain::head_version_current()`), for
    /// comparison against `current_version`.
    pub head_version: i64,
    pub baseline_version: i64,
    /// Full `schema_migrations` history, ascending by version. Empty if
    /// `table_present` is `false`.
    pub rows: Vec<MigrationRow>,
    /// `true` if `0 < current_version < baseline_version` (a pre-framework
    /// v1-v3 store).
    pub legacy: bool,
    /// `true` if the `schema_migrations` table exists at all. A healthy
    /// store always has it; a raw pre-framework store may not.
    pub table_present: bool,
}

/// Reverse the store at `path`'s schema down to `target` (default:
/// [`BASELINE_VERSION`]) by replaying **only** the down-SQL already stored in
/// that store's own `schema_migrations` table.
///
/// Deliberately takes no `&[Migration]` chain — that's the whole point:
/// because every migration's down-SQL is rendered once and persisted as data
/// at apply time (see `runner.rs`), a binary that has never heard of a given
/// migration can still undo it, as long as some *other*, newer binary
/// applied it first. Accepting a compiled chain here instead would make that
/// impossible, since an old binary's chain simply has no entry for a
/// migration it doesn't know about yet. This is what lets an old binary step
/// back a store a newer binary left behind.
pub async fn downgrade_store(path: &Path, target: Option<i64>) -> Result<DowngradeReport, Error> {
    let (_db, conn) = open_for_maintenance(path).await?;

    let from_version = schema::get_schema_version(&conn)
        .await
        .map_err(map_libsql_err)?;
    let target = target.unwrap_or(BASELINE_VERSION);

    if target < BASELINE_VERSION {
        return Err(Error::InvalidConfig {
            message: format!(
                "cannot downgrade below the frozen baseline version {BASELINE_VERSION}: the \
                 baseline schema predates the migration framework and has no down-SQL to replay"
            ),
        });
    }
    if target >= from_version {
        return Err(Error::InvalidConfig {
            message: format!(
                "nothing to downgrade: target version {target} must be below the current \
                 version {from_version}"
            ),
        });
    }

    if !table_exists(&conn, "schema_migrations")
        .await
        .map_err(map_libsql_err)?
    {
        return Err(Error::InvalidConfig {
            message: "this store has no schema_migrations table yet — it predates the \
                       migration framework entirely; run 'localdb db migrate' first"
                .to_string(),
        });
    }

    let mut rows = table::list_rows_desc_above(&conn, target)
        .await
        .map_err(map_libsql_err)?;
    // Defensive: only ever replay rows at or below the version we actually
    // read `PRAGMA user_version` as, even if schema_migrations somehow has
    // stale rows above it.
    rows.retain(|r| r.version <= from_version);

    // Refuse up front, before touching anything, if any planned row can't be
    // reversed — naming the nearest target that *is* reachable rather than
    // stopping partway through an already-started downgrade.
    if let Some(blocked) = rows.iter().find(|r| r.down_unsupported_reason.is_some()) {
        let reason = blocked.down_unsupported_reason.as_deref().unwrap_or("");
        return Err(Error::InvalidConfig {
            message: format!(
                "cannot downgrade past migration '{name}' (version {version}): {reason}. \
                 Nothing was changed. Downgrade to version {version} instead (`db downgrade \
                 --to {version}`) to keep it applied and only replay the migrations above it.",
                name = blocked.name,
                version = blocked.version,
            ),
        });
    }

    let mut steps = Vec::new();
    for row in &rows {
        let started = Instant::now();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(map_libsql_err)?;

        if let Err(e) = replay_one(&tx, row).await {
            if let Err(rollback_err) = tx.rollback().await {
                tracing::error!(
                    migration = row.name,
                    version = row.version,
                    error = %rollback_err,
                    "rollback after failed downgrade step also failed"
                );
            }
            return Err(Error::Internal {
                message: format!(
                    "downgrade of migration '{name}' (version {version}) failed and was rolled \
                     back: {e}",
                    name = row.name,
                    version = row.version,
                ),
                correlation_id: "libsql_migrations_downgrade_failed".to_string(),
            });
        }

        tx.commit().await.map_err(map_libsql_err)?;

        let duration = started.elapsed();
        tracing::info!(
            version = row.version,
            name = row.name,
            duration_ms = duration.as_millis() as u64,
            "downgraded migration"
        );
        eprintln!(
            "downgraded migration v{version} '{name}' in {duration_ms}ms",
            version = row.version,
            name = row.name,
            duration_ms = duration.as_millis(),
        );

        steps.push(DowngradeStep {
            version: row.version,
            name: row.name.clone(),
            duration,
        });
    }

    Ok(DowngradeReport {
        from_version,
        to_version: target,
        steps,
    })
}

/// Replay one row's stored down-SQL, delete its `schema_migrations` row, and
/// step `PRAGMA user_version` back by one — all against `tx`. Caller owns
/// commit/rollback.
async fn replay_one(tx: &Connection, row: &MigrationRow) -> Result<(), libsql::Error> {
    let down_sql = row.down_sql.as_ref().unwrap_or_else(|| {
        unreachable!(
            "downgrade_store pre-scans for down_unsupported_reason rows before replaying; \
             row for version {} must have down_sql",
            row.version
        )
    });
    for stmt in down_sql {
        tx.execute(stmt, ()).await?;
    }
    table::delete_row(tx, row.version).await?;
    // PRAGMAs may return rows; use query() not execute() (see
    // schema::set_user_version).
    tx.query(&format!("PRAGMA user_version = {}", row.version - 1), ())
        .await?;
    Ok(())
}

async fn table_exists(conn: &Connection, name: &str) -> Result<bool, libsql::Error> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
            libsql::params![name],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

/// Read-only schema inspection for `db status`. Never refuses: a store
/// that's too new, predates the migration framework, or has never been
/// touched is all reportable state, not an error.
pub async fn inspect_schema(path: &Path) -> Result<SchemaStatus, Error> {
    let (_db, conn) = open_for_maintenance(path).await?;

    let current_version = schema::get_schema_version(&conn)
        .await
        .map_err(map_libsql_err)?;
    let head_version = chain::head_version_current();
    let table_present = table_exists(&conn, "schema_migrations")
        .await
        .map_err(map_libsql_err)?;

    let rows = if table_present {
        let mut descending = table::list_rows_desc_above(&conn, i64::MIN)
            .await
            .map_err(map_libsql_err)?;
        descending.reverse(); // -> ascending by version
        descending
    } else {
        Vec::new()
    };

    let legacy = current_version > 0 && current_version < BASELINE_VERSION;

    Ok(SchemaStatus {
        current_version,
        head_version,
        baseline_version: BASELINE_VERSION,
        rows,
        legacy,
        table_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_fixtures;

    #[tokio::test]
    async fn downgrade_store_default_target_replays_stored_down_sql_back_to_baseline() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_plus_chain(&path, &test_fixtures::reversible_chain()).await;

        let report = downgrade_store(&path, None).await.unwrap();
        assert_eq!(report.from_version, BASELINE_VERSION + 3);
        assert_eq!(report.to_version, BASELINE_VERSION);
        assert_eq!(
            report.steps.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![
                BASELINE_VERSION + 3,
                BASELINE_VERSION + 2,
                BASELINE_VERSION + 1
            ],
            "steps must replay newest-first"
        );

        let (_db, conn) = open_for_maintenance(&path).await.unwrap();
        assert_eq!(
            schema::get_schema_version(&conn).await.unwrap(),
            BASELINE_VERSION
        );
        let remaining = table::list_rows_desc_above(&conn, BASELINE_VERSION)
            .await
            .unwrap();
        assert!(
            remaining.is_empty(),
            "no schema_migrations rows above baseline should remain: {remaining:?}"
        );

        let after_master = test_fixtures::normalized_master_rows(&conn).await;
        drop(conn);

        let (_fresh_dir, fresh_path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_db(&fresh_path).await;
        let (_fresh_db, fresh_conn) = open_for_maintenance(&fresh_path).await.unwrap();
        let fresh_master = test_fixtures::normalized_master_rows(&fresh_conn).await;

        assert_eq!(
            after_master, fresh_master,
            "a full downgrade must restore exactly a fresh baseline DB's schema"
        );
    }

    #[tokio::test]
    async fn downgrade_store_to_intermediate_target_replays_only_rows_above_it() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_plus_chain(&path, &test_fixtures::reversible_chain()).await;

        let target = BASELINE_VERSION + 1;
        let report = downgrade_store(&path, Some(target)).await.unwrap();
        assert_eq!(report.from_version, BASELINE_VERSION + 3);
        assert_eq!(report.to_version, target);
        assert_eq!(
            report.steps.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![BASELINE_VERSION + 3, BASELINE_VERSION + 2]
        );

        let (_db, conn) = open_for_maintenance(&path).await.unwrap();
        assert_eq!(schema::get_schema_version(&conn).await.unwrap(), target);

        let mut remaining_versions: Vec<i64> =
            table::list_rows_desc_above(&conn, BASELINE_VERSION - 1)
                .await
                .unwrap()
                .iter()
                .map(|r| r.version)
                .collect();
        remaining_versions.sort();
        assert_eq!(
            remaining_versions,
            vec![BASELINE_VERSION, BASELINE_VERSION + 1],
            "only the baseline row and the still-applied +1 row should remain"
        );
    }

    #[tokio::test]
    async fn downgrade_store_refuses_past_unsupported_migration_and_leaves_db_untouched() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_plus_chain(
            &path,
            &test_fixtures::chain_with_unsupported_middle(),
        )
        .await;

        let before = test_fixtures::dump_db(&path).await;
        let result = downgrade_store(&path, None).await;
        let after = test_fixtures::dump_db(&path).await;

        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("drop_widget_notes_irreversibly"),
                    "should name the blocking migration: {message}"
                );
                assert!(
                    message.contains(&(BASELINE_VERSION + 2).to_string()),
                    "should name the blocking version: {message}"
                );
                assert!(
                    message.contains("fixture migration has no down path"),
                    "should include the stored reason: {message}"
                );
                assert!(
                    message.contains("--to"),
                    "should suggest the reachable --to target: {message}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        assert_eq!(
            before, after,
            "a refused downgrade must not mutate the store at all"
        );
    }

    #[tokio::test]
    async fn downgrade_store_to_the_unsupported_versions_own_target_succeeds() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_plus_chain(
            &path,
            &test_fixtures::chain_with_unsupported_middle(),
        )
        .await;

        // Targeting the Unsupported migration's own version keeps it
        // applied and only replays the (reversible) row above it.
        let target = BASELINE_VERSION + 2;
        let report = downgrade_store(&path, Some(target)).await.unwrap();
        assert_eq!(report.to_version, target);
        assert_eq!(
            report.steps.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![BASELINE_VERSION + 3]
        );

        let (_db, conn) = open_for_maintenance(&path).await.unwrap();
        assert_eq!(schema::get_schema_version(&conn).await.unwrap(), target);
        let remaining = table::list_rows_desc_above(&conn, target).await.unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn downgrade_store_rejects_target_below_baseline() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_plus_chain(&path, &test_fixtures::reversible_chain()).await;

        let result = downgrade_store(&path, Some(BASELINE_VERSION - 1)).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(message.contains("baseline"), "message: {message}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn downgrade_store_rejects_target_at_or_above_current_version() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_plus_chain(&path, &test_fixtures::reversible_chain()).await;

        let current = BASELINE_VERSION + 3;
        let result = downgrade_store(&path, Some(current)).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("nothing to downgrade"),
                    "message: {message}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn downgrade_store_without_migrations_table_is_refused() {
        // A store above baseline with no `schema_migrations` table is not
        // reachable through any normal migrate/apply path (both always
        // create the table) — this simulates a corrupted/tampered-with
        // store to exercise the defensive check. (A raw, untouched baseline
        // store has `current_version == BASELINE_VERSION` exactly, which
        // `downgrade_store`'s default target also resolves to, so it hits
        // "nothing to downgrade" before ever reaching the table check —
        // itself the correct behavior for that case.)
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_db(&path).await;
        test_fixtures::stamp_user_version(&path, BASELINE_VERSION + 1).await;

        let result = downgrade_store(&path, None).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(message.contains("db migrate"), "message: {message}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inspect_schema_on_healthy_store_reports_table_present_and_baseline_row() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_healthy_baseline_store(&path).await;

        let status = inspect_schema(&path).await.unwrap();
        assert_eq!(status.current_version, BASELINE_VERSION);
        assert_eq!(status.head_version, chain::head_version_current());
        assert_eq!(status.baseline_version, BASELINE_VERSION);
        assert!(status.table_present);
        assert!(!status.legacy);
        assert_eq!(status.rows.len(), 1);
        assert_eq!(status.rows[0].version, BASELINE_VERSION);
    }

    #[tokio::test]
    async fn inspect_schema_on_raw_baseline_store_without_table_reports_table_absent() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_db(&path).await;

        let status = inspect_schema(&path).await.unwrap();
        assert_eq!(status.current_version, BASELINE_VERSION);
        assert!(!status.table_present);
        assert!(status.rows.is_empty());
        assert!(!status.legacy);
    }

    #[tokio::test]
    async fn inspect_schema_on_too_new_store_does_not_error() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_healthy_baseline_store(&path).await;
        let future_version = chain::head_version_current() + 5;
        test_fixtures::stamp_user_version(&path, future_version).await;

        let status = inspect_schema(&path).await.unwrap();
        assert_eq!(status.current_version, future_version);
        assert!(!status.legacy);
    }

    #[tokio::test]
    async fn inspect_schema_on_legacy_store_reports_legacy_true() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::stamp_user_version(&path, 2).await;

        let status = inspect_schema(&path).await.unwrap();
        assert_eq!(status.current_version, 2);
        assert!(status.legacy);
        assert!(!status.table_present);
    }
}
