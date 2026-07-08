//! The `schema_migrations` tracking table: DDL + CRUD.
//!
//! This table is the source of truth for which migrations have been applied
//! to a given database. `PRAGMA user_version` is kept in lockstep as a cheap
//! marker (managed by the runner, added in a later step) but this table is
//! what downgrade/verification logic actually reads: unlike `user_version`,
//! a row can carry the rendered-and-persisted down-SQL (or the reason a
//! migration can't be undone) plus a checksum of what was actually applied.

use libsql::{params, Connection};

use super::chain::BASELINE_VERSION;
use super::checksum::baseline_checksum;

/// Idempotent CREATE TABLE for the migration tracking table.
///
/// The CHECK constraint enforces that every row has exactly one of
/// `down_sql` (a JSON array of SQL strings, see [`MigrationRow::down_sql`])
/// or `down_unsupported_reason` set — never both, never neither.
pub async fn ensure_table(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL,
            down_sql TEXT,
            down_unsupported_reason TEXT,
            checksum TEXT NOT NULL,
            CHECK ((down_sql IS NOT NULL AND down_unsupported_reason IS NULL)
                OR (down_sql IS NULL AND down_unsupported_reason IS NOT NULL))
        )",
        (),
    )
    .await?;
    Ok(())
}

/// Insert the frozen baseline row (`version = BASELINE_VERSION`) if it is not
/// already present.
///
/// The baseline predates the migration framework entirely, so it has no
/// down-SQL — only a fixed, human-readable reason — and its checksum comes
/// from [`baseline_checksum`] rather than from rendering any `Migration`.
pub async fn ensure_baseline_row(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations
             (version, name, applied_at, down_sql, down_unsupported_reason, checksum)
         VALUES (?, 'baseline', ?, NULL, ?, ?)",
        params![
            BASELINE_VERSION,
            localdb_core::ingestion::now_rfc3339(),
            "baseline schema predates the migration framework; cannot downgrade below v4",
            baseline_checksum(),
        ],
    )
    .await?;
    Ok(())
}

/// The highest applied migration version, or `None` if the table is empty.
pub async fn max_version(conn: &Connection) -> Result<Option<i64>, libsql::Error> {
    let mut rows = conn
        .query("SELECT MAX(version) FROM schema_migrations", ())
        .await?;
    match rows.next().await? {
        Some(row) => row.get::<Option<i64>>(0),
        None => Ok(None),
    }
}

/// One row of the `schema_migrations` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRow {
    pub version: i64,
    pub name: String,
    pub applied_at: String,
    /// The rendered "down" SQL statements, in application order, or `None`
    /// if this migration is not reversible (see `down_unsupported_reason`).
    ///
    /// Stored as a JSON array rather than a `;`-joined string: statements
    /// (e.g. trigger bodies) may themselves contain semicolons, so naive
    /// joining/splitting would be lossy.
    pub down_sql: Option<Vec<String>>,
    pub down_unsupported_reason: Option<String>,
    pub checksum: String,
}

/// All rows with `version > target`, ordered by version descending (i.e. the
/// order a downgrade would walk them in).
pub async fn list_rows_desc_above(
    conn: &Connection,
    target: i64,
) -> Result<Vec<MigrationRow>, libsql::Error> {
    let mut rows = conn
        .query(
            "SELECT version, name, applied_at, down_sql, down_unsupported_reason, checksum
             FROM schema_migrations
             WHERE version > ?
             ORDER BY version DESC",
            params![target],
        )
        .await?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let version: i64 = row.get(0)?;
        let name: String = row.get(1)?;
        let applied_at: String = row.get(2)?;
        let down_sql_json: Option<String> = row.get(3)?;
        let down_unsupported_reason: Option<String> = row.get(4)?;
        let checksum: String = row.get(5)?;

        let down_sql = down_sql_json
            .map(|json| {
                serde_json::from_str::<Vec<String>>(&json).map_err(|e| {
                    libsql::Error::SqliteFailure(
                        0,
                        format!(
                            "schema_migrations.down_sql for version {version} is not valid \
                             JSON: {e}"
                        ),
                    )
                })
            })
            .transpose()?;

        out.push(MigrationRow {
            version,
            name,
            applied_at,
            down_sql,
            down_unsupported_reason,
            checksum,
        });
    }
    Ok(out)
}

/// Insert one row. Callers are responsible for exactly one of `down_sql` /
/// `down_unsupported_reason` being `Some` — the table's CHECK constraint
/// rejects anything else.
pub async fn insert_row(conn: &Connection, row: &MigrationRow) -> Result<(), libsql::Error> {
    let down_sql_json = row
        .down_sql
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| {
            libsql::Error::SqliteFailure(
                0,
                format!(
                    "serializing down_sql for migration '{name}' (version {version}): {e}",
                    name = row.name,
                    version = row.version,
                ),
            )
        })?;

    conn.execute(
        "INSERT INTO schema_migrations
             (version, name, applied_at, down_sql, down_unsupported_reason, checksum)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            row.version,
            row.name.as_str(),
            row.applied_at.as_str(),
            down_sql_json,
            row.down_unsupported_reason.as_deref(),
            row.checksum.as_str(),
        ],
    )
    .await?;
    Ok(())
}

/// Delete the row for `version`. Used by downgrade (a later step).
pub async fn delete_row(conn: &Connection, version: i64) -> Result<(), libsql::Error> {
    conn.execute(
        "DELETE FROM schema_migrations WHERE version = ?",
        params![version],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsql::Builder;
    use tempfile::tempdir;

    async fn open_test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        (dir, conn)
    }

    #[tokio::test]
    async fn ensure_table_is_idempotent() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();
        ensure_table(&conn).await.unwrap();
    }

    #[tokio::test]
    async fn ensure_baseline_row_is_idempotent() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();
        ensure_baseline_row(&conn).await.unwrap();
        ensure_baseline_row(&conn).await.unwrap();

        let mut rows = conn
            .query("SELECT COUNT(*) FROM schema_migrations", ())
            .await
            .unwrap();
        let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(count, 1, "ensure_baseline_row must not duplicate the row");

        let all = list_rows_desc_above(&conn, 0).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].version, BASELINE_VERSION);
        assert_eq!(all[0].name, "baseline");
        assert!(all[0].down_sql.is_none());
        assert!(all[0].down_unsupported_reason.is_some());
        assert_eq!(all[0].checksum, baseline_checksum());
    }

    #[tokio::test]
    async fn check_constraint_rejects_both_down_sql_and_reason() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();

        let result = conn
            .execute(
                "INSERT INTO schema_migrations
                     (version, name, applied_at, down_sql, down_unsupported_reason, checksum)
                 VALUES (5, 'bad', '2024-01-01T00:00:00Z', '[]', 'nope', 'chk')",
                (),
            )
            .await;
        assert!(
            result.is_err(),
            "row with both down_sql and down_unsupported_reason set should violate CHECK"
        );
    }

    #[tokio::test]
    async fn check_constraint_rejects_neither_down_sql_nor_reason() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();

        let result = conn
            .execute(
                "INSERT INTO schema_migrations
                     (version, name, applied_at, down_sql, down_unsupported_reason, checksum)
                 VALUES (5, 'bad', '2024-01-01T00:00:00Z', NULL, NULL, 'chk')",
                (),
            )
            .await;
        assert!(
            result.is_err(),
            "row with neither down_sql nor down_unsupported_reason set should violate CHECK"
        );
    }

    #[tokio::test]
    async fn insert_and_list_round_trips_down_sql_json_with_embedded_semicolons() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();

        let down_stmts = vec![
            "DROP TRIGGER IF EXISTS t_ai".to_string(),
            "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN \
             INSERT INTO log(msg) VALUES ('a;b;c'); END"
                .to_string(),
        ];
        insert_row(
            &conn,
            &MigrationRow {
                version: BASELINE_VERSION + 1,
                name: "add_thing".to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql: Some(down_stmts.clone()),
                down_unsupported_reason: None,
                checksum: "abc123".to_string(),
            },
        )
        .await
        .unwrap();

        let rows = list_rows_desc_above(&conn, BASELINE_VERSION).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].down_sql.as_deref(), Some(down_stmts.as_slice()));
    }

    #[tokio::test]
    async fn list_rows_desc_above_orders_descending_and_filters_by_target() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();

        for (version, name) in [
            (BASELINE_VERSION + 1, "one"),
            (BASELINE_VERSION + 2, "two"),
            (BASELINE_VERSION + 3, "three"),
        ] {
            insert_row(
                &conn,
                &MigrationRow {
                    version,
                    name: name.to_string(),
                    applied_at: "2024-06-01T00:00:00Z".to_string(),
                    down_sql: Some(vec!["SELECT 1".to_string()]),
                    down_unsupported_reason: None,
                    checksum: format!("chk-{name}"),
                },
            )
            .await
            .unwrap();
        }

        let above_baseline_plus_1 = list_rows_desc_above(&conn, BASELINE_VERSION + 1)
            .await
            .unwrap();
        let versions: Vec<i64> = above_baseline_plus_1.iter().map(|r| r.version).collect();
        assert_eq!(
            versions,
            vec![BASELINE_VERSION + 3, BASELINE_VERSION + 2],
            "should be strictly greater than target, ordered descending"
        );
    }

    #[tokio::test]
    async fn max_version_is_none_on_empty_table() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();
        assert_eq!(max_version(&conn).await.unwrap(), None);
    }

    #[tokio::test]
    async fn max_version_reflects_highest_inserted_version() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();
        ensure_baseline_row(&conn).await.unwrap();
        assert_eq!(max_version(&conn).await.unwrap(), Some(BASELINE_VERSION));

        insert_row(
            &conn,
            &MigrationRow {
                version: BASELINE_VERSION + 1,
                name: "one".to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql: Some(vec!["SELECT 1".to_string()]),
                down_unsupported_reason: None,
                checksum: "chk".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            max_version(&conn).await.unwrap(),
            Some(BASELINE_VERSION + 1)
        );
    }

    #[tokio::test]
    async fn delete_row_removes_only_the_targeted_version() {
        let (_dir, conn) = open_test_db().await;
        ensure_table(&conn).await.unwrap();
        ensure_baseline_row(&conn).await.unwrap();
        insert_row(
            &conn,
            &MigrationRow {
                version: BASELINE_VERSION + 1,
                name: "one".to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql: Some(vec!["SELECT 1".to_string()]),
                down_unsupported_reason: None,
                checksum: "chk".to_string(),
            },
        )
        .await
        .unwrap();

        delete_row(&conn, BASELINE_VERSION + 1).await.unwrap();

        assert_eq!(max_version(&conn).await.unwrap(), Some(BASELINE_VERSION));
    }
}
