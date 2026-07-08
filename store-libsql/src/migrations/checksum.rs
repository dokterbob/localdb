//! Drift detection: hashing a [`Migration`]'s rendered SQL (or a `RustStep`'s
//! `checksum_repr`) so a stored `schema_migrations` row can be compared
//! against what the currently-compiled chain would produce for that version.
//!
//! A mismatch means the binary's notion of what migration N does has changed
//! since it was applied — e.g. someone edited a shipped migration's SQL in
//! place instead of adding a new one. That's a bug in how migrations are
//! authored (chain entries must be treated as immutable once released, like
//! the `baseline` module), and refusing to proceed is safer than silently
//! running against a database that doesn't match the compiled chain.

use super::chain::{head_version, BASELINE_VERSION};
use super::table;
use super::{Down, Migration, MigrationContext, Up};

/// Blake3 hex digest of a migration's identity plus its rendered up/down
/// steps.
///
/// Input is `version\0name\0<rendered-up>\0<rendered-down-or-reason>`:
/// - rendered-up: `Up::Sql` statements rendered via `ctx` and joined with
///   `\n`; `Up::Rust` uses the step's `checksum_repr()` verbatim.
/// - rendered-down: `Down::Sql` statements rendered via `ctx` and joined
///   with `\n`; `Down::Unsupported` uses the reason string verbatim.
pub fn migration_checksum(m: &Migration, ctx: &MigrationContext) -> String {
    let rendered_up = match &m.up {
        Up::Sql(render) => render(ctx).join("\n"),
        Up::Rust(step) => step.checksum_repr().to_string(),
    };
    let rendered_down = match &m.down {
        Down::Sql(render) => render(ctx).join("\n"),
        Down::Unsupported(reason) => reason.to_string(),
    };
    let input = format!(
        "{version}\0{name}\0{rendered_up}\0{rendered_down}",
        version = m.version,
        name = m.name,
    );
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

/// Blake3 hex digest for the frozen baseline row (`version = BASELINE_VERSION`).
///
/// The baseline predates the migration framework — there's no `Migration`
/// value to render — so this hashes a fixed, arbitrary-but-frozen marker
/// instead. It must never change: doing so would make every existing
/// database's baseline row fail verification.
pub fn baseline_checksum() -> String {
    let input = format!("{BASELINE_VERSION}\0baseline\0<frozen-v4-baseline>");
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

/// Verify every applicable `schema_migrations` row's stored checksum against
/// what the compiled `chain` (rendered with `ctx`) would produce today.
///
/// - The baseline row (`version == BASELINE_VERSION`), if present, is checked
///   against [`baseline_checksum`].
/// - Rows with `BASELINE_VERSION < version <= head_version(chain)` are
///   checked against the matching chain entry.
/// - Rows with `version > head_version(chain)` are **skipped**: they were
///   written by a newer binary than this one, which has already verified
///   them; this (older) binary can still read their stored down-SQL to
///   downgrade past them without understanding what they do.
///
/// Returns `Error::Internal` with correlation id
/// `libsql_migrations_checksum_mismatch` naming the offending migration on
/// the first mismatch found.
pub async fn verify_checksums(
    conn: &libsql::Connection,
    chain: &[Migration],
    ctx: &MigrationContext,
) -> Result<(), localdb_core::Error> {
    let head = head_version(chain);
    let rows = table::list_rows_desc_above(conn, BASELINE_VERSION - 1)
        .await
        .map_err(|e| localdb_core::Error::Internal {
            message: format!("reading schema_migrations for checksum verification: {e}"),
            correlation_id: "libsql_migrations_checksum_mismatch".to_string(),
        })?;

    for row in rows {
        if row.version == BASELINE_VERSION {
            let expected = baseline_checksum();
            if row.checksum != expected {
                return Err(mismatch_err(
                    "baseline",
                    row.version,
                    &row.checksum,
                    &expected,
                ));
            }
            continue;
        }

        if row.version > head {
            // Newer than this binary's chain; verified by whichever binary
            // wrote it. Nothing to compare against here.
            continue;
        }

        let Some(migration) = chain.iter().find(|m| m.version == row.version) else {
            // A contiguous, validated chain (see chain::validate_chain) has an
            // entry for every version up to `head`, so this shouldn't happen.
            // Treat it the same as a mismatch rather than silently ignoring
            // a database that's out of sync with the chain.
            return Err(localdb_core::Error::Internal {
                message: format!(
                    "schema_migrations has row for version {v} but no matching chain entry \
                     (head_version={head})",
                    v = row.version,
                ),
                correlation_id: "libsql_migrations_checksum_mismatch".to_string(),
            });
        };

        let expected = migration_checksum(migration, ctx);
        if row.checksum != expected {
            return Err(mismatch_err(
                migration.name,
                row.version,
                &row.checksum,
                &expected,
            ));
        }
    }

    Ok(())
}

fn mismatch_err(name: &str, version: i64, stored: &str, expected: &str) -> localdb_core::Error {
    localdb_core::Error::Internal {
        message: format!(
            "checksum mismatch for migration '{name}' (version {version}): stored={stored}, \
             expected={expected}. The compiled migration's SQL has changed since it was \
             applied to this database."
        ),
        correlation_id: "libsql_migrations_checksum_mismatch".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::table::MigrationRow;
    use libsql::Builder;
    use localdb_core::{Error, VectorEncoding};
    use tempfile::tempdir;

    fn ctx() -> MigrationContext {
        MigrationContext {
            embedding_dim: 384,
            encoding: VectorEncoding::Float32,
        }
    }

    fn up_a(_ctx: &MigrationContext) -> Vec<String> {
        vec!["CREATE TABLE a(x)".into()]
    }
    fn up_b(_ctx: &MigrationContext) -> Vec<String> {
        vec!["CREATE TABLE b(x)".into()]
    }
    fn down_a(_ctx: &MigrationContext) -> Vec<String> {
        vec!["DROP TABLE a".into()]
    }
    fn down_b(_ctx: &MigrationContext) -> Vec<String> {
        vec!["DROP TABLE b".into()]
    }

    fn base_migration() -> Migration {
        Migration {
            version: 5,
            name: "add_a",
            summary: "adds table a",
            up: Up::Sql(up_a),
            down: Down::Sql(down_a),
            needs_reindex: false,
        }
    }

    #[test]
    fn checksum_changes_when_version_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.version = 6;
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_name_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.name = "add_a_renamed";
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_up_sql_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.up = Up::Sql(up_b);
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_down_sql_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.down = Down::Sql(down_b);
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_down_becomes_unsupported_with_different_reasons() {
        let c = ctx();
        let mut m1 = base_migration();
        m1.down = Down::Unsupported("reason one");
        let mut m2 = base_migration();
        m2.down = Down::Unsupported("reason two");
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn baseline_checksum_is_deterministic() {
        assert_eq!(baseline_checksum(), baseline_checksum());
    }

    async fn open_test_db() -> (tempfile::TempDir, libsql::Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        (dir, conn)
    }

    #[tokio::test]
    async fn verify_checksums_passes_on_freshly_built_matching_table() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = base_migration();
        table::insert_row(
            &conn,
            &MigrationRow {
                version: migration.version,
                name: migration.name.to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql: Some(vec!["DROP TABLE a".to_string()]),
                down_unsupported_reason: None,
                checksum: migration_checksum(&migration, &c),
            },
        )
        .await
        .unwrap();

        let chain = vec![migration];
        verify_checksums(&conn, &chain, &c).await.unwrap();
    }

    #[tokio::test]
    async fn verify_checksums_fails_when_a_row_checksum_is_corrupted() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = base_migration();
        table::insert_row(
            &conn,
            &MigrationRow {
                version: migration.version,
                name: migration.name.to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql: Some(vec!["DROP TABLE a".to_string()]),
                down_unsupported_reason: None,
                checksum: migration_checksum(&migration, &c),
            },
        )
        .await
        .unwrap();

        conn.execute(
            "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = ?",
            libsql::params![migration.version],
        )
        .await
        .unwrap();

        let chain = vec![migration];
        let err = verify_checksums(&conn, &chain, &c)
            .await
            .expect_err("tampered checksum should fail verification");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_checksum_mismatch");
                assert!(
                    message.contains("add_a"),
                    "message should name migration: {message}"
                );
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_checksums_ignores_rows_newer_than_head_version() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        // No entries in the compiled chain, so head_version == BASELINE_VERSION.
        let chain: Vec<Migration> = Vec::new();
        let c = ctx();

        // A row from a newer binary this one has never heard of, with a
        // checksum that would never match anything we could compute.
        table::insert_row(
            &conn,
            &MigrationRow {
                version: BASELINE_VERSION + 1,
                name: "from_the_future".to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql: Some(vec!["DROP TABLE future_thing".to_string()]),
                down_unsupported_reason: None,
                checksum: "nonsense-checksum".to_string(),
            },
        )
        .await
        .unwrap();

        verify_checksums(&conn, &chain, &c)
            .await
            .expect("rows above head_version should be skipped, not fail verification");
    }
}
