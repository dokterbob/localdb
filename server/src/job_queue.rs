//! Async job queue for indexing work.
//!
//! Accepts `IndexJob` submissions, executes them via the ingestion pipeline,
//! and tracks state/stats so HTTP callers can poll `GET /jobs/{id}`.
//!
//! Jobs are queued via a tokio channel and executed sequentially by a
//! background worker task (one worker per queue for simplicity). The work
//! itself is an async future (`server::job_exec::run_job` in production) —
//! the worker `tokio::spawn`s it and awaits the `JoinHandle`, rather than
//! `spawn_blocking`: the ingestion pipeline does its own blocking dispatch
//! for CPU-bound work internally (`core::blocking::run_blocking`, which
//! uses `tokio::task::block_in_place` on a multi-thread runtime — see
//! specs/01-architecture.md §6), so the queue itself stays on the async
//! runtime.
//!
//! A per-store in-flight guard (`inflight`) rejects a second submission for a
//! store that already has a job queued or running, at submit time, with
//! `Error::IndexInProgress` — before real ingestion, two concurrent jobs
//! against the same store could race on the same `DocumentIndex`/store
//! handle.
//!
//! Cancellation (issue #218) latency: `run_worker` races a running job's
//! future against its `CancellationToken` in one `tokio::select!`, which
//! only gets a chance to observe the token when the task future actually
//! yields control back to the executor — an `.await` on another task, an
//! I/O readiness wait, a timer. `block_in_place` does NOT yield: it blocks
//! the current worker thread until the closure returns, so a cancellation
//! requested mid-parse or mid-embedding-batch does not take effect until
//! that CPU-bound operation finishes on its own. See `run_worker`'s
//! `select!` for where this matters in practice.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use localdb_core::{
    complete_index_job, create_index_job, fail_index_job, fail_index_job_with_error,
    start_index_job, Error, IndexJob, IndexJobScope, IndexJobState, IndexJobStats, ProgressEvent,
    ProgressSink,
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
    /// This job's cancellation signal (issue #218) — the same
    /// `CancellationToken` clone held in this job's `JobHandle` (in
    /// `JobQueue::handles`) at submit time, so triggering it (from
    /// `JobQueue::cancel`, potentially long before the worker ever
    /// dequeues this `QueuedJob`) is visible here too. Cheap to clone:
    /// `CancellationToken` is `Arc`-backed.
    cancel_token: CancellationToken,
}

/// Shared job registry: job_id → IndexJob.
pub type JobRegistry = Arc<RwLock<HashMap<String, IndexJob>>>;

/// Shared set of store ids with a job currently queued or running.
type InFlightSet = Arc<RwLock<HashSet<String>>>;

/// A live job's two per-job handles: its progress-event broadcast sender
/// (issue #83) and its cancellation token (issue #218), held together in one
/// registry entry — they share the exact same lifecycle (created together in
/// `submit`, torn down together in `run_worker` once the job is terminal),
/// so keeping them in two separate `Arc<RwLock<HashMap<..>>>` maps (as an
/// earlier version of this file did) only bought two lock acquisitions and
/// two lookups everywhere instead of one, for no benefit.
struct JobHandle {
    events: broadcast::Sender<ProgressEvent>,
    cancel_token: CancellationToken,
}

/// Shared per-job handle registry: job_id → [`JobHandle`].
///
/// An entry exists from `submit` until the job reaches a terminal state, at
/// which point `run_worker` removes it — dropping the queue's own `events`
/// `Sender` clone. Once every clone (the queue's and the task's
/// `ProgressSink`) is dropped, subscribed receivers observe
/// `RecvError::Closed`, which is how `GET /v1/jobs/{id}/events` (issue #83)
/// knows to stop waiting for more progress and fetch the terminal `IndexJob`
/// from the registry instead.
///
/// Removal always happens *after* the registry's own state update in
/// `run_worker` (see there), so a subscriber that observes the channel close
/// is guaranteed to find the job already terminal in the registry — no
/// window where the terminal event could be missed. `JobQueue::cancel`
/// relies on the same ordering: a non-terminal `IndexJob` in the registry
/// guarantees this job's entry (and so its `cancel_token`) is still present.
type HandleRegistry = Arc<RwLock<HashMap<String, JobHandle>>>;

/// A handle to the job queue.
///
/// Clone-safe: underlying channel, registry, in-flight set, and handle
/// registry are Arc'd.
#[derive(Clone)]
pub struct JobQueue {
    sender: mpsc::Sender<QueuedJob>,
    registry: JobRegistry,
    inflight: InFlightSet,
    handles: HandleRegistry,
    /// Capacity of each job's progress-event broadcast channel — normally
    /// `EVENT_CHANNEL_CAPACITY`, shrinkable in tests via
    /// `new_with_event_capacity` (issue #187 review, finding 4d) so a test
    /// can force `broadcast::error::RecvError::Lagged` deterministically
    /// with only a handful of events instead of needing 1024+.
    event_capacity: usize,
    /// Configured worker-pool size (issue #208, `server.job_workers` config
    /// key). Stored for the next step's consumption; `with_workers` still
    /// spawns exactly one worker regardless of this value — see
    /// `worker_count`'s doc comment. Only read via the `#[cfg(test)]`
    /// `worker_count` accessor until the pool itself is wired up, hence the
    /// blanket allow here rather than on the (production) field use sites.
    #[allow(dead_code)]
    workers: usize,
}

impl JobQueue {
    /// Create a new job queue and start the background worker.
    ///
    /// Returns the queue handle. The worker runs until the sender is dropped.
    /// Equivalent to `with_workers(1)`.
    pub fn new() -> Self {
        Self::with_workers(1)
    }

    /// Create a new job queue configured for `workers` job-queue workers
    /// (issue #208, `server.job_workers` config key).
    ///
    /// The worker count is stored on the returned queue but not yet acted
    /// on: this constructor still spawns exactly one worker regardless of
    /// `workers`.
    // #208: worker pool wired in the next commit
    pub fn with_workers(workers: usize) -> Self {
        Self::with_capacity(EVENT_CHANNEL_CAPACITY, workers)
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
        Self::with_capacity(capacity, 1)
    }

    /// Test-only: the worker-pool size this queue was constructed with (see
    /// `with_workers`) — not yet consulted by the worker itself (issue #208;
    /// wired in a later commit), but observable here so a test can pin that
    /// the value survives construction.
    #[cfg(test)]
    pub(crate) fn worker_count(&self) -> usize {
        self.workers
    }

    fn with_capacity(event_capacity: usize, workers: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<QueuedJob>(QUEUE_CAPACITY);
        let registry: JobRegistry = Arc::new(RwLock::new(HashMap::new()));
        let inflight: InFlightSet = Arc::new(RwLock::new(HashSet::new()));
        let handles: HandleRegistry = Arc::new(RwLock::new(HashMap::new()));

        let worker_registry = registry.clone();
        let worker_inflight = inflight.clone();
        let worker_handles = handles.clone();
        // #208: worker pool wired in the next commit — exactly one worker
        // spawned regardless of `workers`, matching JobQueue::new's
        // documented behavior until the pool itself is implemented.
        tokio::spawn(async move {
            run_worker(receiver, worker_registry, worker_inflight, worker_handles).await;
        });

        Self {
            sender,
            registry,
            inflight,
            handles,
            event_capacity,
            workers,
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

        // Create this job's progress-event channel, its sink, and its
        // cancellation token, before enqueuing — so `subscribe(job_id)`
        // (issue #83) and `JobQueue::cancel(job_id)` (issue #218) both work
        // the instant `submit` returns, even before the worker has picked
        // the job up (the latter is what makes cancelling a still-`Pending`
        // job possible at all).
        let (tx, _rx) = broadcast::channel::<ProgressEvent>(self.event_capacity);
        let cancel_token = CancellationToken::new();
        {
            let mut handles = self.handles.write().await;
            handles.insert(
                job_id.clone(),
                JobHandle {
                    events: tx.clone(),
                    cancel_token: cancel_token.clone(),
                },
            );
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
            cancel_token,
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
            let mut handles = self.handles.write().await;
            handles.remove(&job_id);
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

    /// Request cancellation of `job_id` (issue #218; `DELETE /v1/jobs/{id}`).
    ///
    /// - Unknown job id: `Error::JobNotFound`.
    /// - Job already terminal (`Done`/`Failed`) *before* the token is ever
    ///   touched: `Error::JobAlreadyTerminal` unconditionally, including a
    ///   job that was already `Failed`/`job_cancelled` (e.g. a repeated
    ///   cancel) — a cancel landing after normal completion (or after a
    ///   real failure) must never overwrite the recorded outcome, so this
    ///   check happens against the registry, not the cancellation token,
    ///   strictly before the token is touched at all.
    /// - Otherwise (`Pending` or `Running`): triggers this job's
    ///   `CancellationToken`, then re-checks the registry once more (see
    ///   below) before answering. A `Pending` job's worker iteration
    ///   observes the token before ever starting the pipeline (see
    ///   `run_worker`); a `Running` job's `tokio::select!` observes it at
    ///   its next scheduling point (subject to this crate's
    ///   `block_in_place` cancellation-latency caveat — see this module's
    ///   doc comment).
    ///
    /// The pre-trigger check and the token trigger are two separate lock
    /// acquisitions (`registry` and `handles` are distinct `RwLock`s), so a
    /// job can race to a terminal state in between — on a multi-thread
    /// runtime, `run_worker` can observe the just-triggered token and
    /// finish recording `Failed`/`job_cancelled` before this function's own
    /// post-trigger read ever runs. Without a second look this would either
    /// report "cancellation requested" for a job that, in fact, already
    /// finished by some *other* means (issue #218 review, fix 3), or —
    /// naively treating any post-trigger-terminal job as a conflict — hand
    /// the very caller whose cancellation just worked a confusing `409`
    /// (issue #218 review, fix 5). `resolve_post_trigger_outcome` (below)
    /// is what tells those two cases apart: a job that is terminal *because
    /// this cancellation reached it* (`Failed` with
    /// `error_code: "job_cancelled"`) is success, indistinguishable from —
    /// and no less valid a response than — a `Pending`/`Running` snapshot
    /// taken a moment earlier; any other terminal state (`Done`, or
    /// `Failed` with a different `error_code`) is a genuine conflict, since
    /// the job reached its own outcome first, unrelated to this cancel.
    /// Even this narrows rather than eliminates the window — the one gap
    /// that genuinely can't be closed this way is between this function
    /// returning and the response reaching the caller, exactly what
    /// "202 = requested, not guaranteed" already covers.
    pub async fn cancel(&self, job_id: &str) -> Result<IndexJob, Error> {
        Self::terminal_check(&self.registry, job_id).await?;

        if let Some(handle) = self.handles.read().await.get(job_id) {
            handle.cancel_token.cancel();
        }

        let reg = self.registry.read().await;
        let job = reg.get(job_id).ok_or_else(|| Error::JobNotFound {
            id: job_id.to_string(),
        })?;
        resolve_post_trigger_outcome(job)
    }

    /// The pre-trigger half of `cancel`'s bracketing check: `Ok(job)` for a
    /// known, non-terminal job; `Err(JobNotFound)` for an unknown id;
    /// `Err(JobAlreadyTerminal)` for one already `Done`/`Failed` —
    /// unconditionally, unlike [`resolve_post_trigger_outcome`] below,
    /// since nothing has been triggered yet for a repeated cancel to have
    /// legitimately caused.
    async fn terminal_check(registry: &JobRegistry, job_id: &str) -> Result<IndexJob, Error> {
        let reg = registry.read().await;
        let job = reg.get(job_id).ok_or_else(|| Error::JobNotFound {
            id: job_id.to_string(),
        })?;
        if matches!(job.state, IndexJobState::Done | IndexJobState::Failed) {
            return Err(Error::JobAlreadyTerminal);
        }
        Ok(job.clone())
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
        let handles = self.handles.read().await;
        handles.get(job_id).map(|h| h.events.subscribe())
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
        let handles = self.handles.read().await;
        handles.get(job_id).map(|h| h.events.clone())
    }
}

/// Background worker: pulls queued jobs and executes them.
async fn run_worker(
    mut receiver: mpsc::Receiver<QueuedJob>,
    registry: JobRegistry,
    inflight: InFlightSet,
    handles: HandleRegistry,
) {
    while let Some(queued) = receiver.recv().await {
        let job_id = queued.id.clone();
        let store_id = queued.store_id.clone();
        let cancel_token = queued.cancel_token;

        // A job cancelled while it was still `Pending` (issue #218): the
        // token was triggered before this worker ever dequeued it — never
        // start the pipeline at all, not even one poll of the task future.
        // `(queued.task)()` (which *builds* the future) is deliberately
        // never called on this path.
        if cancel_token.is_cancelled() {
            info!("job {} was cancelled before starting", job_id);
            let mut reg = registry.write().await;
            if let Some(job) = reg.get_mut(&job_id) {
                fail_index_job_with_error(job, &Error::JobCancelled);
            }
        } else {
            info!("starting job {}", job_id);

            // Mark as running
            {
                let mut reg = registry.write().await;
                if let Some(job) = reg.get_mut(&job_id) {
                    start_index_job(job);
                }
            }

            // Build and run the job's future on the async runtime — the
            // ingestion pipeline does its own blocking dispatch for
            // CPU-bound work internally (`core::blocking::run_blocking`,
            // specs/01-architecture.md §6), so the queue worker itself
            // stays async. Raced against the cancellation token in one
            // `select!` (issue #218): this is what covers an in-progress
            // `backon` retry sleep or a `governor` pacing wait without
            // threading the token through `core`/`ingest` at all — but
            // only at a genuine `.await` yield point. A `block_in_place`
            // call (this module's doc comment) blocks the worker thread
            // without yielding, so cancellation requested mid-parse or
            // mid-embedding-batch takes effect only once that operation
            // returns on its own, not before.
            let fut = (queued.task)();
            let mut handle = tokio::spawn(fut);
            let outcome = tokio::select! {
                r = &mut handle => JobOutcome::Finished(r),
                _ = cancel_token.cancelled() => {
                    // `abort()` only *requests* cancellation. If the task
                    // had already finished by the time this branch won the
                    // race — the natural-completion/cancel race — `abort()`
                    // is a no-op and the re-awaited handle below resolves
                    // to the task's real result, not a cancellation
                    // `JoinError`; `resolve_aborted` (issue #218 review,
                    // fix 1) tells the two apart so a real result always
                    // wins over the cancellation flag. Only when `abort()`
                    // actually pre-empted the task (its future dropped,
                    // triggering Wave 1's synchronous mid-write rollback
                    // guarantee) does the re-await resolve to a
                    // `JoinError` with `is_cancelled() == true`. Either
                    // way, awaiting the handle again blocks until the task
                    // has genuinely stopped running, so the in-flight
                    // guard released below is never premature — no window
                    // where a fresh submission for this store could start
                    // while the old task is still being torn down.
                    handle.abort();
                    resolve_aborted((&mut handle).await)
                }
            };

            // Update registry
            {
                let mut reg = registry.write().await;
                if let Some(job) = reg.get_mut(&job_id) {
                    match outcome {
                        JobOutcome::Finished(Ok(Ok(stats))) => {
                            info!("job {} completed: {:?}", job_id, stats);
                            complete_index_job(job, stats);
                        }
                        JobOutcome::Finished(Ok(Err(e))) => {
                            warn!("job {} failed: {}", job_id, e);
                            fail_index_job_with_error(job, &e);
                        }
                        JobOutcome::Finished(Err(join_err)) => {
                            error!("job {} panicked: {}", job_id, join_err);
                            fail_index_job(job, format!("task panicked: {}", join_err));
                        }
                        JobOutcome::Cancelled => {
                            info!("job {} cancelled", job_id);
                            fail_index_job_with_error(job, &Error::JobCancelled);
                        }
                    }
                }
            }
        }

        // Tear down this job's handle (progress-event channel + cancel
        // token) now that it's terminal — *after* the registry update
        // above, never before: a subscriber that observes the channel
        // close (`RecvError::Closed`) must always find the job already
        // terminal when it then reads the registry (see
        // `HandleRegistry`'s doc comment and issue #83's
        // no-missed-terminal-event requirement); `JobQueue::cancel` relies
        // on the same ordering. Dropping the events `Sender`'s last clone
        // (the `ProgressSink` given to the task already went out of scope
        // when the task future completed or was dropped) is what actually
        // closes the channel for any subscribed receivers.
        {
            let mut handles = handles.write().await;
            handles.remove(&job_id);
        }

        // Release the in-flight guard now that this store's job is done
        // (successfully, failed, or cancelled) — a new submission for it
        // may proceed.
        {
            let mut guard = inflight.write().await;
            guard.remove(&store_id);
        }
    }
    info!("job queue worker stopped");
}

/// Outcome of racing a running job's future against its cancellation token
/// in `run_worker`'s `tokio::select!` (issue #218).
#[derive(Debug)]
enum JobOutcome {
    /// The task's `JoinHandle` resolved on its own — either a normal
    /// `Ok`/`Err` result, or `Err(JoinError)` if it panicked.
    Finished(Result<Result<IndexJobStats, Error>, tokio::task::JoinError>),
    /// The cancellation token fired first, and the task was actually
    /// aborted before it produced a result.
    Cancelled,
}

/// Resolve a `JoinHandle`'s re-await *after* `handle.abort()` was called in
/// `run_worker`'s cancellation branch (issue #218 review, fix 1).
///
/// `abort()` only requests cancellation — if the task had already finished
/// before this branch of the `select!` won the race (the
/// natural-completion/cancel race: the job's future resolved to a real
/// `Ok`/`Err` a moment before the cancellation signal was even observed),
/// `abort()` is a complete no-op and the re-awaited handle resolves to that
/// real result, not a cancellation error. Reporting `Cancelled` in that case
/// would silently discard a genuinely-completed (and, per Wave 1, durably
/// committed) run — recording `Failed`/`job_cancelled` over it, or masking
/// a real failure's error code. So: a real result (`Ok(_)`, or `Err` from a
/// genuine panic — `is_cancelled() == false`) always wins over the
/// cancellation flag; only `Err(join_err)` with `join_err.is_cancelled()`
/// means the abort actually pre-empted the task before it produced
/// anything, which is the only case this reports as `Cancelled`.
fn resolve_aborted(
    joined: Result<Result<IndexJobStats, Error>, tokio::task::JoinError>,
) -> JobOutcome {
    match joined {
        Err(join_err) if join_err.is_cancelled() => JobOutcome::Cancelled,
        other => JobOutcome::Finished(other),
    }
}

/// Decide `JobQueue::cancel`'s response from the job's registry state
/// *after* its cancellation token has been triggered (issue #218 review,
/// fix 3, then fix 5).
///
/// A job observed non-terminal here is the ordinary "cancellation
/// requested" case: `Ok(job)`. A job observed terminal needs one more
/// distinction, unlike the *pre*-trigger check (`terminal_check`, which
/// treats every terminal state as a conflict): the token trigger and this
/// read are two separate lock acquisitions, so `run_worker` can
/// legitimately race this very call and finish recording
/// `Failed`/`job_cancelled` before this function ever runs. That specific
/// terminal state — and only that one — means the outcome the caller asked
/// for was actually achieved (by this call, or rarely a concurrent one),
/// so it is `Ok(job)`, not a conflict; reporting `Err(JobAlreadyTerminal)`
/// there would hand the very caller whose cancellation just worked a
/// confusing `409`. Any *other* terminal state — `Done`, or `Failed` with a
/// different `error_code` — reached its own outcome first, unrelated to
/// this cancel, and stays `Err(JobAlreadyTerminal)`.
fn resolve_post_trigger_outcome(job: &IndexJob) -> Result<IndexJob, Error> {
    let is_terminal = matches!(job.state, IndexJobState::Done | IndexJobState::Failed);
    let is_this_cancellation =
        job.state == IndexJobState::Failed && job.error_code.as_deref() == Some("job_cancelled");
    if is_terminal && !is_this_cancellation {
        return Err(Error::JobAlreadyTerminal);
    }
    Ok(job.clone())
}

impl Default for JobQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
