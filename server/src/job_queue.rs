//! Async job queue for indexing work.
//!
//! Accepts `IndexJob` submissions, executes them via the ingestion pipeline,
//! and tracks state/stats so HTTP callers can poll `GET /jobs/{id}`.
//!
//! Jobs are queued via a tokio channel and executed sequentially by a
//! background worker task (one worker per queue for simplicity).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

use localdb_core::{
    complete_index_job, create_index_job, fail_index_job, start_index_job, IndexJob, IndexJobScope,
    IndexJobStats,
};

/// Maximum number of pending jobs in the channel.
const QUEUE_CAPACITY: usize = 64;

/// A submitted job together with the closure that performs the work.
type JobTask = Box<dyn FnOnce() -> Result<IndexJobStats, String> + Send + 'static>;

struct QueuedJob {
    id: String,
    task: JobTask,
}

/// Shared job registry: job_id → IndexJob.
pub type JobRegistry = Arc<RwLock<HashMap<String, IndexJob>>>;

/// A handle to the job queue.
///
/// Clone-safe: underlying channel and registry are Arc'd.
#[derive(Clone)]
pub struct JobQueue {
    sender: mpsc::Sender<QueuedJob>,
    registry: JobRegistry,
}

impl JobQueue {
    /// Create a new job queue and start the background worker.
    ///
    /// Returns the queue handle. The worker runs until the sender is dropped.
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<QueuedJob>(QUEUE_CAPACITY);
        let registry: JobRegistry = Arc::new(RwLock::new(HashMap::new()));

        let worker_registry = registry.clone();
        tokio::spawn(async move {
            run_worker(receiver, worker_registry).await;
        });

        Self { sender, registry }
    }

    /// Submit a new indexing job.
    ///
    /// Creates an `IndexJob` in `Pending` state, registers it, and enqueues
    /// the actual work closure.
    ///
    /// Returns the created `IndexJob`.
    pub async fn submit<F>(&self, store_id: &str, scope: IndexJobScope, task: F) -> IndexJob
    where
        F: FnOnce() -> Result<IndexJobStats, String> + Send + 'static,
    {
        let job = create_index_job(store_id, scope);
        let job_id = job.id.clone();

        // Register before enqueuing so callers can poll immediately.
        {
            let mut reg = self.registry.write().await;
            reg.insert(job_id.clone(), job.clone());
        }

        let queued = QueuedJob {
            id: job_id.clone(),
            task: Box::new(task),
        };

        if let Err(e) = self.sender.send(queued).await {
            error!("job queue full or closed: {}", e);
            // Mark as failed in registry
            let mut reg = self.registry.write().await;
            if let Some(j) = reg.get_mut(&job_id) {
                fail_index_job(j, "job queue is full or closed".to_string());
            }
        }

        // Return the current state of the job (it's Pending until the worker picks it up).
        let reg = self.registry.read().await;
        reg.get(&job_id).cloned().unwrap_or(job)
    }

    /// Get a job by ID.
    pub async fn get_job(&self, id: &str) -> Option<IndexJob> {
        let reg = self.registry.read().await;
        reg.get(id).cloned()
    }

    /// List all jobs.
    pub async fn list_jobs(&self) -> Vec<IndexJob> {
        let reg = self.registry.read().await;
        reg.values().cloned().collect()
    }
}

/// Background worker: pulls queued jobs and executes them.
async fn run_worker(mut receiver: mpsc::Receiver<QueuedJob>, registry: JobRegistry) {
    while let Some(queued) = receiver.recv().await {
        let job_id = queued.id.clone();
        info!("starting job {}", job_id);

        // Mark as running
        {
            let mut reg = registry.write().await;
            if let Some(job) = reg.get_mut(&job_id) {
                start_index_job(job);
            }
        }

        // Execute task on a blocking thread (CPU+IO work)
        let result = tokio::task::spawn_blocking(queued.task).await;

        // Update registry
        {
            let mut reg = registry.write().await;
            if let Some(job) = reg.get_mut(&job_id) {
                match result {
                    Ok(Ok(stats)) => {
                        info!("job {} completed: {:?}", job_id, stats);
                        complete_index_job(job, stats);
                    }
                    Ok(Err(e)) => {
                        warn!("job {} failed: {}", job_id, e);
                        fail_index_job(job, e);
                    }
                    Err(join_err) => {
                        error!("job {} panicked: {}", job_id, join_err);
                        fail_index_job(job, format!("task panicked: {}", join_err));
                    }
                }
            }
        }
    }
    info!("job queue worker stopped");
}

impl Default for JobQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::IndexJobState;

    #[tokio::test]
    async fn submit_creates_job_in_known_state() {
        let queue = JobQueue::new();
        let job = queue
            .submit("store-1", IndexJobScope::Store, || {
                Ok(IndexJobStats::default())
            })
            .await;
        assert_eq!(job.store_id, "store-1");
        // State can be Pending or Running depending on timing — but it exists
        assert!(
            job.state == IndexJobState::Pending
                || job.state == IndexJobState::Running
                || job.state == IndexJobState::Done,
            "unexpected state: {:?}",
            job.state
        );
    }

    #[tokio::test]
    async fn job_completes_successfully() {
        let queue = JobQueue::new();
        let stats = IndexJobStats {
            docs_indexed: 5,
            ..Default::default()
        };
        let job = queue
            .submit("store-1", IndexJobScope::Store, move || Ok(stats))
            .await;
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
            .submit("store-1", IndexJobScope::Store, || {
                Err("something went wrong".to_string())
            })
            .await;
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
                break;
            }
        }
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
            .submit("store-1", IndexJobScope::Store, || {
                Ok(IndexJobStats::default())
            })
            .await;
        queue
            .submit("store-2", IndexJobScope::Store, || {
                Ok(IndexJobStats::default())
            })
            .await;

        // Give time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let jobs = queue.list_jobs().await;
        assert_eq!(jobs.len(), 2);
    }
}
