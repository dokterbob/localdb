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
                // BEFORE `verify_checksums`, not after. `ctx` is built from
                // the caller-supplied `(embedding_dim, encoding)`, and since
                // schema v6 a migration's rendered SQL — and therefore its
                // checksum — depends on `ctx.encoding` (see
                // `chain::shrink_vector_index_up`). So opening a store with
                // the wrong encoding makes every checksum computed from that
                // context meaningless, and `verify_checksums` would report it
                // as `Internal` "migration drift" — pointing the user at a
                // corrupt-bookkeeping problem they don't have, and masking the
                // actionable `InvalidConfig` "embedding schema mismatch:
                // expected …, found …" they do. Establishing that the context
                // actually describes this store is a precondition for the
                // checksum check meaning anything.
                validate_embedding_column(&conn, embedding_dim, encoding).await?;

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
mod tests;
