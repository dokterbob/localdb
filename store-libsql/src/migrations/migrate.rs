//! `db migrate`: bring an existing store up to date with this binary's
//! compiled migration chain (`chain::migrations()`).
//!
//! Dispatches on the store's current `PRAGMA user_version` (mirroring
//! `connection.rs`'s `classify_version`, but this is the *mutating* side of
//! that dispatch — `LibsqlDb::open` only ever refuses):
//!
//! - `0` (a fresh/empty file the user pointed at): treated like a brand-new
//!   store — `schema::create_schema` plus bookkeeping seed rows.
//! - `0 < version < BASELINE_VERSION` (legacy v1-v3): destructive rebuild —
//!   drop everything and recreate at head — gated behind
//!   `allow_legacy_rebuild` so the CLI's confirmation prompt is what actually
//!   authorizes data loss, not merely running the command.
//! - `version > head`: refused; this binary is older than the store.
//! - otherwise (`BASELINE_VERSION <= version <= head`): the ordinary
//!   incremental path, via [`runner::apply_pending`].

use std::path::Path;

use localdb_core::Error;

use super::chain::{self, BASELINE_VERSION};
use super::checksum;
use super::maintenance::open_for_maintenance;
use super::runner::{self, AppliedStep};
use super::{Migration, MigrationContext};
use crate::connection::map_libsql_err;
use crate::schema;

/// The result of one `migrate_store` run.
#[derive(Debug, Clone)]
pub struct MigrateReport {
    pub from_version: i64,
    pub to_version: i64,
    /// Every migration actually applied via the incremental runner path, in
    /// application order. Empty for the fresh-create and legacy-rebuild
    /// paths (which don't run the chain step-by-step) and for a no-op call
    /// at head.
    pub applied: Vec<AppliedStep>,
    /// `true` if this run performed the destructive legacy (v1-v3) rebuild.
    pub legacy_rebuilt: bool,
    /// `true` if any applied migration is `needs_reindex: true` — the caller
    /// (the CLI) should print the `localdb index` hint.
    pub staleness_marked: bool,
}

/// Bring the store at `path` up to date with this binary's compiled
/// migration chain (`chain::migrations()`).
///
/// `allow_legacy_rebuild` must be `true` for a legacy (pre-baseline v1-v3)
/// store to be rebuilt — the CLI passes `true` only after its own
/// confirm-destructive prompt has been accepted; without it a legacy store
/// is refused (and left completely untouched) rather than silently erased.
pub async fn migrate_store(
    path: &Path,
    ctx: &MigrationContext,
    allow_legacy_rebuild: bool,
) -> Result<MigrateReport, Error> {
    migrate_store_with_chain(path, ctx, allow_legacy_rebuild, &chain::migrations()).await
}

/// Same as [`migrate_store`], but against an explicit `real_chain` instead of
/// the compiled registry. This is the seam the fixture-chain tests use to
/// exercise the incremental-apply path without waiting for real migrations
/// to land — `migrate_store` itself always calls this with
/// `chain::migrations()`.
async fn migrate_store_with_chain(
    path: &Path,
    ctx: &MigrationContext,
    allow_legacy_rebuild: bool,
    real_chain: &[Migration],
) -> Result<MigrateReport, Error> {
    let (_db, conn) = open_for_maintenance(path).await?;

    let current = schema::get_schema_version(&conn)
        .await
        .map_err(map_libsql_err)?;
    let head = chain::head_version(real_chain);

    if current == 0 {
        // A fresh or 0-byte file the user pointed at: defensible to treat
        // exactly like a brand-new store.
        schema::create_schema(&conn, ctx.embedding_dim, ctx.encoding)
            .await
            .map_err(|e| Error::Internal {
                message: format!("create_schema during migrate (fresh store): {e}"),
                correlation_id: "libsql_migrate_fresh_create".to_string(),
            })?;
        runner::seed_for_fresh_create(&conn, real_chain, ctx).await?;
        post_check(&conn, real_chain, ctx).await?;

        return Ok(MigrateReport {
            from_version: 0,
            to_version: head,
            applied: Vec::new(),
            legacy_rebuilt: false,
            staleness_marked: false,
        });
    }

    if current < BASELINE_VERSION {
        if !allow_legacy_rebuild {
            return Err(Error::InvalidConfig {
                message: format!(
                    "database schema version {current} predates the migration baseline \
                     (v{BASELINE_VERSION}); rebuilding it is destructive — all indexed data is \
                     lost and 'localdb index' must re-run afterward — and requires explicit \
                     confirmation before it proceeds; nothing was changed"
                ),
            });
        }

        schema::drop_all_tables(&conn)
            .await
            .map_err(map_libsql_err)?;
        schema::create_schema(&conn, ctx.embedding_dim, ctx.encoding)
            .await
            .map_err(|e| Error::Internal {
                message: format!("create_schema during legacy rebuild: {e}"),
                correlation_id: "libsql_migrate_legacy_rebuild".to_string(),
            })?;
        runner::seed_for_fresh_create(&conn, real_chain, ctx).await?;
        post_check(&conn, real_chain, ctx).await?;

        return Ok(MigrateReport {
            from_version: current,
            to_version: head,
            applied: Vec::new(),
            legacy_rebuilt: true,
            staleness_marked: false,
        });
    }

    if current > head {
        return Err(Error::InvalidConfig {
            message: format!(
                "database schema version {current} is newer than this build (v{head}); \
                 run 'localdb db downgrade' with this binary to step it back, or upgrade localdb"
            ),
        });
    }

    let report = runner::apply_pending(&conn, real_chain, ctx).await?;
    post_check(&conn, real_chain, ctx).await?;

    let staleness_marked = report.applied.iter().any(|step| {
        real_chain
            .iter()
            .find(|m| m.version == step.version)
            .map(|m| m.needs_reindex)
            .unwrap_or(false)
    });

    Ok(MigrateReport {
        from_version: current,
        to_version: head,
        applied: report.applied,
        legacy_rebuilt: false,
        staleness_marked,
    })
}

/// Post-migration integrity check: the chain is contiguous and every
/// applicable stored checksum still matches what the compiled chain would
/// produce today.
async fn post_check(
    conn: &libsql::Connection,
    real_chain: &[Migration],
    ctx: &MigrationContext,
) -> Result<(), Error> {
    chain::validate_chain(real_chain)?;
    checksum::verify_checksums(conn, real_chain, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::{table, test_fixtures};

    #[tokio::test]
    async fn migrate_store_refuses_legacy_rebuild_without_confirmation_and_leaves_db_untouched() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::stamp_user_version(&path, 2).await;
        // stamp_user_version alone leaves an otherwise-empty file; that's
        // fine — the refusal path never inspects the rest of the schema.

        let before = test_fixtures::dump_db(&path).await;
        let result = migrate_store(&path, &test_fixtures::ctx(), false).await;
        let after = test_fixtures::dump_db(&path).await;

        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("destructive"),
                    "error should warn the rebuild is destructive: {message}"
                );
                assert!(
                    message.contains("2"),
                    "error should mention the offending version: {message}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        assert_eq!(
            before, after,
            "a refused legacy rebuild must not mutate the store at all"
        );
    }

    #[tokio::test]
    async fn migrate_store_legacy_rebuild_succeeds_when_allowed() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::stamp_user_version(&path, 2).await;

        let report = migrate_store(&path, &test_fixtures::ctx(), true)
            .await
            .unwrap();
        assert_eq!(report.from_version, 2);
        assert_eq!(report.to_version, chain::head_version_current());
        assert!(report.legacy_rebuilt);
        assert!(report.applied.is_empty());
        assert!(!report.staleness_marked);

        let (_db, conn) = open_for_maintenance(&path).await.unwrap();
        let v = schema::get_schema_version(&conn).await.unwrap();
        assert_eq!(v, chain::head_version_current());

        let rows = table::list_rows_desc_above(&conn, i64::MIN).await.unwrap();
        assert!(
            rows.iter().any(|r| r.version == BASELINE_VERSION),
            "seeded schema_migrations should include the baseline row"
        );
    }

    #[tokio::test]
    async fn migrate_store_with_chain_applies_pending_fixture_migrations() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_db(&path).await;

        let chain_migrations = test_fixtures::reversible_chain();
        let report =
            migrate_store_with_chain(&path, &test_fixtures::ctx(), false, &chain_migrations)
                .await
                .unwrap();

        assert_eq!(report.from_version, BASELINE_VERSION);
        assert_eq!(report.to_version, BASELINE_VERSION + 3);
        assert_eq!(
            report.applied.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![
                BASELINE_VERSION + 1,
                BASELINE_VERSION + 2,
                BASELINE_VERSION + 3
            ]
        );
        assert!(!report.legacy_rebuilt);
        assert!(!report.staleness_marked);
    }

    #[tokio::test]
    async fn migrate_store_with_chain_reports_staleness_when_a_migration_needs_reindex() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_baseline_db(&path).await;

        let chain_migrations = test_fixtures::chain_with_reindex_marker();
        let report =
            migrate_store_with_chain(&path, &test_fixtures::ctx(), false, &chain_migrations)
                .await
                .unwrap();

        assert!(report.staleness_marked);
    }

    #[tokio::test]
    async fn migrate_store_on_fresh_empty_file_creates_schema_from_zero() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::touch_empty_db_file(&path);

        let report = migrate_store(&path, &test_fixtures::ctx(), false)
            .await
            .unwrap();

        assert_eq!(report.from_version, 0);
        assert_eq!(report.to_version, chain::head_version_current());
        assert!(!report.legacy_rebuilt);
        assert!(report.applied.is_empty());
    }

    #[tokio::test]
    async fn migrate_store_is_noop_when_already_at_head() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::touch_empty_db_file(&path);

        let first = migrate_store(&path, &test_fixtures::ctx(), false)
            .await
            .unwrap();
        let head = chain::head_version_current();
        assert_eq!(first.to_version, head);

        let second = migrate_store(&path, &test_fixtures::ctx(), false)
            .await
            .unwrap();
        assert_eq!(second.from_version, head);
        assert_eq!(second.to_version, head);
        assert!(second.applied.is_empty());
        assert!(!second.legacy_rebuilt);
    }

    #[tokio::test]
    async fn migrate_store_on_too_new_store_returns_invalid_config_mentioning_downgrade() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::touch_empty_db_file(&path);
        let head = chain::head_version_current();
        test_fixtures::stamp_user_version(&path, head + 1).await;

        let result = migrate_store(&path, &test_fixtures::ctx(), false).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(message.contains("newer"), "message: {message}");
                assert!(message.contains("db downgrade"), "message: {message}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }
}
