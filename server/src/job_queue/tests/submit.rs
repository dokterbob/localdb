//! `submit`/`get_job`/`list_jobs` basic lifecycle, and `JobQueue::default`.

use localdb_core::{Error, IndexJobScope, IndexJobState, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::JobQueue;

#[tokio::test]
async fn submit_creates_job_in_known_state() {
    let queue = JobQueue::new();
    let job = queue
        .submit("store-1", IndexJobScope::Store, ok_job)
        .await
        .unwrap();
    assert_eq!(job.store_id, "store-1");
    // State can be Pending or Running depending on timing — but it exists
    assert!(
        job.state == IndexJobState::Pending
            || job.state == IndexJobState::Running
            || job.state == IndexJobState::Done,
        "unexpected state: {:?}",
        job.state
    );
    // Drive it to completion too, so `ok_job`'s body is guaranteed to
    // have actually run at least once under coverage.
    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Done);
}

#[tokio::test]
async fn job_completes_successfully() {
    let queue = JobQueue::new();
    let stats = IndexJobStats {
        docs_indexed: 5,
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
    let job_id = job.id.clone();

    // Poll until done (with timeout)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("job did not complete in time");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let current = queue.get_job(&job_id).await.unwrap();
        if current.state == IndexJobState::Done {
            assert_eq!(current.stats.docs_indexed, 5);
            break;
        }
    }
}

#[tokio::test]
async fn job_fails_on_error() {
    let queue = JobQueue::new();
    let job = queue
        .submit("store-1", IndexJobScope::Store, |_progress| async {
            Err(Error::Internal {
                message: "something went wrong".to_string(),
                correlation_id: "test".to_string(),
            })
        })
        .await
        .unwrap();
    let job_id = job.id.clone();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("job did not fail in time");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let current = queue.get_job(&job_id).await.unwrap();
        if current.state == IndexJobState::Failed {
            assert!(current.error.is_some());
            assert_eq!(current.error_code.as_deref(), Some("internal"));
            break;
        }
    }
}

/// Issue #187 review, finding 3: a task failing with a typed
/// `core::Error` must have that error's stable `code()` land in the
/// terminal job's `error_code` — not just its stringified `error`
/// message — so a daemon-attached CLI (`cli::job_attach::finish_job`)
/// can reconstruct the original variant via `Error::from_code` and exit
/// with the same code an embedded pre-flight failure of the same kind
/// would. `GET /v1/jobs/{id}` and the SSE terminal `job` event both
/// serialize this `IndexJob` directly, so this also pins the wire shape.
#[tokio::test]
async fn job_failure_with_a_typed_error_carries_its_stable_code_in_error_code() {
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
    // Exact match, not `.contains`: `done.error` must be the *bare*
    // message, with no "invalid config: " `Display` prefix — the
    // consumer (`cli::job_attach::finish_job`) reconstructs the typed
    // error via `Error::from_code(error_code, error)`, which adds that
    // prefix itself. A prefixed `done.error` here would double it (issue
    // #187 review, finding F4).
    assert_eq!(
        done.error.as_deref(),
        Some("unconfigured embedder provider")
    );

    // The JSON wire shape (what `GET /v1/jobs/{id}` and the SSE terminal
    // `job` event actually send) must carry the same field.
    let json = serde_json::to_value(&done).unwrap();
    assert_eq!(json["error_code"], "invalid_config");
}

#[tokio::test]
async fn get_nonexistent_job_returns_none() {
    let queue = JobQueue::new();
    let result = queue.get_job("nonexistent-id").await;
    assert!(result.is_none());
}

#[tokio::test]
async fn list_jobs_returns_all() {
    let queue = JobQueue::new();
    queue
        .submit("store-1", IndexJobScope::Store, ok_job)
        .await
        .unwrap();
    queue
        .submit("store-2", IndexJobScope::Store, ok_job)
        .await
        .unwrap();

    // Give time to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let jobs = queue.list_jobs().await;
    assert_eq!(jobs.len(), 2);
}

#[tokio::test]
async fn default_constructs_a_working_queue() {
    let queue = JobQueue::default();
    let job = queue
        .submit("store-1", IndexJobScope::Store, ok_job)
        .await
        .unwrap();
    let done = wait_for_done(&queue, &job.id).await;
    assert_eq!(done.state, IndexJobState::Done);
}
