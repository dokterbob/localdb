//! `db vacuum`: reclaim disk space a prior migration or bulk delete freed
//! onto SQLite's own free list, but never returned to the filesystem (issue
//! #177).
//!
//! The v6 `shrink_vector_index` migration (`chain.rs`) rebuilds
//! `chunks_vec_idx` ~9x smaller on binary-encoded stores, but — like any
//! `DROP INDEX`/`CREATE INDEX` or bulk `DELETE` — the pages it frees land on
//! the database's own free list, not back on disk: the file itself does not
//! shrink. `VACUUM` is the only thing that rewrites the whole file to return
//! that space, and it's expensive enough (full file rewrite; needs roughly
//! the current file size again in free disk space; can take minutes on a
//! large store) that it must be an explicit, opt-in step rather than
//! something `db migrate` runs for you.

use std::path::Path;
use std::time::{Duration, Instant};

use localdb_core::Error;

use super::maintenance::open_for_maintenance;
use crate::connection::map_libsql_err;

/// The result of one `vacuum_store` run.
#[derive(Debug, Clone)]
pub struct VacuumReport {
    /// Main database file size, in bytes, before `VACUUM` (measured after a
    /// `TRUNCATE` checkpoint — see [`vacuum_store`]'s doc comment).
    pub size_before: u64,
    /// Main database file size, in bytes, after `VACUUM`.
    pub size_after: u64,
    /// `size_before - size_after`, saturating at 0 (a store with nothing to
    /// reclaim round-trips to the same size; `VACUUM` never grows a file).
    pub bytes_reclaimed: u64,
    pub duration: Duration,
}

/// Rewrite the store at `path` to return free-list pages (left behind by
/// prior deletes, index rebuilds, or migrations — e.g. v6
/// `shrink_vector_index`) to the filesystem.
///
/// # Honesty of the reported sizes (WAL)
///
/// A WAL-mode store's true on-disk footprint is `path` plus its `-wal`
/// sidecar, and until a checkpoint folds the WAL back in, `path`'s own size
/// alone can understate what's actually been committed. To keep
/// `size_before`/`size_after` meaningful — the size of the *database*, not
/// "whatever the main file happened to be mid-checkpoint-cycle" — this runs
/// `PRAGMA wal_checkpoint(TRUNCATE)` immediately before each measurement:
/// once before `VACUUM` (folding any committed-but-not-yet-checkpointed pages
/// into `path` and resetting `-wal` to empty) and once after (in case
/// `VACUUM` itself left anything in the WAL). `TRUNCATE`, not the default
/// `PASSIVE` mode, is required — `PASSIVE` only replays the WAL, it doesn't
/// shrink the `-wal` file back down, which would leave a stale-sized sidecar
/// sitting next to a freshly measured main file. This is safe to do
/// unconditionally here: `open_for_maintenance` always sets
/// `journal_mode=WAL`, and maintenance commands run with the daemon stopped
/// (`cli/src/cmds/db.rs`'s `refuse_if_daemon_running`), so no other
/// connection should be contending for the checkpoint.
///
/// # Space and time
///
/// `VACUUM` builds a complete replacement copy of the database before
/// swapping it in, so it needs roughly `size_before` bytes of *additional*
/// free space on the same filesystem, and its runtime scales with database
/// size — potentially minutes for a large store. It cannot run inside a
/// transaction; this issues it as its own autocommit statement against a
/// freshly opened connection.
pub async fn vacuum_store(path: &Path) -> Result<VacuumReport, Error> {
    let started = Instant::now();
    let (_db, conn) = open_for_maintenance(path).await?;

    checkpoint_truncate(&conn).await?;
    let size_before = file_size(path)?;

    // VACUUM cannot run inside a transaction. `open_for_maintenance` hands
    // back an autocommit connection and this is the only statement issued
    // against it before this function returns, so there's nothing to wrap it
    // in.
    conn.execute("VACUUM", ()).await.map_err(map_libsql_err)?;

    checkpoint_truncate(&conn).await?;
    let size_after = file_size(path)?;

    Ok(VacuumReport {
        size_before,
        size_after,
        bytes_reclaimed: size_before.saturating_sub(size_after),
        duration: started.elapsed(),
    })
}

async fn checkpoint_truncate(conn: &libsql::Connection) -> Result<(), Error> {
    // PRAGMAs may return rows; use query() not execute() (see
    // baseline::set_user_version / downgrade.rs's replay_one for the same
    // note).
    conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ())
        .await
        .map_err(map_libsql_err)?;
    Ok(())
}

fn file_size(path: &Path) -> Result<u64, Error> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| Error::Internal {
            message: format!(
                "cannot stat store file '{}' during vacuum: {e}",
                path.display()
            ),
            correlation_id: "libsql_vacuum_stat".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use libsql::params;

    use super::*;
    use crate::migrations::test_fixtures;

    #[tokio::test]
    async fn vacuum_store_on_missing_file_is_refused() {
        let (dir, _path) = test_fixtures::temp_db_path();
        let missing = dir.path().join("does-not-exist.db");

        let result = vacuum_store(&missing).await;
        match result {
            Err(Error::InvalidConfig { message }) => {
                assert!(message.contains("does not exist"), "message: {message}");
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vacuum_store_shrinks_a_file_with_free_pages_and_preserves_data() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_healthy_baseline_store(&path).await;

        {
            let (_db, conn) = open_for_maintenance(&path).await.unwrap();
            conn.execute(
                "CREATE TABLE scratch (id INTEGER PRIMARY KEY, payload BLOB NOT NULL)",
                (),
            )
            .await
            .unwrap();

            // A control row that survives the churn below, to prove VACUUM
            // doesn't lose data it's supposed to keep.
            conn.execute(
                "INSERT INTO scratch (id, payload) VALUES (1, ?)",
                params![vec![0u8; 64]],
            )
            .await
            .unwrap();

            // Bulk-insert throwaway rows, then delete them all: this is what
            // leaves pages on the free list without shrinking the file,
            // mirroring what a migration's DROP INDEX/CREATE INDEX or a large
            // delete does in a real store.
            let payload = vec![0u8; 4096];
            for i in 0..2_000i64 {
                conn.execute(
                    "INSERT INTO scratch (id, payload) VALUES (?, ?)",
                    params![i + 1000, payload.clone()],
                )
                .await
                .unwrap();
            }
            conn.execute("DELETE FROM scratch WHERE id != 1", ())
                .await
                .unwrap();
        }

        let report = vacuum_store(&path).await.unwrap();

        assert!(
            report.size_after < report.size_before,
            "vacuum should shrink a file with a large free list: before={} after={}",
            report.size_before,
            report.size_after
        );
        assert!(
            report.bytes_reclaimed > 0,
            "bytes_reclaimed should be positive: {report:?}"
        );
        assert_eq!(
            report.bytes_reclaimed,
            report.size_before - report.size_after
        );

        // Data outside the churned rows must survive untouched.
        let (_db, conn) = open_for_maintenance(&path).await.unwrap();
        let mut rows = conn
            .query("SELECT COUNT(*) FROM scratch", ())
            .await
            .unwrap();
        let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(count, 1, "only the control row should remain");
    }

    #[tokio::test]
    async fn vacuum_store_on_an_already_compact_store_reclaims_nothing_and_stays_intact() {
        let (_dir, path) = test_fixtures::temp_db_path();
        test_fixtures::write_healthy_baseline_store(&path).await;

        let report = vacuum_store(&path).await.unwrap();
        assert_eq!(
            report.bytes_reclaimed, 0,
            "a store with no free-list bloat has nothing to reclaim: {report:?}"
        );
        assert_eq!(report.size_before, report.size_after);
    }
}
