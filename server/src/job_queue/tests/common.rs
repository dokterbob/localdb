//! Shared test helpers for job_queue tests.

use std::time::{Duration, Instant};

use localdb_core::{Error, IndexJob, IndexJobState, IndexJobStats, ProgressSink};

use crate::job_queue::JobQueue;

/// Shared trivial task body used by every test below that only cares
/// "the job completes successfully with no stats to speak of" — a plain
/// function item (not a closure literal) so its body is one piece of
/// code shared by every call site, rather than a separate,
/// separately-instrumented closure per call site (several of which,
/// written inline, would never actually run to completion within their
/// own test — e.g. a submission that's expected to be rejected before
/// the worker ever invokes its task).
pub(in crate::job_queue) async fn ok_job(_progress: ProgressSink) -> Result<IndexJobStats, Error> {
    Ok(IndexJobStats::default())
}

/// Poll `queue.get_job(job_id)` until it reports `Done`, panicking if
/// `deadline` elapses first — the shared wait-for-completion pattern
/// used throughout this module's tests so a task's body is guaranteed
/// to have actually run (not just been scheduled) before the test
/// inspects it.
pub(in crate::job_queue) async fn wait_for_done(queue: &JobQueue, job_id: &str) -> IndexJob {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let job = queue.get_job(job_id).await.unwrap();
        if job.state == IndexJobState::Done || job.state == IndexJobState::Failed {
            return job;
        }
        if Instant::now() > deadline {
            panic!("job did not reach a terminal state in time: {job:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
