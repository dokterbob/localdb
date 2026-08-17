//! Job cancellation (issue #218): `JobQueue::cancel` triggers a per-job
//! `tokio_util::sync::CancellationToken` that `run_worker` races the job's
//! future against in one `tokio::select!` — no fifth `IndexJobState`
//! variant; a cancelled job is recorded as `Failed` with
//! `error_code: "job_cancelled"` (`localdb_core::Error::JobCancelled`),
//! exactly like any other typed job failure (see `submit.rs`'s
//! `job_failure_with_a_typed_error_carries_its_stable_code_in_error_code`).
//!
//! Every test here is deterministic: no test waits on a fixed sleep to prove
//! cancellation happened — either the job is driven to a terminal state via
//! `wait_for_done`'s bounded poll (a safety-net deadline, not a timing
//! assertion), or a task body is parked on a gate this test alone controls
//! (a `oneshot` it either never sends, or sends only after asserting on the
//! cancelled outcome).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex as AsyncMutex};

use localdb_core::{Error, IndexJobScope, IndexJobState, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::JobQueue;

// ---------------------------------------------------------------------------
// Unknown / already-terminal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_unknown_job_id_returns_job_not_found() {
    let queue = JobQueue::new();
    let err = queue.cancel("nonexistent-job-id").await.unwrap_err();
    assert!(
        matches!(err, Error::JobNotFound { ref id } if id == "nonexistent-job-id"),
        "expected JobNotFound, got: {err:?}"
    );
}

/// The completion/cancel race, made deterministic: the job is driven all
/// the way to `Done` (via `wait_for_done`) *before* `cancel` is ever
/// called, so there is no ambiguity about which happened first — `cancel`
/// must refuse with `JobAlreadyTerminal` and the job's recorded outcome
/// (`Done`, with its real stats) must be completely untouched.
#[tokio::test]
async fn cancel_after_normal_completion_returns_job_already_terminal_and_leaves_outcome_unchanged()
{
    let queue = JobQueue::new();
    let stats = IndexJobStats {
        docs_indexed: 3,
        ..Default::default()
    };
    let job = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |_progress| async move { Ok(stats) },
        )
        .await
        .unwrap();
    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Done);

    let err = queue.cancel(&job.id).await.unwrap_err();
    assert!(
        matches!(err, Error::JobAlreadyTerminal),
        "expected JobAlreadyTerminal, got: {err:?}"
    );

    // The terminal transition must be recorded exactly once: re-reading the
    // job must show the exact same successful outcome, not something a
    // late-arriving cancel overwrote.
    let after = queue.get_job(&job.id).await.unwrap();
    assert_eq!(after.state, IndexJobState::Done);
    assert_eq!(after.stats.docs_indexed, 3);
}

/// Same race, but for a job that reached `Failed` on its own (a real typed
/// error, nothing to do with cancellation) — `cancel` must still refuse,
/// and must not clobber the *original* failure's `error`/`error_code` with
/// `job_cancelled`.
#[tokio::test]
async fn cancel_after_a_real_failure_returns_job_already_terminal_and_preserves_the_original_error()
{
    let queue = JobQueue::new();
    let job = queue
        .submit("store-1", IndexJobScope::Store, |_progress| async {
            Err(Error::InvalidConfig {
                message: "unconfigured embedder provider".to_string(),
            })
        })
        .await
        .unwrap();
    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Failed);
    assert_eq!(done.error_code.as_deref(), Some("invalid_config"));

    let err = queue.cancel(&job.id).await.unwrap_err();
    assert!(matches!(err, Error::JobAlreadyTerminal));

    let after = queue.get_job(&job.id).await.unwrap();
    assert_eq!(after.state, IndexJobState::Failed);
    assert_eq!(
        after.error_code.as_deref(),
        Some("invalid_config"),
        "a cancel arriving after a real failure must never overwrite it with job_cancelled"
    );
}

// ---------------------------------------------------------------------------
// Pending (queued, never started)
// ---------------------------------------------------------------------------

/// A job still sitting in the queue when cancelled must never run its
/// pipeline at all — not even one poll of its task future, and not even a
/// call to the task-building `FnOnce` itself (issue #218-followups Fix B).
/// Constructed deterministically: the queue has exactly one background
/// worker (`job_queue.rs`'s module doc comment), so a first job parked on a
/// gate this test controls guarantees a second submission (to a *different*
/// store — the in-flight guard is per-store, so this isn't what blocks it)
/// stays `Pending` until the first job is released.
#[tokio::test]
async fn pending_job_cancelled_before_the_worker_starts_it_never_runs_and_is_recorded_failed() {
    let queue = JobQueue::new();

    // Job 1: blocks the single worker until this test releases it.
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

    // Job 2: a distinct store (so the in-flight guard doesn't reject the
    // submission), guaranteed to still be sitting in the channel — the one
    // worker is busy on job 1. `invoked` is set synchronously by the
    // task-building `FnOnce` itself, *before* it returns the async block
    // that increments `ran` — proving `process_job` never even calls
    // `(queued.task)()` for a job cancelled before it starts (Fix B's
    // atomic check-and-transition), not merely that the resulting future's
    // body never ran (which `ran` alone already covered before this fix).
    let ran = Arc::new(AtomicUsize::new(0));
    let invoked = Arc::new(AtomicBool::new(false));
    let ran_for_task = ran.clone();
    let invoked_for_task = invoked.clone();
    let job2 = queue
        .submit("store-2", IndexJobScope::Store, move |_progress| {
            invoked_for_task.store(true, Ordering::SeqCst);
            async move {
                ran_for_task.fetch_add(1, Ordering::SeqCst);
                Ok(IndexJobStats::default())
            }
        })
        .await
        .unwrap();
    assert_eq!(
        job2.state,
        IndexJobState::Pending,
        "job 2 must still be queued behind job 1's still-running task"
    );

    queue.cancel(&job2.id).await.unwrap();

    // Release job 1 so the worker can move on to (not-)running job 2.
    let _ = release_tx.send(());
    wait_for_done(&queue, &job1.id).await;
    let done2 = wait_for_done(&queue, &job2.id).await;

    assert_eq!(done2.state, IndexJobState::Failed);
    assert_eq!(done2.error_code.as_deref(), Some("job_cancelled"));
    assert!(
        !invoked.load(Ordering::SeqCst),
        "a pending job cancelled before the worker reached it must never even build its task future"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "a pending job cancelled before the worker reached it must never run its task body"
    );
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// A running job cancelled mid-task must never resume past the point it was
/// parked at — proven by parking on a `oneshot` gate this test never sends,
/// so the only way the task's counter increment past that `.await` could
/// ever run is if cancellation were merely cooperative (waiting for the
/// task to notice and unwind on its own) rather than a real abort of the
/// future. Also proves the in-flight guard is released only *after* the
/// abort has actually completed: a second submission for the same store
/// succeeds immediately once `cancel` + the terminal poll have both
/// returned.
#[tokio::test]
async fn running_job_cancelled_mid_task_never_resumes_and_releases_the_inflight_guard() {
    let queue = JobQueue::new();
    let resumed = Arc::new(AtomicUsize::new(0));
    let resumed_for_task = resumed.clone();

    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(AsyncMutex::new(Some(started_tx)));
    // Never sent — the task can only leave this await via cancellation.
    let (_gate_tx, gate_rx) = oneshot::channel::<()>();

    let job = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |_progress| async move {
                if let Some(tx) = started_tx.lock().await.take() {
                    let _ = tx.send(());
                }
                let _ = gate_rx.await;
                resumed_for_task.fetch_add(1, Ordering::SeqCst);
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    // Wait for the task to actually be parked (not merely scheduled) before
    // cancelling — otherwise a cancel racing task startup could land before
    // `start_index_job` ran at all, which is the `pending`-job case above,
    // not this one.
    started_rx.await.unwrap();

    queue.cancel(&job.id).await.unwrap();

    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Failed);
    assert_eq!(done.error_code.as_deref(), Some("job_cancelled"));
    assert_eq!(
        done.error.as_deref(),
        Some("job was cancelled"),
        "error text must be the bare Display string (no field to double-prefix)"
    );
    assert_eq!(
        resumed.load(Ordering::SeqCst),
        0,
        "the task body must never resume past the cancelled await point"
    );

    // `run_worker`'s tail (inflight release) only runs once the aborted
    // task's `JoinHandle` has actually been awaited past the abort — by the
    // time `wait_for_done` observed `Failed` above, that must already have
    // happened, so a fresh submission for the same store must succeed right
    // away rather than racing a not-yet-torn-down previous task.
    let resubmit = queue.submit("store-1", IndexJobScope::Store, ok_job).await;
    assert!(
        resubmit.is_ok(),
        "expected the in-flight guard to already be released: {:?}",
        resubmit.err()
    );
}

/// Cancellation must preempt a genuinely never-resolving await point — not
/// merely one that happens to be gated by something this test could
/// release. `std::future::pending::<()>()` never completes on its own by
/// construction, standing in for an unbounded wait deep in the pipeline
/// (e.g. a `backon` retry sleep or a `governor` pacing wait) that the queue
/// deliberately never threads the cancellation token through — this is
/// exactly the case `run_worker`'s outer `tokio::select!` exists for. The
/// only way this test can pass is if cancellation actually aborts the
/// task's future rather than waiting for it to yield control voluntarily.
#[tokio::test]
async fn cancellation_preempts_a_never_resolving_await_point() {
    let queue = JobQueue::new();
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(AsyncMutex::new(Some(started_tx)));

    let job = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |_progress| async move {
                if let Some(tx) = started_tx.lock().await.take() {
                    let _ = tx.send(());
                }
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    started_rx.await.unwrap();
    queue.cancel(&job.id).await.unwrap();

    // `wait_for_done` itself has a bounded deadline (5s) and will panic if
    // this never reaches a terminal state — that panic is the actual
    // failure mode if cancellation were merely cooperative here, since
    // nothing internal to `pending()` can ever wake this task up on its
    // own.
    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Failed);
    assert_eq!(done.error_code.as_deref(), Some("job_cancelled"));
}

// ---------------------------------------------------------------------------
// Publication-before-handle window (issue #218-followups PR #229 review)
// ---------------------------------------------------------------------------

/// The exact scenario the bug report described: a client sees a job via
/// `GET /v1/jobs`/`GET /jobs/{id}` (here, the `IndexJob` `submit` itself
/// returns — the same registry entry) and cancels it immediately. Before
/// `submit` was reordered to install the handle before the registry entry,
/// this could land in a window where the registry showed the job
/// non-terminal but no handle existed yet — `cancel` would silently report
/// success (`Ok`, the ordinary "cancellation requested" case) without ever
/// triggering the token, and the job went on to run normally.
///
/// Deterministic without controlling whether the worker has already
/// dequeued the job or not by the time `cancel` runs (unlike the
/// pending-cancel and running-cancel tests above, which each pin one of
/// those cases via a blocking first job): the task is parked on a `oneshot`
/// this test never sends, so if cancellation is a no-op — the bug this
/// closes — `wait_for_done` below hangs until its own 5s deadline and
/// panics, regardless of whether the race landed on the `Pending` or
/// `Running` side. Either way, the only way this test can pass is if the
/// task body genuinely never resumes past `gate_rx.await` — the same
/// "never resumes" guarantee `running_job_cancelled_mid_task_...` above
/// pins for the already-`Running` case, now also covered without needing
/// to force it.
#[tokio::test]
async fn cancel_immediately_after_submit_always_triggers_never_silently_no_ops() {
    let queue = JobQueue::new();

    // Never sent — the only way out of this await is cancellation.
    let (_gate_tx, gate_rx) = oneshot::channel::<()>();

    let job = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |_progress| async move {
                let _ = gate_rx.await;
                #[allow(unreachable_code)]
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    // Exactly the id a client would have learned from this same `IndexJob`
    // (or, over HTTP, from the `GET /v1/jobs`/`GET /jobs/{id}` response
    // that reads the same registry entry) — cancel it right away, with no
    // synchronization forcing either the `Pending` or `Running` case.
    let result = queue.cancel(&job.id).await;
    assert!(
        result.is_ok(),
        "cancel immediately after submit must always find a handle to trigger, got: {result:?}"
    );

    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Failed);
    assert_eq!(done.error_code.as_deref(), Some("job_cancelled"));
}
