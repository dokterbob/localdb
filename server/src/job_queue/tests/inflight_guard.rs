//! In-flight guard (#187): a second submission for a store that already has
//! a job queued or running is rejected, distinct stores queue independently,
//! and the guard is released once a job reaches any terminal state.

use localdb_core::{Error, IndexJobScope, IndexJobState, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::JobQueue;

#[tokio::test]
async fn second_submit_for_same_store_is_rejected_while_first_is_inflight() {
    let queue = JobQueue::new();
    // A slow first job that blocks until we let it go, so the second
    // submission is guaranteed to observe it still in flight.
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let release_rx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
    let release_rx_for_task = release_rx.clone();

    queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |_progress| async move {
                if let Some(rx) = release_rx_for_task.lock().await.take() {
                    let _ = rx.await;
                }
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    let second = queue.submit("store-1", IndexJobScope::Store, ok_job).await;
    assert!(
        matches!(second, Err(Error::IndexInProgress)),
        "expected IndexInProgress, got: {:?}",
        second
    );

    // Release the blocked first job and drive it to completion, so its
    // task body (the `move |_progress| async move { ... }` closure
    // above) is guaranteed to actually run under coverage rather than
    // merely being scheduled.
    let _ = release_tx.send(());
    let job_id = queue
        .list_jobs()
        .await
        .into_iter()
        .find(|j| j.store_id == "store-1")
        .expect("the first submission's job must be registered")
        .id;
    let done = wait_for_done(&queue, &job_id).await;
    assert_eq!(done.state, IndexJobState::Done);
}

#[tokio::test]
async fn two_distinct_stores_both_queue_fine() {
    let queue = JobQueue::new();
    let a = queue.submit("store-a", IndexJobScope::Store, ok_job).await;
    let b = queue.submit("store-b", IndexJobScope::Store, ok_job).await;
    assert!(a.is_ok());
    assert!(b.is_ok());
}

#[tokio::test]
async fn guard_is_released_after_job_completes_allowing_resubmission() {
    let queue = JobQueue::new();
    let job = queue
        .submit("store-1", IndexJobScope::Store, ok_job)
        .await
        .unwrap();
    wait_for_done(&queue, &job.id).await;

    // Poll for the guard release too — it happens just after the
    // registry update, so a resubmission may race it by a tick.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resubmit = queue.submit("store-1", IndexJobScope::Store, ok_job).await;
        if resubmit.is_ok() {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("guard was never released after job completion");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn guard_is_released_after_job_fails() {
    let queue = JobQueue::new();
    let job = queue
        .submit("store-1", IndexJobScope::Store, |_progress| async {
            Err(Error::Internal {
                message: "boom".to_string(),
                correlation_id: "test".to_string(),
            })
        })
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("job did not fail in time");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if queue.get_job(&job.id).await.unwrap().state == IndexJobState::Failed {
            break;
        }
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let resubmit = queue.submit("store-1", IndexJobScope::Store, ok_job).await;
        if resubmit.is_ok() {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("guard was never released after job failure");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
