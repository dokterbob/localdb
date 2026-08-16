//! Progress-event subscription (#83): `subscribe` finds a channel while a
//! job is live and loses it once the job is terminal, and a subscriber
//! actually observes events sent via the task's `ProgressSink`.

use localdb_core::{IndexJobScope, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::JobQueue;

/// `subscribe` must find a channel for a freshly-submitted job (the
/// channel is created in `submit`, before the worker ever picks the job
/// up) and must find no channel once the job is terminal — `run_worker`
/// tears it down right after the registry update.
#[tokio::test]
async fn subscribe_finds_channel_before_terminal_and_none_after() {
    let queue = JobQueue::new();
    let job = queue
        .submit("store-1", IndexJobScope::Store, ok_job)
        .await
        .unwrap();

    assert!(
        queue.subscribe(&job.id).await.is_some(),
        "expected a channel for a freshly-submitted job"
    );

    wait_for_done(&queue, &job.id).await;

    assert!(
        queue.subscribe(&job.id).await.is_none(),
        "expected no channel for a job that has already reached a terminal state"
    );
}

/// A subscriber must actually observe events the task's `ProgressSink`
/// sends — the whole point of threading a per-job sink through `submit`
/// (issue #83). Uses the same block-until-released pattern as the
/// in-flight guard tests, so the subscription is guaranteed to be
/// in place before the task sends its event.
#[tokio::test]
async fn subscriber_observes_events_sent_via_the_tasks_progress_sink() {
    let queue = JobQueue::new();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

    let job = queue
        .submit(
            "store-1",
            IndexJobScope::Store,
            move |progress| async move {
                let _ = release_rx.await;
                progress(localdb_core::ProgressEvent::Discovered { total: 3 });
                Ok(IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    let mut rx = queue.subscribe(&job.id).await.unwrap();
    release_tx.send(()).unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for progress event")
        .expect("channel closed before delivering the event");
    match event {
        localdb_core::ProgressEvent::Discovered { total } => assert_eq!(total, 3),
        other => panic!("unexpected event: {other:?}"),
    }
}
