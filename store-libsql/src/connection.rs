//! Single libsql connection over the unified schema.
//!
//! Holds a `Database` + `Connection` behind a `tokio::sync::Mutex`. Every
//!
//! Cross-process serialisation is SQLite's job: WAL admits one writer at a
//! time per file, `busy_timeout=5000` makes contenders wait, and an
//! exhausted busy-timeout maps to the existing `Error::RuntimeStateLocked`
//! (exit 4). There is no advisory file lock — see proposal §3 (Decision 3).

use std::path::Path;

use libsql::{Builder, Connection, Database};
use tokio::sync::{Mutex, MutexGuard};

use localdb_core::{Error, VectorEncoding};

use crate::schema;
use crate::vectors::embedding_column_type;

/// A shared libsql handle to the unified single-file store.
///
/// Cheap to keep behind `Arc`. All writes go through the single mutex-guarded
/// connection.
pub(crate) struct LibsqlDb {
    /// The owning `Database`. Kept alive for the `Connection`'s lifetime.
    #[allow(dead_code)]
    db: Database,
    conn: Mutex<Connection>,
}

impl LibsqlDb {
    /// Open (or create) the unified database at `path`.
    ///
    /// Creates parent directories, sets PRAGMAs (`busy_timeout=5000` first,
    /// then `journal_mode=WAL`, then `foreign_keys=ON`), runs the unified
    /// schema DDL, and validates that the existing `chunks.embedding` column
    /// type matches the requested `(embedding_dim, encoding)`. Rejecting a
    /// mismatched reopen prevents silently corrupting an existing index.
    pub(crate) async fn open(
        path: &Path,
        embedding_dim: usize,
        encoding: VectorEncoding,
    ) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                localdb_core::config::refuse_legacy_layout(parent)?;
                std::fs::create_dir_all(parent).map_err(|e| Error::Internal {
                    message: format!("cannot create data directory '{}': {}", parent.display(), e),
                    correlation_id: "libsql_db_mkdir".to_string(),
                })?;
            }
        }

        let db = Builder::new_local(path)
            .build()
            .await
            .map_err(|e| Error::Internal {
                message: format!("cannot open unified DB: {e}"),
                correlation_id: "libsql_db_open".to_string(),
            })?;

        let conn = db.connect().map_err(|e| Error::Internal {
            message: format!("cannot connect to unified DB: {e}"),
            correlation_id: "libsql_db_connect".to_string(),
        })?;

        // PRAGMA ordering matters. Setting `busy_timeout` first ensures the
        // subsequent `journal_mode=WAL` switch waits on a contended writer
        // instead of failing with `SQLITE_BUSY`.
        conn.query("PRAGMA busy_timeout=5000", ())
            .await
            .map_err(map_libsql_err)?;
        conn.query("PRAGMA journal_mode=WAL", ())
            .await
            .map_err(map_libsql_err)?;
        conn.query("PRAGMA foreign_keys=ON", ())
            .await
            .map_err(map_libsql_err)?;

        let version = schema::get_schema_version(&conn)
            .await
            .map_err(map_libsql_err)?;
        if version != 0 && version > schema::SCHEMA_VERSION {
            // Newer schema than this build understands — refuse to open.
            return Err(Error::InvalidConfig {
                message: format!(
                    "database schema version {version} is newer than this build \
                     (v{expected}); upgrade localdb or delete the database to reinitialize",
                    expected = schema::SCHEMA_VERSION,
                ),
            });
        }
        if version != 0 && version < schema::SCHEMA_VERSION {
            // Old schema detected — drop everything and let create_schema rebuild.
            // This project is pre-release with no data preservation guarantee.
            eprintln!(
                "warning: database schema version mismatch (found v{}, expected v{}): \
                 all indexed data will be erased and the database re-initialised. \
                 Re-run `localdb index` to restore your index.",
                version,
                schema::SCHEMA_VERSION,
            );
            tracing::warn!(
                old_version = version,
                new_version = schema::SCHEMA_VERSION,
                "DB schema version mismatch: dropping all tables and reinitialising"
            );
            schema::drop_all_tables(&conn)
                .await
                .map_err(|e| Error::Internal {
                    message: format!("drop_all_tables during schema upgrade: {e}"),
                    correlation_id: "libsql_db_drop_tables".to_string(),
                })?;
        }

        schema::create_schema(&conn, embedding_dim, encoding)
            .await
            .map_err(|e| Error::Internal {
                message: format!("create_schema: {e}"),
                correlation_id: "libsql_db_schema".to_string(),
            })?;

        validate_embedding_column(&conn, embedding_dim, encoding).await?;

        Ok(Self {
            db,
            conn: Mutex::new(conn),
        })
    }

    /// Acquire the underlying connection mutex.
    ///
    pub(crate) async fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().await
    }
}

async fn validate_embedding_column(
    conn: &Connection,
    embedding_dim: usize,
    encoding: VectorEncoding,
) -> Result<(), Error> {
    let expected = embedding_column_type(embedding_dim, encoding);
    let mut rows = conn
        .query(
            "SELECT type FROM pragma_table_info('chunks') WHERE name = 'embedding'",
            (),
        )
        .await
        .map_err(map_libsql_err)?;

    let row = rows
        .next()
        .await
        .map_err(map_libsql_err)?
        .ok_or_else(|| Error::Internal {
            message: "chunks.embedding column missing after schema creation; database is corrupt"
                .to_string(),
            correlation_id: "libsql_db_missing_embedding_col".to_string(),
        })?;
    let actual: String = row.get(0).map_err(map_libsql_err)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(Error::InvalidConfig {
            message: format!(
                "embedding schema mismatch: expected {expected}, found {actual}. \
                 Re-create the database to change embedding model/encoding."
            ),
        });
    }
    Ok(())
}

/// Map a libsql error to our error taxonomy.
///
/// "database is locked" / `SQLITE_BUSY` → `RuntimeStateLocked` (exit 4),
/// everything else → `Internal` with the libsql message.
pub(crate) fn map_libsql_err(e: libsql::Error) -> Error {
    let msg = format!("{e}");
    if msg.contains("database is locked") || msg.contains("SQLITE_BUSY") {
        return Error::RuntimeStateLocked;
    }
    Error::Internal {
        message: format!("unified DB error: {e}"),
        correlation_id: "libsql_db".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn open_creates_new_db() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");
        assert!(!path.exists());
        let _db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        assert!(path.exists(), "DB file should be created on open");
    }

    #[tokio::test]
    async fn open_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("subdir").join("nested").join("localdb.db");
        let _db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        assert!(
            path.exists(),
            "DB file should be created in new directories"
        );
    }

    #[tokio::test]
    async fn second_open_succeeds_on_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");
        let _db1 = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let _db2 = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn open_rejects_encoding_mismatch_on_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");

        // Open as Float32
        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        drop(db);

        // Reopen as Binary — should fail with InvalidConfig
        let result = LibsqlDb::open(&path, 4, VectorEncoding::Binary).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("mismatch"),
                    "error should mention mismatch: {message}"
                );
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!("expected InvalidConfig, but reopen succeeded"),
        }
    }

    #[tokio::test]
    async fn refuses_to_open_with_legacy_stores_dir() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("stores").join("notes")).unwrap();
        let result =
            LibsqlDb::open(&dir.path().join("localdb.db"), 4, VectorEncoding::Float32).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(message.contains("legacy") || message.contains("stores"));
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!("expected InvalidConfig"),
        }
    }

    #[tokio::test]
    async fn open_rejects_dim_mismatch_on_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");

        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        drop(db);

        match LibsqlDb::open(&path, 8, VectorEncoding::Float32).await {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("mismatch"),
                    "error should mention mismatch: {message}"
                );
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!("expected InvalidConfig, but reopen with different dim succeeded"),
        }
    }

    #[tokio::test]
    async fn foreign_keys_pragma_is_enabled() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");
        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let conn = db.conn().await;
        let mut rows = conn.query("PRAGMA foreign_keys", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let on: i64 = row.get(0).unwrap();
        assert_eq!(on, 1, "PRAGMA foreign_keys should be ON after open");
    }

    #[tokio::test]
    async fn wal_pragma_is_enabled() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");
        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let conn = db.conn().await;
        let mut rows = conn.query("PRAGMA journal_mode", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let mode: String = row.get(0).unwrap();
        assert_eq!(
            mode.to_ascii_lowercase(),
            "wal",
            "journal_mode should be WAL after open"
        );
    }

    #[tokio::test]
    async fn map_libsql_err_lock_strings_become_runtime_state_locked() {
        let busy = libsql::Error::SqliteFailure(5, "database is locked".to_string());
        assert!(matches!(map_libsql_err(busy), Error::RuntimeStateLocked));

        let busy2 = libsql::Error::SqliteFailure(5, "SQLITE_BUSY: writer".to_string());
        assert!(matches!(map_libsql_err(busy2), Error::RuntimeStateLocked));
    }

    #[tokio::test]
    async fn map_libsql_err_other_becomes_internal() {
        let other = libsql::Error::SqliteFailure(1, "no such table: foo".to_string());
        match map_libsql_err(other) {
            Error::Internal { message, .. } => {
                assert!(message.contains("no such table"));
            }
            e => panic!("expected Internal, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn reopen_with_old_schema_version_reinitialises() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        // Stamp version 1 on a raw libsql DB (bypassing LibsqlDb::open).
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.query("PRAGMA user_version = 1", ()).await.unwrap();
        }
        // Opening via LibsqlDb::open should now succeed: it drops and recreates.
        let result = LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32).await;
        assert!(
            result.is_ok(),
            "old schema version should trigger reinitialisation, not an error; got: {:?}",
            result.as_ref().err()
        );
    }

    #[tokio::test]
    async fn fresh_db_and_reopen_both_succeed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32)
            .await
            .unwrap();
        LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reopen_with_newer_schema_version_returns_invalid_config_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        // Stamp a version SCHEMA_VERSION + 1 on a raw libsql DB (bypassing LibsqlDb::open).
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            let future_version = crate::schema::SCHEMA_VERSION + 1;
            conn.query(
                &format!("PRAGMA user_version = {future_version}"),
                (),
            )
            .await
            .unwrap();
        }
        // Opening via LibsqlDb::open must fail with InvalidConfig, NOT drop the data.
        let result = LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("newer"),
                    "error should mention 'newer': {message}"
                );
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!("expected InvalidConfig, but reopen with newer schema succeeded"),
        }
    }
}
