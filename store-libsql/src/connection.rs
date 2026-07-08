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

use crate::migrations::{chain, checksum, runner, table, MigrationContext};
use crate::schema;
use crate::vectors::embedding_column_type;

/// How a database's `PRAGMA user_version` compares to what this binary
/// expects, and therefore what `LibsqlDb::open` should do about it.
///
/// Pulled out as a pure function of `(version, head)` so the five-way
/// dispatch can be unit-tested directly, including the `Pending` branch that
/// today's empty real migration chain makes otherwise unreachable (there is
/// no way to have `BASELINE_VERSION <= version < head` when `head ==
/// BASELINE_VERSION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionDisposition {
    /// `version == 0`: brand-new database file.
    Fresh,
    /// `0 < version < BASELINE_VERSION`: predates the migration framework
    /// entirely (v1-v3).
    Legacy,
    /// `BASELINE_VERSION <= version < head`: at or past baseline, but behind
    /// this build's compiled migration chain.
    Pending,
    /// `version == head`: exactly what this build expects.
    AtHead,
    /// `version > head`: newer than this build understands.
    TooNew,
}

fn classify_version(version: i64, head: i64) -> VersionDisposition {
    if version == 0 {
        VersionDisposition::Fresh
    } else if version < chain::BASELINE_VERSION {
        VersionDisposition::Legacy
    } else if version < head {
        VersionDisposition::Pending
    } else if version == head {
        VersionDisposition::AtHead
    } else {
        VersionDisposition::TooNew
    }
}

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
    /// then `journal_mode=WAL`, then `foreign_keys=ON`), then dispatches on
    /// `PRAGMA user_version` (see `classify_version`):
    ///
    /// - a fresh (`version == 0`) database gets the current schema DDL plus
    ///   migration-bookkeeping seed rows;
    /// - a healthy at-head database gets idempotent bookkeeping backfill,
    ///   checksum verification, and the idempotent schema DDL (a no-op there,
    ///   but what guarantees newly-added indexes etc. exist);
    /// - every other version (pre-baseline legacy, behind-head pending
    ///   migrations, or newer-than-this-build) is refused with an actionable
    ///   `Error::InvalidConfig` and the database is **never mutated** — no
    ///   destructive "drop and rebuild" happens implicitly on open anymore.
    ///
    /// Finally validates that the existing `chunks.embedding` column type
    /// matches the requested `(embedding_dim, encoding)`. Rejecting a
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
        let head = chain::head_version(&chain::migrations());
        let ctx = MigrationContext {
            embedding_dim,
            encoding,
        };

        // `open` NEVER mutates the schema of a version-mismatched store —
        // every disposition other than `Fresh`/`AtHead` refuses with an
        // actionable hint instead of touching the database. See
        // `classify_version` for the branch this dispatches on.
        match classify_version(version, head) {
            VersionDisposition::Fresh => {
                schema::create_schema(&conn, embedding_dim, encoding)
                    .await
                    .map_err(|e| Error::Internal {
                        message: format!("create_schema: {e}"),
                        correlation_id: "libsql_db_schema".to_string(),
                    })?;
                runner::seed_for_fresh_create(&conn, &chain::migrations(), &ctx).await?;
            }
            VersionDisposition::Legacy => {
                return Err(Error::InvalidConfig {
                    message: format!(
                        "database schema version {version} predates the migration baseline \
                         (v{baseline}); run 'localdb db migrate' to erase and rebuild it (all \
                         indexed data is lost, then re-run 'localdb index'), or delete the \
                         database file",
                        baseline = chain::BASELINE_VERSION,
                    ),
                });
            }
            VersionDisposition::Pending => {
                return Err(Error::InvalidConfig {
                    message: format!(
                        "database schema version {version} is behind this build (v{head}); \
                         run 'localdb db migrate' to apply pending migrations"
                    ),
                });
            }
            VersionDisposition::AtHead => {
                table::ensure_table(&conn).await.map_err(map_libsql_err)?;
                table::ensure_baseline_row(&conn)
                    .await
                    .map_err(map_libsql_err)?;
                checksum::verify_checksums(&conn, &chain::migrations(), &ctx).await?;

                schema::create_schema(&conn, embedding_dim, encoding)
                    .await
                    .map_err(|e| Error::Internal {
                        message: format!("create_schema: {e}"),
                        correlation_id: "libsql_db_schema".to_string(),
                    })?;
            }
            VersionDisposition::TooNew => {
                return Err(Error::InvalidConfig {
                    message: format!(
                        "database schema version {version} is newer than this build (v{head}); \
                         run 'localdb db downgrade' with this binary to step it back, or \
                         upgrade localdb"
                    ),
                });
            }
        }

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
    use crate::migrations::baseline;
    use tempfile::tempdir;

    /// Everything about a database file's on-disk schema state that `open`
    /// must leave untouched when it refuses a version-mismatched store:
    /// `sqlite_master`'s DDL rows, `PRAGMA user_version`, and — if the
    /// bookkeeping table happens to exist — its rows. Used to prove several
    /// refusal branches are pure reads.
    #[derive(Debug, PartialEq)]
    struct DbDump {
        master_rows: Vec<(String, String, String)>,
        user_version: i64,
        migration_rows: Vec<table::MigrationRow>,
    }

    async fn dump_db(path: &std::path::Path) -> DbDump {
        let db = libsql::Builder::new_local(path).build().await.unwrap();
        let conn = db.connect().unwrap();

        let mut rows = conn
            .query(
                "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
                (),
            )
            .await
            .unwrap();
        let mut master_rows = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            master_rows.push((
                row.get::<String>(0).unwrap(),
                row.get::<String>(1).unwrap(),
                row.get::<String>(2).unwrap(),
            ));
        }

        let user_version = schema::get_schema_version(&conn).await.unwrap();

        let mut exists = conn
            .query(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
                (),
            )
            .await
            .unwrap();
        let migration_rows = if exists.next().await.unwrap().is_some() {
            table::list_rows_desc_above(&conn, i64::MIN).await.unwrap()
        } else {
            Vec::new()
        };

        DbDump {
            master_rows,
            user_version,
            migration_rows,
        }
    }

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
    async fn reopen_with_legacy_schema_version_is_refused_without_mutation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        // Stamp version 1 (pre-baseline legacy) on a raw libsql DB (bypassing
        // LibsqlDb::open).
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.query("PRAGMA user_version = 1", ()).await.unwrap();
        }

        let before = dump_db(&path).await;
        let result = LibsqlDb::open(&path, 4, VectorEncoding::Float32).await;
        let after = dump_db(&path).await;

        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("db migrate"),
                    "error should point at 'localdb db migrate': {message}"
                );
                assert!(
                    message.contains("predates"),
                    "error should explain the version predates the baseline: {message}"
                );
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!("expected InvalidConfig, but reopen of legacy schema succeeded"),
        }

        assert_eq!(
            before, after,
            "a refused open of a legacy-version store must not mutate it at all"
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
        let head = chain::head_version(&chain::migrations());
        // Stamp a version head + 1 on a raw libsql DB (bypassing LibsqlDb::open).
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            let future_version = head + 1;
            conn.query(&format!("PRAGMA user_version = {future_version}"), ())
                .await
                .unwrap();
        }

        let before = dump_db(&path).await;
        let result = LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32).await;
        let after = dump_db(&path).await;

        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("newer"),
                    "error should mention 'newer': {message}"
                );
                assert!(
                    message.contains("db downgrade"),
                    "error should point at 'localdb db downgrade': {message}"
                );
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!("expected InvalidConfig, but reopen with newer schema succeeded"),
        }

        assert_eq!(
            before, after,
            "a refused open of a too-new store must not mutate it at all"
        );
    }

    // -- classify_version: the pure five-way dispatch helper. `Pending` is
    // unreachable through today's empty real migration chain at the
    // `LibsqlDb::open` level (head always equals BASELINE_VERSION), so it's
    // exercised here directly against a synthetic head instead.
    #[test]
    fn classify_version_covers_all_five_branches() {
        let baseline = chain::BASELINE_VERSION;
        assert_eq!(classify_version(0, baseline), VersionDisposition::Fresh);
        assert_eq!(
            classify_version(1, baseline),
            VersionDisposition::Legacy,
            "1 < BASELINE_VERSION is legacy"
        );
        assert_eq!(
            classify_version(baseline, baseline + 2),
            VersionDisposition::Pending,
            "at baseline but behind a (synthetic) head is pending"
        );
        assert_eq!(
            classify_version(baseline, baseline),
            VersionDisposition::AtHead
        );
        assert_eq!(
            classify_version(baseline + 1, baseline),
            VersionDisposition::TooNew
        );
    }

    // Plan test 12: opening a raw v4 store that predates the migrations
    // framework (no `schema_migrations` table at all) silently backfills the
    // bookkeeping table with just the baseline row — pure bookkeeping, no
    // user-table DDL.
    #[tokio::test]
    async fn silent_backfill_on_healthy_v4_store_without_migrations_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Build a raw v4 store the way a pre-framework binary would have:
        // baseline DDL only, no schema_migrations table.
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
            let ctx = MigrationContext {
                embedding_dim: 4,
                encoding: VectorEncoding::Float32,
            };
            baseline::create_baseline_schema(&conn, &ctx).await.unwrap();
        }

        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let conn = db.conn().await;

        let rows = table::list_rows_desc_above(&conn, i64::MIN).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "backfill on open should add exactly the baseline row: {rows:?}"
        );
        assert_eq!(rows[0].version, chain::BASELINE_VERSION);
        assert_eq!(rows[0].name, "baseline");
    }

    // Plan test 13: a brand-new store created via `LibsqlDb::open` seeds
    // exactly one bookkeeping row (today's real chain is empty, so that row
    // is the baseline) and stamps user_version to head.
    #[tokio::test]
    async fn fresh_open_seeds_exactly_one_baseline_row() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let conn = db.conn().await;

        let user_version = schema::get_schema_version(&conn).await.unwrap();
        assert_eq!(user_version, chain::head_version(&chain::migrations()));
        assert_eq!(
            user_version, 4,
            "today's empty real chain leaves head == baseline == 4"
        );

        let rows = table::list_rows_desc_above(&conn, i64::MIN).await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "real chain is empty; only the baseline row should exist"
        );
        assert_eq!(rows[0].version, chain::BASELINE_VERSION);
        assert_eq!(rows[0].name, "baseline");
    }

    // Checksum drift: a healthy at-head store whose baseline row's checksum
    // has been tampered with must refuse to open (Error::Internal) without
    // mutating anything further.
    #[tokio::test]
    async fn checksum_drift_on_healthy_store_returns_internal_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Build then close a healthy at-head store.
        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        drop(db);

        // Corrupt the baseline row's checksum directly.
        {
            let raw_db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = raw_db.connect().unwrap();
            conn.execute(
                "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = ?",
                libsql::params![chain::BASELINE_VERSION],
            )
            .await
            .unwrap();
        }

        let before = dump_db(&path).await;
        let result = LibsqlDb::open(&path, 4, VectorEncoding::Float32).await;
        let after = dump_db(&path).await;

        match result {
            Err(Error::Internal { message, .. }) => {
                assert!(
                    message.contains("checksum mismatch"),
                    "error should mention checksum mismatch: {message}"
                );
            }
            Err(other) => panic!("expected Internal, got: {other:?}"),
            Ok(_) => panic!("expected Internal error due to checksum drift, but open succeeded"),
        }

        assert_eq!(
            before, after,
            "a refused open due to checksum drift must not mutate the store"
        );
    }
}
