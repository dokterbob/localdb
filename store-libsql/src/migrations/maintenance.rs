//! Shared "open an existing store without touching its schema" helper for
//! maintenance commands (`db migrate`, `db downgrade`, `db status`).
//!
//! Unlike `LibsqlDb::open` (`connection.rs`), this never creates a parent
//! directory and never runs the `classify_version` dispatch (fresh-create /
//! refuse-on-mismatch / idempotent DDL) — maintenance commands operate on a
//! store that must already exist, and whatever schema work they do (or
//! refuse to do) from there is each command's own, explicit business.

use std::path::Path;

use libsql::{Builder, Connection, Database, OpenFlags};

use localdb_core::Error;

use crate::connection::map_libsql_err;

/// Open the libsql database at `path` for a maintenance command.
///
/// Sets the same three connection PRAGMAs, in the same order, as
/// `LibsqlDb::open`: `busy_timeout` first so a subsequent contended
/// `journal_mode=WAL` switch waits on the lock instead of failing with
/// `SQLITE_BUSY`, then `journal_mode=WAL`, then `foreign_keys=ON`.
///
/// Does **not** create `path`'s parent directory, and does **not** create
/// `path` itself if it's missing — a maintenance command run against a store
/// that was never initialized has nothing to migrate, downgrade, or inspect,
/// so it errors cleanly (`Error::InvalidConfig`) rather than silently
/// conjuring an empty database into existence.
pub(crate) async fn open_for_maintenance(path: &Path) -> Result<(Database, Connection), Error> {
    if !path.is_file() {
        return Err(Error::InvalidConfig {
            message: format!(
                "no store found at '{}': the database file does not exist. Maintenance \
                 commands (migrate/downgrade/status) operate on an existing store only.",
                path.display()
            ),
        });
    }

    let db = Builder::new_local(path)
        .build()
        .await
        .map_err(|e| Error::Internal {
            message: format!("cannot open store for maintenance: {e}"),
            correlation_id: "libsql_maintenance_open".to_string(),
        })?;

    let conn = db.connect().map_err(|e| Error::Internal {
        message: format!("cannot connect to store for maintenance: {e}"),
        correlation_id: "libsql_maintenance_connect".to_string(),
    })?;

    // PRAGMA ordering matters. Setting `busy_timeout` first ensures the
    // subsequent `journal_mode=WAL` switch waits on a contended writer
    // instead of failing with `SQLITE_BUSY` (mirrors `LibsqlDb::open`).
    conn.query("PRAGMA busy_timeout=5000", ())
        .await
        .map_err(map_libsql_err)?;
    conn.query("PRAGMA journal_mode=WAL", ())
        .await
        .map_err(map_libsql_err)?;
    conn.query("PRAGMA foreign_keys=ON", ())
        .await
        .map_err(map_libsql_err)?;

    Ok((db, conn))
}

/// Open the libsql database at `path` read-only, for `db status`'s
/// `inspect_schema` — the one maintenance path that must be a pure read.
///
/// Unlike [`open_for_maintenance`] (used by `migrate`/`downgrade`, which
/// *do* write and so are allowed to switch to WAL), this never runs `PRAGMA
/// journal_mode=WAL`: that pragma persists a header change to the database
/// file and creates `-wal`/`-shm` sidecar files, which a read-only status
/// query must never do. It still sets `busy_timeout` and `foreign_keys` —
/// both connection-local settings, harmless (and inert, since nothing is
/// written) on a read-only handle.
///
/// Opens with `OpenFlags::SQLITE_OPEN_READ_ONLY` so the connection can't
/// write to the file even if some future change here accidentally tried to.
pub(crate) async fn open_for_readonly_inspection(
    path: &Path,
) -> Result<(Database, Connection), Error> {
    if !path.is_file() {
        return Err(Error::InvalidConfig {
            message: format!(
                "no store found at '{}': the database file does not exist. Maintenance \
                 commands (migrate/downgrade/status) operate on an existing store only.",
                path.display()
            ),
        });
    }

    let db = Builder::new_local(path)
        .flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
        .build()
        .await
        .map_err(|e| Error::Internal {
            message: format!("cannot open store for read-only inspection: {e}"),
            correlation_id: "libsql_maintenance_open".to_string(),
        })?;

    let conn = db.connect().map_err(|e| Error::Internal {
        message: format!("cannot connect to store for read-only inspection: {e}"),
        correlation_id: "libsql_maintenance_connect".to_string(),
    })?;

    // Same busy_timeout as open_for_maintenance; deliberately NOT
    // journal_mode=WAL (see doc comment above).
    conn.query("PRAGMA busy_timeout=5000", ())
        .await
        .map_err(map_libsql_err)?;
    conn.query("PRAGMA foreign_keys=ON", ())
        .await
        .map_err(map_libsql_err)?;

    Ok((db, conn))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn missing_path_errors_without_creating_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.db");
        assert!(!path.exists());

        let result = open_for_maintenance(&path).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("does not exist"),
                    "error should explain the file is missing: {message}"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
        assert!(
            !path.exists(),
            "open_for_maintenance must not create the file"
        );
    }

    #[tokio::test]
    async fn missing_parent_directory_is_not_created() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("subdir").join("localdb.db");
        assert!(!path.parent().unwrap().exists());

        let _ = open_for_maintenance(&path).await;
        assert!(
            !path.parent().unwrap().exists(),
            "open_for_maintenance must not create parent directories"
        );
    }

    #[tokio::test]
    async fn opens_an_existing_store_and_sets_the_expected_pragmas() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            // Create the file up front, bypassing open_for_maintenance.
            let db = Builder::new_local(&path).build().await.unwrap();
            let _conn = db.connect().unwrap();
        }
        assert!(path.exists());

        let (_db, conn) = open_for_maintenance(&path).await.unwrap();

        let mut rows = conn.query("PRAGMA foreign_keys", ()).await.unwrap();
        let on: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(on, 1, "foreign_keys should be ON");

        let mut rows = conn.query("PRAGMA journal_mode", ()).await.unwrap();
        let mode: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(
            mode.to_ascii_lowercase(),
            "wal",
            "journal_mode should be WAL"
        );
    }

    #[tokio::test]
    async fn opens_a_zero_byte_file_as_a_fresh_database() {
        // A 0-byte file that exists is a valid empty SQLite database — the
        // case a `migrate_store` caller relies on for "fresh file at v0".
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.db");
        std::fs::write(&path, []).unwrap();

        let (_db, conn) = open_for_maintenance(&path).await.unwrap();
        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let v: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(v, 0);
    }
}
