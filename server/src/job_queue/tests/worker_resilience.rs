//! Worker resilience: a panicking task is recorded as a normal job failure
//! rather than crashing the worker loop, and `submit` fails the job (rather
//! than panicking or hanging) when the worker side of the channel is
//! already gone.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use localdb_core::{Error, IndexJobScope, IndexJobState, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::{JobQueue, QueuedJob, EVENT_CHANNEL_CAPACITY};

/// A task future that panics must be recorded as a normal job failure
/// (via `tokio::spawn`'s `JoinError`), not crash the worker loop or the
/// process — the whole point of running each job's future through
/// `tokio::spawn` rather than awaiting it inline.
#[tokio::test]
async fn job_whose_task_panics_is_recorded_as_failed_not_worker_crashing() {
    let queue = JobQueue::new();
    let job = queue
        .submit("store-1", IndexJobScope::Store, |_progress| async {
            panic!("simulated task panic");
            #[allow(unreachable_code)]
            Ok(IndexJobStats::default())
        })
        .await
        .unwrap();

    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Failed);
    assert!(
        done.error
            .as_deref()
            .unwrap_or_default()
            .contains("panicked"),
        "expected the panic to surface in the job's error text, got: {:?}",
        done.error
    );

    // The worker loop itself must have survived: a second, unrelated
    // submission still gets processed normally afterwards.
    let job2 = queue
        .submit("store-2", IndexJobScope::Store, ok_job)
        .await
        .unwrap();
    let done2 = wait_for_done(&queue, &job2.id).await;
    assert_eq!(done2.state, IndexJobState::Done);
}

/// A different, sharper panic seam than the one above (issue #208 review,
/// concurrency-breaker finding): here the `FnOnce` handed to `submit`
/// panics *while being called*, before it ever returns a future — so
/// there's no future yet for `tokio::spawn` to run, and so no `JoinError`
/// for the previous test's protection to catch anything from. This panics
/// synchronously inside `process_job`'s own `(queued.task)()` call, which
/// `process_job` now wraps in `std::panic::catch_unwind` specifically to
/// cover this seam. Without that: the job would be stuck `Running`
/// forever (its handles entry and in-flight guard never released), and —
/// worse, in a multi-worker pool — the worker task itself would die with
/// nothing to respawn it, silently shrinking the pool by one. This test
/// pins all of that cleanup still happening, and that the worker loop
/// itself survives to process a later, unrelated job.
#[tokio::test]
async fn task_fn_that_panics_before_returning_a_future_still_tears_down_cleanly() {
    let queue = JobQueue::new();

    let job = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            |_progress| -> std::future::Ready<Result<IndexJobStats, Error>> {
                panic!("simulated task-build panic")
            },
        )
        .await
        .unwrap();

    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Failed);
    assert_eq!(
        done.error_code.as_deref(),
        Some("internal"),
        "a panic while building the task future must record a typed \
         Error::Internal (code \"internal\"), not a bare untyped string"
    );
    assert!(
        done.error
            .as_deref()
            .unwrap_or_default()
            .contains("simulated task-build panic"),
        "expected the panic message to surface in the job's error text, got: {:?}",
        done.error
    );

    // Handles entry torn down — an SSE subscriber's channel must be
    // closed, exactly as for any other terminal job.
    assert!(
        queue.subscribe(&job.id).await.is_none(),
        "no progress channel should remain after a task-build panic"
    );

    // In-flight guard released — a fresh submission for the same store
    // must succeed immediately, not hit `IndexInProgress`.
    let resubmit = queue
        .submit("store-1", IndexJobScope::Store, ok_job)
        .await
        .expect("expected the in-flight guard to be released after a task-build panic");
    let resubmit_done = wait_for_done(&queue, &resubmit.id).await;
    assert_eq!(resubmit_done.state, IndexJobState::Done);

    // The worker loop itself must have survived (pool not silently
    // shrunk): a second, unrelated submission still gets processed
    // normally afterwards.
    let job2 = queue
        .submit("store-2", IndexJobScope::Store, ok_job)
        .await
        .unwrap();
    let done2 = wait_for_done(&queue, &job2.id).await;
    assert_eq!(done2.state, IndexJobState::Done);
}

/// `submit` must report the job as `Failed` (not panic or hang) when the
/// worker side of the channel is already gone — the "queue full or
/// closed" branch. Constructed directly (this test lives in a descendant
/// module of `job_queue`, so `JobQueue`'s private fields are visible) with
/// a receiver dropped up front and no `run_worker` task spawned, so the
/// very first `send` in `submit` is guaranteed to hit a closed channel
/// rather than one that's merely unpolled.
#[tokio::test]
async fn submit_fails_the_job_when_the_worker_channel_is_already_closed() {
    let (sender, receiver) = mpsc::channel::<QueuedJob>(1);
    drop(receiver);
    let queue = JobQueue {
        sender,
        registry: Arc::new(RwLock::new(HashMap::new())),
        inflight: Arc::new(RwLock::new(HashSet::new())),
        handles: Arc::new(RwLock::new(HashMap::new())),
        event_capacity: EVENT_CHANNEL_CAPACITY,
        workers: 1,
    };

    let job = queue
        .submit("store-1", IndexJobScope::Store, ok_job)
        .await
        .expect("submit itself still returns Ok — the failure is recorded on the job");

    assert_eq!(job.state, IndexJobState::Failed);
    assert_eq!(
        job.error.as_deref(),
        Some("job queue is full or closed"),
        "unexpected error: {:?}",
        job.error
    );
    // The in-flight guard for this store must have been released too —
    // a fresh submission (against a real, working queue) must not see
    // a stale reservation.
    assert!(
        !queue.inflight.read().await.contains("store-1"),
        "the in-flight guard must be released on a send failure"
    );
    // And the just-created progress-event channel must have been torn
    // down, matching a normally-terminal job.
    assert!(
        queue.subscribe(&job.id).await.is_none(),
        "no progress channel should remain for a job that never ran"
    );
}
