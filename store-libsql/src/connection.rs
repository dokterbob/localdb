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
                // `create_schema` uses `CREATE TABLE IF NOT EXISTS`, so an
                // interrupted earlier fresh-create that already built
                // `chunks` with a different embedding shape (and never
                // stamped user_version, so it's still 0) would otherwise be
                // silently seeded/stamped as if healthy here. Validate BEFORE
                // seeding/stamping so a mismatch is refused untouched instead
                // of stamped-then-rejected on the next open.
                validate_embedding_column(&conn, embedding_dim, encoding).await?;
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
                // Only backfill `schema_migrations` (table + baseline row)
                // when it was absent before this open AND `head ==
                // BASELINE_VERSION` — i.e. this build's compiled chain is
                // itself empty, so a table-absent store reporting
                // `user_version == head` genuinely is the raw pre-framework
                // case (a bare-baseline store that just needs bookkeeping
                // scaffolding). When `head > BASELINE_VERSION`, a
                // table-absent store claiming `user_version == head` is
                // fabricated or corrupt: the only real code paths that reach
                // `head` (`seed_for_fresh_create`/`apply_pending`) always
                // leave the table and its rows behind, so this can't be a
                // legitimate pre-framework store. Backfilling it here would
                // create the table and baseline row only for
                // `verify_checksums` to immediately refuse it anyway (a
                // missing row for v{head}) — mutating a store `open` is
                // about to refuse, violating "open never mutates a store it
                // refuses". So leave it untouched and let `verify_checksums`
                // below refuse it with a missing-row error.
                //
                // If the table already exists but its baseline row is
                // missing, that's corrupt bookkeeping regardless of `head`:
                // fall through to `verify_checksums` unmutated so it refuses
                // with a missing-row error, rather than recreating the row
                // here and letting a tampered/corrupt store pass as healthy
                // (C3).
                let migrations_table_existed = table::table_exists(&conn, "schema_migrations")
                    .await
                    .map_err(map_libsql_err)?;
                if !migrations_table_existed && head == chain::BASELINE_VERSION {
                    table::ensure_table(&conn).await.map_err(map_libsql_err)?;
                    table::ensure_baseline_row(&conn)
                        .await
                        .map_err(map_libsql_err)?;
                }
                checksum::verify_checksums(&conn, &chain::migrations(), &ctx, head).await?;

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

/// Refuse if `chunks.embedding`'s actual column type doesn't match what
/// `(embedding_dim, encoding)` would produce.
///
/// Shared by `LibsqlDb::open` (every ordinary open) and
/// `migrations::migrate::migrate_store` (the maintenance path, which opens
/// its own connection via `maintenance::open_for_maintenance` rather than
/// going through `open` — so it must run this same check itself instead of
/// getting it for free; see that function's call site for why).
pub(crate) async fn validate_embedding_column(
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

/// Deserialize a `resources.metadata_json` column value, warning (rather than
/// erroring) on a genuine parse failure.
///
/// Defensive reads must never error the row: rows written before the
/// tagged-`Metadata` migration (#130) hold untagged, flat Dublin Core JSON
/// and legitimately fail to deserialize as the tagged enum — that's expected
/// and silent by design. The problem (issue C4) is that a *different* kind of
/// failure — invalid JSON, or JSON of some unrelated shape, e.g. from
/// corruption or a bug — was indistinguishable from that benign legacy case;
/// both silently fell back to `T::default()`, discarding whatever real
/// metadata existed with no trace. This keeps the same fallback behavior but
/// logs a `tracing::warn!` naming the resource and the parse error on every
/// failure, so a genuine problem is at least observable.
pub(crate) fn parse_metadata_json_lenient<T>(metadata_json: &str, resource_ref: &str) -> T
where
    T: serde::de::DeserializeOwned + Default,
{
    match serde_json::from_str(metadata_json) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(
                resource = resource_ref,
                error = %e,
                "failed to parse resources.metadata_json; falling back to default metadata \
                 (expected for pre-#130 untagged rows, but also fires on genuine corruption)"
            );
            T::default()
        }
    }
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

    /// A DB stamped at the pre-#128 v4 schema (old `chunks.block_id` column
    /// and `idx_chunks_store_resource` index, `user_version=4`, no
    /// `schema_migrations` table) is exactly the `Pending` disposition this
    /// binary's compiled chain now makes reachable (head is 5, one migration
    /// past baseline) — `LibsqlDb::open` must refuse it with a `db migrate`
    /// hint and leave it byte-for-byte untouched, not silently wipe and
    /// reinitialise it the way the pre-framework binary used to.
    #[tokio::test]
    async fn reopen_with_v4_era_block_id_schema_is_refused_without_mutation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            // Old (v4) chunks table shape: has block_id, old index name.
            conn.execute(
                "CREATE TABLE chunks (
                    rowid         INTEGER PRIMARY KEY,
                    store_id      TEXT NOT NULL,
                    id            TEXT NOT NULL,
                    resource_id   TEXT NOT NULL,
                    block_id      INTEGER NOT NULL,
                    block_seq     INTEGER NOT NULL,
                    seq_in_block  INTEGER NOT NULL DEFAULT 0,
                    block_kind    TEXT,
                    text          TEXT NOT NULL,
                    heading_path  TEXT NOT NULL,
                    embedding     F32_BLOB(4) NOT NULL,
                    location_json TEXT,
                    UNIQUE (store_id, id)
                )",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "CREATE INDEX idx_chunks_store_resource ON chunks(store_id, resource_id)",
                (),
            )
            .await
            .unwrap();
            conn.query("PRAGMA user_version = 4", ()).await.unwrap();
        }

        let before = dump_db(&path).await;
        let result = LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32).await;
        let after = dump_db(&path).await;

        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("db migrate"),
                    "error should point at 'localdb db migrate': {message}"
                );
                assert!(
                    message.contains("behind"),
                    "error should explain the version is behind this build: {message}"
                );
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!(
                "expected InvalidConfig, but reopen of a pending v4-era block_id schema succeeded"
            ),
        }

        assert_eq!(
            before, after,
            "a refused open of a pending store must not mutate it at all — block_id, the old \
             index, and user_version=4 must all still be exactly as they were"
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

    // -- classify_version: the pure five-way dispatch helper, exercised
    // directly against a synthetic head (in addition to the real chain's
    // current head, which `reopen_with_v4_era_block_id_schema_is_refused_
    // without_mutation` above already exercises `Pending` through).
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

    // Plan test 12 (superseded): opening a raw v4 store that predates the
    // migrations framework (no `schema_migrations` table at all) used to be
    // silently backfilled with just the baseline row when the real chain
    // was empty — back then `head == BASELINE_VERSION`, so a bare-baseline
    // store genuinely was `AtHead`. Now that a real chain entry exists, that
    // same store is `Pending` instead (see
    // `reopen_with_v4_era_block_id_schema_is_refused_without_mutation`
    // above), so `AtHead`'s backfill path (`table::ensure_baseline_row`
    // followed by `checksum::verify_checksums`) is only ever reachable with
    // *some* bookkeeping already in place.
    //
    // This test now pins the resulting behavior for a store that is
    // fabricated to claim head's `user_version` without ever having run
    // through the framework (impossible via any real code path once the
    // chain is non-empty, since reaching head always means `apply_pending`
    // or `seed_for_fresh_create` ran and left chain-entry rows behind): it
    // must be refused as corrupt bookkeeping, not silently trusted just
    // because the version number matches.
    #[tokio::test]
    async fn at_head_store_missing_chain_entry_rows_is_refused_not_silently_trusted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let head = chain::head_version(&chain::migrations());

        // Build a store with today's head DDL and `user_version` stamped
        // straight to head, but no `schema_migrations` table at all.
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
            schema::create_schema(&conn, 4, VectorEncoding::Float32)
                .await
                .unwrap();
            conn.query(&format!("PRAGMA user_version = {head}"), ())
                .await
                .unwrap();
        }

        match LibsqlDb::open(&path, 4, VectorEncoding::Float32).await {
            Err(Error::Internal { message, .. }) => {
                assert!(
                    message.contains("missing a row"),
                    "error should explain the bookkeeping is incomplete: {message}"
                );
            }
            Err(other) => panic!("expected Internal, got: {other:?}"),
            Ok(_) => panic!(
                "expected Internal error: an at-head store with no chain-entry bookkeeping \
                 rows must not be silently trusted"
            ),
        }
    }

    // Fix 1 (adversarial review, track 4): the fabricated at-head store above
    // (real chain head > BASELINE_VERSION, no `schema_migrations` table) must
    // be refused WITHOUT `open` having created the table (or its baseline
    // row) first. Before this fix, the `AtHead` branch unconditionally
    // created the table and — because it was absent — backfilled the
    // baseline row, then only afterward let `verify_checksums` refuse for the
    // still-missing v{head} chain-entry row: a store `open` refuses had
    // already been mutated. This pins that the table stays entirely absent
    // and `user_version` is untouched.
    #[tokio::test]
    async fn at_head_store_with_no_migrations_table_is_refused_without_creating_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let head = chain::head_version(&chain::migrations());
        assert!(
            head > chain::BASELINE_VERSION,
            "this test's premise requires a non-empty real chain, so a table-absent \
             at-head store is never legitimately backfillable"
        );

        // Build a store with today's head DDL and `user_version` stamped
        // straight to head, but no `schema_migrations` table at all.
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
            schema::create_schema(&conn, 4, VectorEncoding::Float32)
                .await
                .unwrap();
            conn.query(&format!("PRAGMA user_version = {head}"), ())
                .await
                .unwrap();
        }

        let before = dump_db(&path).await;
        assert!(
            !before
                .master_rows
                .iter()
                .any(|(_, name, _)| name == "schema_migrations"),
            "precondition: schema_migrations must not exist yet"
        );

        let result = LibsqlDb::open(&path, 4, VectorEncoding::Float32).await;
        let after = dump_db(&path).await;

        match result {
            Err(Error::Internal { message, .. }) => {
                assert!(
                    message.contains("missing a row"),
                    "error should explain the bookkeeping is incomplete: {message}"
                );
            }
            Err(other) => panic!("expected Internal, got: {other:?}"),
            Ok(_) => panic!(
                "expected Internal error: an at-head store with no chain-entry bookkeeping \
                 rows must not be silently trusted"
            ),
        }

        assert_eq!(
            before, after,
            "a refused open of a fabricated table-absent at-head store must not mutate it at \
             all"
        );
        assert!(
            !after
                .master_rows
                .iter()
                .any(|(_, name, _)| name == "schema_migrations"),
            "open must not have created schema_migrations while refusing this store: {:?}",
            after.master_rows
        );
        assert_eq!(
            after.user_version, head,
            "user_version must remain exactly as stamped, untouched by the refused open"
        );
    }

    // Plan test 13: a brand-new store created via `LibsqlDb::open` seeds
    // exactly one bookkeeping row per real chain entry plus the baseline row,
    // and stamps user_version to head.
    #[tokio::test]
    async fn fresh_open_seeds_baseline_plus_chain_rows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let conn = db.conn().await;

        let user_version = schema::get_schema_version(&conn).await.unwrap();
        assert_eq!(user_version, chain::head_version(&chain::migrations()));
        assert_eq!(
            user_version,
            chain::BASELINE_VERSION + chain::migrations().len() as i64,
            "head == baseline + the real chain's length"
        );

        let rows = table::list_rows_desc_above(&conn, i64::MIN).await.unwrap();
        assert_eq!(
            rows.len(),
            1 + chain::migrations().len(),
            "baseline row plus one row per real chain entry should exist"
        );
        assert!(
            rows.iter()
                .any(|r| r.version == chain::BASELINE_VERSION && r.name == "baseline"),
            "baseline row missing: {rows:?}"
        );
    }

    // Codex review #152 fix 1: a database that only got as far as
    // `create_schema` (no seeding, no stamp — simulating a crash between
    // `create_schema` finishing and `seed_for_fresh_create` committing) must
    // still open successfully: `user_version` is still 0, so `open`
    // classifies it as `Fresh` and re-runs `create_schema` (idempotent) plus
    // seeding, landing at head with the baseline row present.
    #[tokio::test]
    async fn crash_before_seeding_reclassifies_as_fresh_and_recovers_on_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Simulate the interrupted fresh-create: run create_schema directly,
        // bypassing LibsqlDb::open, and never seed schema_migrations or stamp
        // user_version — exactly what create_schema alone now leaves behind.
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
            schema::create_schema(&conn, 4, VectorEncoding::Float32)
                .await
                .unwrap();
        }

        // Confirm the simulated crash point: schema exists, but user_version
        // is still 0.
        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            assert_eq!(
                schema::get_schema_version(&conn).await.unwrap(),
                0,
                "create_schema alone must not stamp user_version"
            );
        }

        // Reopening via the normal path must succeed — classified as Fresh —
        // and end up healthy at head with the baseline row present.
        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let conn = db.conn().await;

        let v = schema::get_schema_version(&conn).await.unwrap();
        assert_eq!(v, chain::head_version(&chain::migrations()));

        let rows = table::list_rows_desc_above(&conn, i64::MIN).await.unwrap();
        assert!(
            rows.iter()
                .any(|r| r.version == chain::BASELINE_VERSION && r.name == "baseline"),
            "recovered store should have the baseline row: {rows:?}"
        );
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

    // C1: same latent bug as migrate.rs's v0 branch, but in `open`'s `Fresh`
    // disposition — a store that only got as far as `create_schema` (chunks
    // built with dim 4, user_version still 0, simulating an interrupted
    // earlier fresh-create) must be refused when reopened with a mismatched
    // dim, and refused BEFORE `seed_for_fresh_create` stamps user_version to
    // head — not stamped-then-rejected on the next open.
    #[tokio::test]
    async fn open_refuses_and_leaves_store_unstamped_on_fresh_create_recovery_dim_mismatch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        {
            let db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
            schema::create_schema(&conn, 4, VectorEncoding::Float32)
                .await
                .unwrap();
        }

        let before = dump_db(&path).await;
        let result = LibsqlDb::open(&path, 8, VectorEncoding::Float32).await;
        let after = dump_db(&path).await;

        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(
                    message.contains("mismatch"),
                    "error should mention mismatch: {message}"
                );
            }
            Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
            Ok(_) => panic!("expected InvalidConfig, but reopen with mismatched dim succeeded"),
        }

        assert_eq!(
            before, after,
            "a refused fresh-create recovery due to an embedding shape mismatch must not \
             mutate the store — user_version must remain 0 and no schema_migrations rows \
             may be written"
        );
        assert_eq!(after.user_version, 0, "must remain unstamped at v0");
        assert!(
            after.migration_rows.is_empty(),
            "no schema_migrations rows should have been written: {:?}",
            after.migration_rows
        );
    }

    // C3: `AtHead`'s bookkeeping backfill must only apply when
    // `schema_migrations` was ABSENT before this open (the raw
    // pre-framework case) — if the table already exists but its baseline row
    // is missing (corrupt bookkeeping), `open` must refuse via
    // `verify_checksums`'s missing-row error, not silently recreate the row
    // and let the store pass as healthy.
    #[tokio::test]
    async fn at_head_open_refuses_and_does_not_backfill_baseline_row_when_table_present_but_row_missing(
    ) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Build then close a healthy at-head store (schema_migrations table
        // present, baseline + chain rows seeded).
        let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        drop(db);

        // Corrupt bookkeeping: the table exists, but its baseline row is
        // gone (as opposed to `checksum_drift_on_healthy_store_returns_
        // internal_error` above, which tampers the row's checksum instead of
        // deleting it).
        {
            let raw_db = libsql::Builder::new_local(&path).build().await.unwrap();
            let conn = raw_db.connect().unwrap();
            conn.execute(
                "DELETE FROM schema_migrations WHERE version = ?",
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
                    message.contains("missing a row"),
                    "error should explain the bookkeeping is incomplete: {message}"
                );
                assert!(
                    message.contains("baseline"),
                    "error should name the missing baseline row: {message}"
                );
            }
            Err(other) => panic!("expected Internal, got: {other:?}"),
            Ok(_) => panic!(
                "expected Internal error: a store whose schema_migrations table exists but \
                 whose baseline row is missing must not be silently trusted"
            ),
        }

        assert_eq!(
            before, after,
            "open must not backfill the baseline row (or otherwise mutate the store) when \
             schema_migrations already existed but was missing a required row"
        );
        assert!(
            !after
                .migration_rows
                .iter()
                .any(|r| r.version == chain::BASELINE_VERSION),
            "the baseline row must remain missing, not silently recreated: {:?}",
            after.migration_rows
        );
    }
}
