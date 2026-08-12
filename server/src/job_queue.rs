//! Async job queue for indexing work.
//!
//! Accepts `IndexJob` submissions, executes them via the ingestion pipeline,
//! and tracks state/stats so HTTP callers can poll `GET /jobs/{id}`.
//!
//! Jobs are queued via a tokio channel and executed sequentially by a
//! background worker task (one worker per queue for simplicity). The work
//! itself is an async future (`server::job_exec::run_job` in production) —
//! the worker `tokio::spawn`s it and awaits the `JoinHandle`, rather than
//! `spawn_blocking`: the ingestion pipeline does its own `spawn_blocking` for
//! CPU-bound work internally (specs/01-architecture.md §6), so the queue
//! itself stays on the async runtime.
//!
//! A per-store in-flight guard (`inflight`) rejects a second submission for a
//! store that already has a job queued or running, at submit time, with
//! `Error::IndexInProgress` — before real ingestion, two concurrent jobs
//! against the same store could race on the same `DocumentIndex`/store
//! handle.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{error, info, warn};

use localdb_core::{
    complete_index_job, create_index_job, fail_index_job, fail_index_job_with_error,
    start_index_job, Error, IndexJob, IndexJobScope, IndexJobStats, ProgressEvent, ProgressSink,
};

/// Maximum number of pending jobs in the channel.
const QUEUE_CAPACITY: usize = 64;

/// Capacity of each job's progress-event broadcast channel (issue #83).
///
/// Bounded rather than unbounded: a slow or absent SSE subscriber must never
/// let a fast-producing ingestion run grow memory without limit. A lagging
/// subscriber instead sees `RecvError::Lagged` and skips ahead — progress is
/// documented as lossy-tolerant (unlike the terminal `job` event, which is
/// derived from the registry, never from this channel alone).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// A pinned, boxed future producing a job's final stats (or a typed error) —
/// the async equivalent of the old synchronous `JobTask` closure.
///
/// The error type is `core::Error`, not `String` (issue #187 review, finding
/// 3): stringifying a task's error here — as this used to — discarded the
/// error's stable `code()` before it ever reached `fail_index_job_with_error`,
/// so a daemon-attached job failure always surfaced as an undifferentiated
/// `Error::Internal` (exit 1) even when the underlying failure was e.g.
/// `Error::InvalidConfig` (exit 2 embedded). Carrying the typed `Error`
/// through end to end is what lets `run_worker` classify the failure
/// correctly when it calls `fail_index_job_with_error` below.
type JobFuture = Pin<Box<dyn Future<Output = Result<IndexJobStats, Error>> + Send>>;

/// A submitted job's work, as a `FnOnce` that produces the future when the
/// worker is ready to run it (not before — building the future may itself
/// borrow/move data the caller wants constructed lazily).
type JobTask = Box<dyn FnOnce() -> JobFuture + Send + 'static>;

struct QueuedJob {
    id: String,
    /// The store this job runs against — used to release the in-flight
    /// guard once the worker finishes (successfully or not).
    store_id: String,
    task: JobTask,
}

/// Shared job registry: job_id → IndexJob.
pub type JobRegistry = Arc<RwLock<HashMap<String, IndexJob>>>;

/// Shared set of store ids with a job currently queued or running.
type InFlightSet = Arc<RwLock<HashSet<String>>>;

/// Shared per-job progress-event registry: job_id → broadcast sender.
///
/// An entry exists from `submit` until the job reaches a terminal state, at
/// which point `run_worker` removes it — dropping the queue's own `Sender`
/// clone. Once every clone (the queue's and the task's `ProgressSink`) is
/// dropped, subscribed receivers observe `RecvError::Closed`, which is how
/// `GET /v1/jobs/{id}/events` (issue #83) knows to stop waiting for more
/// progress and fetch the terminal `IndexJob` from the registry instead.
///
/// Removal always happens *after* the registry's own state update in
/// `run_worker` (see there), so a subscriber that observes the channel close
/// is guaranteed to find the job already terminal in the registry — no
/// window where the terminal event could be missed.
type EventRegistry = Arc<RwLock<HashMap<String, broadcast::Sender<ProgressEvent>>>>;

/// A handle to the job queue.
///
/// Clone-safe: underlying channel, registry, in-flight set, and event
/// registry are Arc'd.
#[derive(Clone)]
pub struct JobQueue {
    sender: mpsc::Sender<QueuedJob>,
    registry: JobRegistry,
    inflight: InFlightSet,
    events: EventRegistry,
    /// Capacity of each job's progress-event broadcast channel — normally
    /// `EVENT_CHANNEL_CAPACITY`, shrinkable in tests via
    /// `new_with_event_capacity` (issue #187 review, finding 4d) so a test
    /// can force `broadcast::error::RecvError::Lagged` deterministically
    /// with only a handful of events instead of needing 1024+.
    event_capacity: usize,
}

impl JobQueue {
    /// Create a new job queue and start the background worker.
    ///
    /// Returns the queue handle. The worker runs until the sender is dropped.
    pub fn new() -> Self {
        Self::with_event_capacity(EVENT_CHANNEL_CAPACITY)
    }

    /// Test-only: identical to [`JobQueue::new`], but with a caller-chosen
    /// progress-event broadcast channel capacity instead of the production
    /// `EVENT_CHANNEL_CAPACITY` (1024). Exists so a test exercising `GET
    /// /v1/jobs/{id}/events`'s `RecvError::Lagged` handling (see
    /// `next_job_event` in `handlers/jobs.rs`) can overflow the channel with
    /// a handful of events rather than needing to actually produce 1024+
    /// real progress events. Production behavior (`new()`) is unaffected.
    #[cfg(test)]
    pub(crate) fn new_with_event_capacity(capacity: usize) -> Self {
        Self::with_event_capacity(capacity)
    }

    fn with_event_capacity(event_capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<QueuedJob>(QUEUE_CAPACITY);
        let registry: JobRegistry = Arc::new(RwLock::new(HashMap::new()));
        let inflight: InFlightSet = Arc::new(RwLock::new(HashSet::new()));
        let events: EventRegistry = Arc::new(RwLock::new(HashMap::new()));

        let worker_registry = registry.clone();
        let worker_inflight = inflight.clone();
        let worker_events = events.clone();
        tokio::spawn(async move {
            run_worker(receiver, worker_registry, worker_inflight, worker_events).await;
        });

        Self {
            sender,
            registry,
            inflight,
            events,
            event_capacity,
        }
    }

    /// Submit a new indexing job for `store_id`.
    ///
    /// `task` is called (not awaited) inside this function to obtain the
    /// future; the future itself runs later, on the worker. Creates an
    /// `IndexJob` in `Pending` state, registers it, and enqueues the work.
    ///
    /// `task` receives a [`ProgressSink`] (issue #83) that writes into this
    /// job's broadcast channel — the caller threads it into
    /// `JobExecDeps.progress` so `run_source_ingestion`'s progress callbacks
    /// become observable via `GET /v1/jobs/{id}/events`. The sink is built
    /// here (submit time), not deferred to when the worker picks the job up,
    /// so a subscriber calling `subscribe` immediately after `submit`
    /// returns can never race the channel's creation.
    ///
    /// Returns `Error::IndexInProgress` if `store_id` already has a job
    /// queued or running — checked and reserved atomically at submit time,
    /// before the job is created, so two concurrent submissions for the same
    /// store can never both proceed.
    pub async fn submit<F, Fut>(
        &self,
        store_id: &str,
        scope: IndexJobScope,
        task: F,
    ) -> Result<IndexJob, Error>
    where
        F: FnOnce(ProgressSink) -> Fut + Send + 'static,
        Fut: Future<Output = Result<IndexJobStats, Error>> + Send + 'static,
    {
        {
            let mut inflight = self.inflight.write().await;
            if !inflight.insert(store_id.to_string()) {
                return Err(Error::IndexInProgress);
            }
        }

        let job = create_index_job(store_id, scope);
        let job_id = job.id.clone();

        // Register before enqueuing so callers can poll immediately.
        {
            let mut reg = self.registry.write().await;
            reg.insert(job_id.clone(), job.clone());
        }

        // Create this job's progress-event channel and the sink that feeds
        // it, before enqueuing — so `subscribe(job_id)` works the instant
        // `submit` returns, even before the worker has picked the job up.
        let (tx, _rx) = broadcast::channel::<ProgressEvent>(self.event_capacity);
        {
            let mut events = self.events.write().await;
            events.insert(job_id.clone(), tx.clone());
        }
        let sink: ProgressSink = {
            let tx = tx.clone();
            Arc::new(move |event: ProgressEvent| {
                // No receivers is the common case (nobody is watching
                // `/events`) — `send` returning `Err` there is expected, not
                // an error worth logging.
                let _ = tx.send(event);
            })
        };

        let queued = QueuedJob {
            id: job_id.clone(),
            store_id: store_id.to_string(),
            task: Box::new(move || Box::pin(task(sink))),
        };

        if let Err(e) = self.sender.send(queued).await {
            error!("job queue full or closed: {}", e);
            // The worker will never run this job — release the guard here,
            // it won't run `run_worker`'s release path.
            let mut inflight = self.inflight.write().await;
            inflight.remove(store_id);
            let mut reg = self.registry.write().await;
            if let Some(j) = reg.get_mut(&job_id) {
                fail_index_job(j, "job queue is full or closed".to_string());
            }
            let mut events = self.events.write().await;
            events.remove(&job_id);
        }

        // Return the current state of the job (it's Pending until the worker picks it up).
        let reg = self.registry.read().await;
        Ok(reg.get(&job_id).cloned().unwrap_or(job))
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

    /// Subscribe to a job's live progress events (issue #83).
    ///
    /// Returns `None` once the job has reached a terminal state and its
    /// channel has been torn down — callers should treat that the same as
    /// "no more progress events, go read the terminal `IndexJob` from
    /// `get_job`", not as "unknown job id" (a job that never existed is a
    /// separate case the caller should check via `get_job` first).
    pub async fn subscribe(&self, job_id: &str) -> Option<broadcast::Receiver<ProgressEvent>> {
        let events = self.events.read().await;
        events.get(job_id).map(|tx| tx.subscribe())
    }

    /// Test-only: a clone of a live job's progress-event `Sender`, for
    /// injecting synthetic events directly (bypassing the job's own task and
    /// its `ProgressSink`) — lets a test force
    /// `broadcast::error::RecvError::Lagged` on an already-subscribed
    /// receiver deterministically (send more than the channel's capacity,
    /// with no task-scheduling race), rather than trying to win a timing
    /// race against a real task's own progress reporting. `None` once the
    /// job is terminal and its channel entry has been removed, same as
    /// `subscribe`.
    #[cfg(test)]
    pub(crate) async fn test_progress_sender(
        &self,
        job_id: &str,
    ) -> Option<broadcast::Sender<ProgressEvent>> {
        let events = self.events.read().await;
        events.get(job_id).cloned()
    }
}

/// Background worker: pulls queued jobs and executes them.
async fn run_worker(
    mut receiver: mpsc::Receiver<QueuedJob>,
    registry: JobRegistry,
    inflight: InFlightSet,
    events: EventRegistry,
) {
    while let Some(queued) = receiver.recv().await {
        let job_id = queued.id.clone();
        let store_id = queued.store_id.clone();
        info!("starting job {}", job_id);

        // Mark as running
        {
            let mut reg = registry.write().await;
            if let Some(job) = reg.get_mut(&job_id) {
                start_index_job(job);
            }
        }

        // Build and run the job's future on the async runtime — the
        // ingestion pipeline does its own `spawn_blocking` internally for
        // CPU-bound work, so the queue worker itself stays async
        // (specs/01-architecture.md §6).
        let fut = (queued.task)();
        let result = tokio::spawn(fut).await;

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
                        fail_index_job_with_error(job, &e);
                    }
                    Err(join_err) => {
                        error!("job {} panicked: {}", job_id, join_err);
                        fail_index_job(job, format!("task panicked: {}", join_err));
                    }
                }
            }
        }

        // Tear down this job's progress-event channel now that it's
        // terminal — *after* the registry update above, never before: a
        // subscriber that observes the channel close (`RecvError::Closed`)
        // must always find the job already terminal when it then reads the
        // registry (see `EventRegistry`'s doc comment and issue #83's
        // no-missed-terminal-event requirement). Dropping this last
        // `Sender` clone (the `ProgressSink` given to the task already went
        // out of scope when the task future completed) is what actually
        // closes the channel for any subscribed receivers.
        {
            let mut events = events.write().await;
            events.remove(&job_id);
        }

        // Release the in-flight guard now that this store's job is done
        // (successfully or not) — a new submission for it may proceed.
        {
            let mut guard = inflight.write().await;
            guard.remove(&store_id);
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

    /// Shared trivial task body used by every test below that only cares
    /// "the job completes successfully with no stats to speak of" — a plain
    /// function item (not a closure literal) so its body is one piece of
    /// code shared by every call site, rather than a separate,
    /// separately-instrumented closure per call site (several of which,
    /// written inline, would never actually run to completion within their
    /// own test — e.g. a submission that's expected to be rejected before
    /// the worker ever invokes its task).
    async fn ok_job(_progress: ProgressSink) -> Result<IndexJobStats, Error> {
        Ok(IndexJobStats::default())
    }

    /// Poll `queue.get_job(job_id)` until it reports `Done`, panicking if
    /// `deadline` elapses first — the shared wait-for-completion pattern
    /// used throughout this module's tests so a task's body is guaranteed
    /// to have actually run (not just been scheduled) before the test
    /// inspects it.
    async fn wait_for_done(queue: &JobQueue, job_id: &str) -> IndexJob {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let job = queue.get_job(job_id).await.unwrap();
            if job.state == IndexJobState::Done || job.state == IndexJobState::Failed {
                return job;
            }
            if std::time::Instant::now() > deadline {
                panic!("job did not reach a terminal state in time: {job:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

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

    // --- In-flight guard (#187) ---------------------------------------

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

    // --- Progress-event subscription (#83) -----------------------------

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
    /// in-flight guard tests above, so the subscription is guaranteed to be
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

    // --- Worker resilience -----------------------------------------------

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

    /// `submit` must report the job as `Failed` (not panic or hang) when the
    /// worker side of the channel is already gone — the "queue full or
    /// closed" branch. Constructed directly (this test lives in the same
    /// module as `JobQueue`'s private fields) with a receiver dropped
    /// up front and no `run_worker` task spawned, so the very first `send`
    /// in `submit` is guaranteed to hit a closed channel rather than one
    /// that's merely unpolled.
    #[tokio::test]
    async fn submit_fails_the_job_when_the_worker_channel_is_already_closed() {
        let (sender, receiver) = mpsc::channel::<QueuedJob>(1);
        drop(receiver);
        let queue = JobQueue {
            sender,
            registry: Arc::new(RwLock::new(HashMap::new())),
            inflight: Arc::new(RwLock::new(HashSet::new())),
            events: Arc::new(RwLock::new(HashMap::new())),
            event_capacity: EVENT_CHANNEL_CAPACITY,
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

    // --- Default -----------------------------------------------------------

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
}
