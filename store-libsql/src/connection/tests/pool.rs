//! The writer/reader connection pool (#217): `write_tx()`'s transactional
//! semantics (commit persists, drop-without-commit rolls back, concurrent
//! callers serialise), and `reader()`'s round-robin, read-only pool.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use libsql::Connection;
use tempfile::tempdir;

use localdb_core::VectorEncoding;

use crate::connection::LibsqlDb;

#[tokio::test]
async fn readers_see_writes_committed_via_writer() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();

    {
        let tx = db.write_tx().await.unwrap();
        tx.execute("CREATE TABLE pool_probe (id INTEGER PRIMARY KEY)", ())
            .await
            .unwrap();
        tx.execute("INSERT INTO pool_probe (id) VALUES (1)", ())
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let reader = db.reader();
    let mut rows = reader.query("SELECT id FROM pool_probe", ()).await.unwrap();
    let row = rows
        .next()
        .await
        .unwrap()
        .expect("committed row should be visible to a reader connection");
    let id: i64 = row.get(0).unwrap();
    assert_eq!(id, 1);
}

#[tokio::test]
async fn reader_pool_round_robins_across_connections() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();

    // Each reader connection gets its own TEMP table (SQLite TEMP tables are
    // connection-local, invisible to other connections including other
    // clones of a *different* underlying handle). The row count after an
    // insert is therefore a fingerprint of how many times THIS specific
    // underlying connection has been handed out by `reader()` so far —
    // `Connection::clone` shares the same handle, so re-querying a clone of
    // the same pool member still sees prior inserts.
    //
    // Reader connections are `PRAGMA query_only=ON`, which also blocks TEMP
    // table writes, so this fingerprinting helper toggles it off around the
    // write and restores it afterward — purely a test technique for
    // establishing connection identity, unrelated to the enforcement proven
    // by `reader_query_only_pragma_rejects_writes`.
    async fn hit_count(conn: &Connection) -> i64 {
        conn.execute("PRAGMA query_only=OFF", ()).await.unwrap();
        conn.execute(
            "CREATE TEMP TABLE IF NOT EXISTS pool_hits (id INTEGER PRIMARY KEY AUTOINCREMENT)",
            (),
        )
        .await
        .unwrap();
        conn.execute("INSERT INTO pool_hits DEFAULT VALUES", ())
            .await
            .unwrap();
        let mut rows = conn
            .query("SELECT COUNT(*) FROM pool_hits", ())
            .await
            .unwrap();
        let count = rows.next().await.unwrap().unwrap().get(0).unwrap();
        conn.execute("PRAGMA query_only=ON", ()).await.unwrap();
        count
    }

    // `readers` is private but visible here (this module is a descendant of
    // `connection`), which lets the test assert against the pool's actual
    // size instead of duplicating the clamp(2, 4) policy.
    let pool_len = db.readers.len();
    assert!(
        (2..=4).contains(&pool_len),
        "reader pool should be clamped to 2..=4, got {pool_len}"
    );

    // One full lap around the pool: every call should land on a connection
    // seeing its FIRST hit — a degenerate "always the same connection"
    // implementation would instead show hit counts 1, 2, 3, ....
    for i in 0..pool_len {
        let conn = db.reader();
        assert_eq!(
            hit_count(&conn).await,
            1,
            "call {i} of a full round-robin lap should land on a not-yet-visited connection"
        );
    }

    // The next call wraps back around to the first connection, which should
    // now be seeing its SECOND hit — proving genuine round-robin rather than
    // e.g. always returning a fresh-looking but actually-random connection.
    let wrapped = db.reader();
    assert_eq!(
        hit_count(&wrapped).await,
        2,
        "after one full lap, reader() should cycle back to a previously-seen connection"
    );
}

#[tokio::test]
async fn reader_query_only_pragma_rejects_writes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();

    let reader = db.reader();
    let result = reader
        .execute(
            "CREATE TABLE reader_write_attempt (id INTEGER PRIMARY KEY)",
            (),
        )
        .await;
    assert!(
        result.is_err(),
        "a write through a reader() connection should be rejected by PRAGMA query_only=ON"
    );
}

#[tokio::test]
async fn write_tx_commit_persists_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();

    {
        let tx = db.write_tx().await.unwrap();
        tx.execute("CREATE TABLE commit_probe (id INTEGER PRIMARY KEY)", ())
            .await
            .unwrap();
        tx.execute("INSERT INTO commit_probe (id) VALUES (42)", ())
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let conn = db.writer().await;
    let mut rows = conn.query("SELECT id FROM commit_probe", ()).await.unwrap();
    let row = rows
        .next()
        .await
        .unwrap()
        .expect("committed row should persist");
    let id: i64 = row.get(0).unwrap();
    assert_eq!(id, 42);
}

/// Regression pin: a `WriteTx` that is never committed — because the task
/// holding it was aborted while parked mid-transaction — must roll back via
/// its `Drop` backstop rather than leaving the writer mutex wedged or the
/// row half-committed. Orchestrated entirely via oneshot "ready" signals, no
/// sleeps: the spawned task inserts a row, signals readiness, THEN parks on
/// a oneshot that never fires, so `abort()` deterministically lands while
/// parked (never mid-insert, never mid-commit).
#[tokio::test]
async fn write_tx_drop_without_commit_rolls_back() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = Arc::new(
        LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap(),
    );

    {
        let tx = db.write_tx().await.unwrap();
        tx.execute("CREATE TABLE abort_probe (id INTEGER PRIMARY KEY)", ())
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    // `park_tx` is intentionally never sent on — it's held alive by this
    // test function's scope so the task's `park_rx.await` hangs (rather than
    // immediately resolving to a `RecvError`) until `abort()` cancels it.
    let (park_tx, park_rx) = tokio::sync::oneshot::channel::<()>();

    let task_db = Arc::clone(&db);
    let handle = tokio::spawn(async move {
        let tx = task_db.write_tx().await.unwrap();
        tx.execute("INSERT INTO abort_probe (id) VALUES (999)", ())
            .await
            .unwrap();
        // Row inserted, transaction still uncommitted: signal readiness
        // before parking so the test can wait for exactly this point.
        ready_tx.send(()).unwrap();
        let _ = park_rx.await;
        // Unreachable in this test: the task is aborted while parked above.
    });

    ready_rx
        .await
        .expect("task should signal readiness before parking");
    handle.abort();
    let _ = handle.await; // swallow the JoinError produced by the abort
    drop(park_tx); // no longer needed; makes the "never sent" intent explicit

    // (a) a subsequent write_tx() succeeds without hanging — proves the
    // writer mutex was released and the writer connection isn't left
    // wedged mid-transaction by the aborted task's dropped `WriteTx`.
    let verify_tx = db
        .write_tx()
        .await
        .expect("write_tx() must not hang or error after the prior holder was aborted");
    verify_tx.rollback().await.unwrap();

    // (b) the row inserted by the aborted, never-committed transaction is
    // absent — the `Drop` backstop rollback actually ran.
    let reader = db.reader();
    let mut rows = reader
        .query("SELECT id FROM abort_probe", ())
        .await
        .unwrap();
    assert!(
        rows.next().await.unwrap().is_none(),
        "row inserted by the aborted write_tx should have been rolled back, not persisted"
    );
}

/// The writer mutex genuinely serialises `write_tx()` callers: while task A
/// holds a `WriteTx`, task B's `write_tx()` call must not resolve until A
/// releases it. Proven via an `AtomicUsize` sequence counter bumped by each
/// task immediately after acquiring, not via timing.
#[tokio::test]
async fn writer_serializes_two_concurrent_write_tx_calls() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = Arc::new(
        LibsqlDb::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap(),
    );

    let seq = Arc::new(AtomicUsize::new(0));

    let (a_holding_tx, a_holding_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_a_tx, release_a_rx) = tokio::sync::oneshot::channel::<()>();
    let (a_order_tx, a_order_rx) = tokio::sync::oneshot::channel::<usize>();
    let (b_attempting_tx, b_attempting_rx) = tokio::sync::oneshot::channel::<()>();
    let (b_order_tx, b_order_rx) = tokio::sync::oneshot::channel::<usize>();

    let db_a = Arc::clone(&db);
    let seq_a = Arc::clone(&seq);
    let task_a = tokio::spawn(async move {
        let tx = db_a.write_tx().await.unwrap();
        // A now holds the writer mutex; tell the test so it can spawn B.
        a_holding_tx.send(()).unwrap();
        // Wait for the test's go-ahead before releasing.
        release_a_rx.await.unwrap();
        let order = seq_a.fetch_add(1, Ordering::SeqCst);
        tx.rollback().await.unwrap();
        a_order_tx.send(order).unwrap();
    });

    // Wait until A definitely holds the writer mutex before spawning B, so
    // B's attempt is guaranteed to contend rather than race to go first.
    a_holding_rx.await.unwrap();

    let db_b = Arc::clone(&db);
    let seq_b = Arc::clone(&seq);
    let task_b = tokio::spawn(async move {
        b_attempting_tx.send(()).unwrap();
        let tx = db_b.write_tx().await.unwrap(); // blocks until A releases
        let order = seq_b.fetch_add(1, Ordering::SeqCst);
        tx.rollback().await.unwrap();
        b_order_tx.send(order).unwrap();
    });

    b_attempting_rx
        .await
        .expect("task B should signal that it's about to call write_tx()");
    // Yield cooperatively (no sleep, no wall-clock) so task B's write_tx()
    // call actually gets polled and registers itself as a waiter on the
    // writer mutex before A is released.
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }

    release_a_tx.send(()).unwrap();

    let a_order = a_order_rx.await.unwrap();
    let b_order = b_order_rx.await.unwrap();
    task_a.await.unwrap();
    task_b.await.unwrap();

    assert_eq!(
        a_order, 0,
        "A must acquire and release before B can proceed"
    );
    assert_eq!(
        b_order, 1,
        "B must only acquire the writer mutex after A released it"
    );
}
