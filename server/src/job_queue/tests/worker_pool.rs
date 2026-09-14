//! Worker pool (issue #208, `server.job_workers` config key):
//! `JobQueue::with_workers(N)` spawns `N` `run_worker` tasks sharing the
//! queue's one `mpsc::Receiver<QueuedJob>`. `N == 1` must reproduce the
//! historical strictly-serial behavior exactly (nothing observes the
//! second store's job before the first releases); `N > 1` must let jobs
//! for *different* stores run concurrently, while the per-store in-flight
//! guard and per-job cancellation both stay unaffected by worker count.
//!
//! Every test here is deterministic, following `cancellation.rs`'s
//! convention: no test waits on a fixed sleep to prove ordering or overlap
//! — a task body is parked on a `oneshot` gate this test alone controls (a
//! gate it either never sends, to prove something never got a chance to
//! run, or sends only after asserting on a still-parked task's effect), and
//! `wait_for_done`'s bounded poll is only ever a safety-net deadline, never
//! the thing being asserted on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex as AsyncMutex};

use localdb_core::{Error, IndexJobScope, IndexJobState, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::JobQueue;

/// `with_workers(1)` must keep today's strict serialization: a second
/// submission (to a *different* store, so the in-flight guard isn't what's
/// blocking it) must stay `Pending` for as long as the single worker is
/// still busy on the first job — proof that only one worker exists to pick
/// it up, not merely that it hasn't been scheduled yet.
#[tokio::test]
async fn workers_1_preserves_strict_serialization() {
    let queue = JobQueue::with_workers(1);
    assert_eq!(queue.worker_count(), 1);

    // Job 1: parks the single worker until this test releases it.
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let job1 = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |_progress| async move {
                let _ = release_rx.await;
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    // Job 2: a distinct store, so only the worker count (not the in-flight
    // guard) determines whether it can start.
    let ran = Arc::new(AtomicUsize::new(0));
    let ran_for_task = ran.clone();
    let job2 = queue
        .submit(
            "store-2",
            IndexJobScope::Store,
            move |_progress| async move {
                ran_for_task.fetch_add(1, Ordering::SeqCst);
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();
    assert_eq!(
        job2.state,
        IndexJobState::Pending,
        "with a single worker busy on job 1, job 2 must still be queued"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "job 2's task body must not have run yet"
    );

    // Release job 1; only now can the sole worker move on to job 2.
    let _ = release_tx.send(());
    let done1 = wait_for_done(&queue, &job1.id).await;
    assert_eq!(done1.state, IndexJobState::Done);
    let done2 = wait_for_done(&queue, &job2.id).await;
    assert_eq!(done2.state, IndexJobState::Done);
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

/// `with_workers(2)` must let jobs for two *different* stores overlap: a
/// second worker picks up store-B's job while store-A's job is still
/// parked on a gate this test never releases until after B has already
/// reached a terminal state. With only one worker, B could never complete
/// while A's gate stays closed — so B reaching `Done` here is a
/// deterministic proof of real overlap, not a timing coincidence.
#[tokio::test]
async fn workers_2_overlap_across_stores() {
    let queue = JobQueue::with_workers(2);
    assert_eq!(queue.worker_count(), 2);

    let (a_started_tx, a_started_rx) = oneshot::channel::<()>();
    let a_started_tx = Arc::new(AsyncMutex::new(Some(a_started_tx)));
    let (a_release_tx, a_release_rx) = oneshot::channel::<()>();

    let job_a = queue
        .submit(
            "store-a",
            IndexJobScope::Store,
            move |_progress| async move {
                if let Some(tx) = a_started_tx.lock().await.take() {
                    let _ = tx.send(());
                }
                let _ = a_release_rx.await;
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    // Make sure A is genuinely running (occupying one worker), not merely
    // sitting `Pending`, before submitting B — otherwise a working
    // single-worker queue could also "complete B first" simply because A
    // hadn't started yet.
    a_started_rx.await.unwrap();
    assert_eq!(
        queue.get_job(&job_a.id).await.unwrap().state,
        IndexJobState::Running
    );

    let job_b = queue
        .submit("store-b", IndexJobScope::Store, ok_job)
        .await
        .unwrap();

    // B must reach Done while A's gate is still closed — only possible if
    // a second worker is actually processing B concurrently with A.
    let done_b = wait_for_done(&queue, &job_b.id).await;
    assert_eq!(done_b.state, IndexJobState::Done);
    assert_eq!(
        queue.get_job(&job_a.id).await.unwrap().state,
        IndexJobState::Running,
        "A must still be parked, unaffected by B's completion"
    );

    // Now release A and drive it to completion too.
    let _ = a_release_tx.send(());
    let done_a = wait_for_done(&queue, &job_a.id).await;
    assert_eq!(done_a.state, IndexJobState::Done);
}

/// The per-store in-flight guard is independent of worker count: even with
/// two workers available, a second submission for a store that already has
/// a job in flight must still be rejected with `IndexInProgress`.
#[tokio::test]
async fn same_store_duplicate_still_rejected_with_workers_2() {
    let queue = JobQueue::with_workers(2);

    let (release_tx, release_rx) = oneshot::channel::<()>();
    let job1 = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |_progress| async move {
                let _ = release_rx.await;
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    let second = queue.submit("store-1", IndexJobScope::Store, ok_job).await;
    assert!(
        matches!(second, Err(Error::IndexInProgress)),
        "expected IndexInProgress even with a free second worker, got: {:?}",
        second
    );

    let _ = release_tx.send(());
    let done1 = wait_for_done(&queue, &job1.id).await;
    assert_eq!(done1.state, IndexJobState::Done);
}

/// Cancellation stays per-job/per-worker: with two workers each running a
/// different store's job, cancelling one must not affect the other — the
/// cancelled job terminates `job_cancelled` while the untouched one still
/// completes normally once its own gate is released.
#[tokio::test]
async fn cancellation_works_per_worker() {
    let queue = JobQueue::with_workers(2);

    let (a_started_tx, a_started_rx) = oneshot::channel::<()>();
    let a_started_tx = Arc::new(AsyncMutex::new(Some(a_started_tx)));
    // Never sent — job A can only leave this await via cancellation.
    let (_a_gate_tx, a_gate_rx) = oneshot::channel::<()>();
    let a_resumed = Arc::new(AtomicUsize::new(0));
    let a_resumed_for_task = a_resumed.clone();

    let job_a = queue
        .submit(
            "store-a",
            IndexJobScope::Store,
            move |_progress| async move {
                if let Some(tx) = a_started_tx.lock().await.take() {
                    let _ = tx.send(());
                }
                let _ = a_gate_rx.await;
                a_resumed_for_task.fetch_add(1, Ordering::SeqCst);
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    let (b_started_tx, b_started_rx) = oneshot::channel::<()>();
    let b_started_tx = Arc::new(AsyncMutex::new(Some(b_started_tx)));
    let (b_release_tx, b_release_rx) = oneshot::channel::<()>();

    let job_b = queue
        .submit(
            "store-b",
            IndexJobScope::Store,
            move |_progress| async move {
                if let Some(tx) = b_started_tx.lock().await.take() {
                    let _ = tx.send(());
                }
                let _ = b_release_rx.await;
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    // Both must actually be running (one per worker) before cancelling
    // either — otherwise this wouldn't be proving per-worker independence.
    a_started_rx.await.unwrap();
    b_started_rx.await.unwrap();
    assert_eq!(
        queue.get_job(&job_b.id).await.unwrap().state,
        IndexJobState::Running
    );

    queue.cancel(&job_a.id).await.unwrap();
    let done_a = wait_for_done(&queue, &job_a.id).await;
    assert_eq!(done_a.state, IndexJobState::Failed);
    assert_eq!(done_a.error_code.as_deref(), Some("job_cancelled"));
    assert_eq!(
        a_resumed.load(Ordering::SeqCst),
        0,
        "A's task body must never resume past the cancelled await point"
    );

    // B's worker must be completely unaffected by A's cancellation.
    assert_eq!(
        queue.get_job(&job_b.id).await.unwrap().state,
        IndexJobState::Running,
        "B must still be running after A was cancelled"
    );
    let _ = b_release_tx.send(());
    let done_b = wait_for_done(&queue, &job_b.id).await;
    assert_eq!(done_b.state, IndexJobState::Done);
}
