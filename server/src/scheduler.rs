//! URL refresh scheduling for `url` sources.
//!
//! Per T11 scope: "URL refresh scheduling". Daemon-exclusive capability;
//! embedded mode does one-shot equivalents.
//!
//! Each `url` source can declare a `refresh_interval_secs`. The scheduler
//! runs a periodic loop that, for each URL source due for refresh, submits
//! an index job to the job queue.
//!
//! See PLAN.md T11 and specs/01-architecture.md §3.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use localdb_core::{Error, IndexJobScope};

use crate::job_queue::JobQueue;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// UrlRefreshRecord — tracks the last refresh time per URL source
// ---------------------------------------------------------------------------

/// State for a single URL source refresh.
#[derive(Debug, Clone)]
pub struct UrlRefreshRecord {
    /// Source ID.
    pub source_id: String,
    /// Store name owning this source.
    pub store_name: String,
    /// The URL to fetch.
    pub url: String,
    /// Refresh interval (None = no periodic refresh).
    pub interval: Option<Duration>,
    /// Time of the last successful refresh.
    pub last_refreshed: Option<Instant>,
    /// A refresh job for this source has been submitted and its completion
    /// watcher has not yet stamped `last_refreshed` (PR #229 round-7
    /// review). `tick` skips sources with this set: `last_refreshed` is
    /// only written by the detached watcher task *after* the job's terminal
    /// transition, and the job queue's per-store in-flight guard is
    /// released *before* that watcher gets to run — so without this flag, a
    /// tick landing in that window would see a stale timestamp and a free
    /// guard, and resubmit immediately, defeating the full-interval backoff
    /// a completed (or cancelled) refresh is supposed to buy. Set on
    /// successful submit, cleared by the watcher in the same write that
    /// stamps the timestamp; a failed submit never sets it (the next tick
    /// should retry, as before).
    pub refresh_inflight: bool,
}

// ---------------------------------------------------------------------------
// UrlRefreshScheduler
// ---------------------------------------------------------------------------

/// Scheduler that periodically triggers re-index jobs for URL sources.
///
/// Designed to run as a long-lived background task alongside the daemon.
/// Safe to clone (internally Arc-based).
#[derive(Clone)]
pub struct UrlRefreshScheduler {
    records: Arc<RwLock<HashMap<String, UrlRefreshRecord>>>,
    queue: JobQueue,
    /// The `AppState` `tick()` runs real ingestion against, via
    /// `job_exec::run_job`. `None` until `attach_state` is called.
    ///
    /// Constructor order forces this two-step wiring: `AppState::new` takes
    /// an already-built `UrlRefreshScheduler` as a parameter (so sources can
    /// register with it), so the scheduler can't be given the state it will
    /// eventually drive until after that state exists. `build_daemon_state`
    /// calls `attach_state` immediately after constructing the state.
    ///
    /// This does create a permanent `Arc` reference cycle (`AppState` holds
    /// this scheduler, this scheduler holds that same `AppState`) — harmless
    /// for a daemon process: both live for the process's entire lifetime
    /// regardless, so nothing is ever "leaked" that would otherwise have
    /// been freed.
    state: Arc<RwLock<Option<AppState>>>,
}

impl UrlRefreshScheduler {
    /// Create a new scheduler backed by the given job queue.
    ///
    /// Real ingestion is inert until [`Self::attach_state`] is called —
    /// `tick()` still tracks due sources and submits jobs, but until the
    /// state is attached, submitted jobs fail with a clear error rather than
    /// fabricating success (see `tick`'s doc comment).
    pub fn new(queue: JobQueue) -> Self {
        Self {
            records: Arc::new(RwLock::new(HashMap::new())),
            queue,
            state: Arc::new(RwLock::new(None)),
        }
    }

    /// Attach the `AppState` that `tick()` runs ingestion against.
    ///
    /// Must be called once, after `AppState::new` resolves — see the `state`
    /// field's doc comment for why this can't happen at construction time.
    pub async fn attach_state(&self, state: AppState) {
        let mut w = self.state.write().await;
        *w = Some(state);
    }

    /// Register a URL source for periodic refresh.
    ///
    /// If `interval_secs` is `None`, the source is tracked but never
    /// automatically refreshed (manual refresh only via `POST /jobs`).
    pub async fn register(
        &self,
        source_id: String,
        store_name: String,
        url: String,
        interval_secs: Option<u64>,
    ) {
        let record = UrlRefreshRecord {
            source_id: source_id.clone(),
            store_name,
            url,
            interval: interval_secs.map(Duration::from_secs),
            last_refreshed: None,
            refresh_inflight: false,
        };
        let mut records = self.records.write().await;
        records.insert(source_id, record);
    }

    /// Unregister a URL source (called when the source is removed).
    pub async fn unregister(&self, source_id: &str) {
        let mut records = self.records.write().await;
        records.remove(source_id);
    }

    /// Check all registered sources and submit refresh jobs for those that are due.
    ///
    /// A source is due for refresh when:
    /// - It has an `interval` configured, AND
    /// - Either it has never been refreshed, OR
    ///   `now - last_refreshed >= interval`.
    ///
    /// Each due source's job runs real ingestion via `job_exec::run_job`,
    /// scoped to just that source (`IndexJobScope::Source`) with
    /// `DeletionPolicy::Retain` — a scheduled background refresh never
    /// prunes documents on its own; that stays an explicit, opt-in CLI/HTTP
    /// action (issues #156/#185).
    pub async fn tick(&self) {
        let now = Instant::now();
        let mut due: Vec<UrlRefreshRecord> = Vec::new();

        {
            let records = self.records.read().await;
            for record in records.values() {
                // A source whose previous refresh hasn't been stamped yet is
                // never due, regardless of its timestamp (PR #229 round-7
                // review — see `refresh_inflight`'s doc comment): the stamp
                // lands after the job queue's in-flight guard is already
                // released, so the guard alone can't suppress a resubmit in
                // that window.
                if record.refresh_inflight {
                    continue;
                }
                if let Some(interval) = record.interval {
                    let is_due = match record.last_refreshed {
                        None => true,
                        Some(last) => now.duration_since(last) >= interval,
                    };
                    if is_due {
                        due.push(record.clone());
                    }
                }
            }
        }

        for record in due {
            info!(
                "URL refresh due for source '{}' ({}), submitting job",
                record.source_id, record.url
            );

            let source_id = record.source_id.clone();
            let store_name_for_submit = record.store_name.clone();
            let source_id_for_closure = source_id.clone();
            let store_name_for_closure = record.store_name.clone();
            let state_for_closure = self.state.read().await.clone();

            let submit_result = self
                .queue
                .submit(
                    &store_name_for_submit,
                    IndexJobScope::Source {
                        source_id: source_id.clone(),
                    },
                    move |progress| async move {
                        debug!(
                            "URL refresh job running for source '{}' ({})",
                            source_id_for_closure, store_name_for_closure
                        );

                        let state = state_for_closure.ok_or_else(|| Error::Internal {
                            message: "URL refresh scheduler has no state attached".to_string(),
                            correlation_id: "url_refresh_no_state".to_string(),
                        })?;
                        let store_row = state
                            .backend()
                            .get_store_by_name(&store_name_for_closure)
                            .await?
                            .ok_or_else(|| Error::StoreNotFound {
                                id: store_name_for_closure.clone(),
                            })?;
                        let refresh_scope = IndexJobScope::Source {
                            source_id: source_id_for_closure.clone(),
                        };
                        // Shared with `handlers::jobs::create_job` via
                        // `AppState::run_scoped_job` (#187 review, DRY
                        // finding): resolves the scoped source before
                        // deciding whether to build/reuse an embedder —
                        // a scope that fails to resolve (e.g. the source
                        // was deleted) surfaces that error before paying
                        // for a (potentially huge) embedding model
                        // build, and a resolved-but-empty scope never
                        // builds one at all (Codex review finding G1,
                        // issue #187). Only the deletion policy differs
                        // between the two callers: a scheduled refresh
                        // always uses `Retain`, never pruning documents
                        // on its own (issues #156/#185).
                        state
                            .run_scoped_job(
                                &store_row,
                                refresh_scope,
                                localdb_core::DeletionPolicy::Retain,
                                progress,
                            )
                            .await
                    },
                )
                .await;

            match submit_result {
                Ok(job) => {
                    // Wait for *this* job to reach a terminal state and
                    // stamp `last_refreshed` then, from a task that lives
                    // entirely outside the job's own submitted future
                    // (issue #218-followups Fix C). Previously the stamp
                    // was the tail expression of the closure above — but a
                    // cancelled job's future is `handle.abort()`ed by
                    // `job_queue::process_job`, which drops everything
                    // still pending inside it, including that stamp, before
                    // it ever runs. A cancelled refresh was therefore never
                    // stamped and got resubmitted on the very next tick —
                    // the backoff a cancellation is supposed to buy was
                    // silently undone. Watching from a separate task
                    // instead observes the registry's terminal write
                    // (`process_job` commits it before tearing down the
                    // progress channel — see `HandleRegistry`'s doc comment
                    // in `job_queue.rs`) regardless of *how* the job got
                    // there: normal completion, a real failure, or
                    // cancellation all stamp the same way now.
                    // Suppress this source before the watcher below exists
                    // to clear it (PR #229 round-7 review): from here until
                    // the watcher stamps, `tick` must not consider the
                    // source due — the queue's own in-flight guard stops
                    // covering it the moment `process_job` finishes, which
                    // can be before the watcher ever runs. Serial `tick`s
                    // (one `run` loop) mean no due-check can interleave
                    // between the submit above and this write.
                    {
                        let mut records = self.records.write().await;
                        if let Some(r) = records.get_mut(&record.source_id) {
                            r.refresh_inflight = true;
                        }
                    }
                    let queue_for_wait = self.queue.clone();
                    let records_for_wait = self.records.clone();
                    let source_id_for_wait = record.source_id.clone();
                    let job_id = job.id.clone();
                    tokio::spawn(async move {
                        wait_for_job_terminal(&queue_for_wait, &job_id).await;
                        // Record completion time now, not at submit time
                        // (#187 review F1): stamping at submit made a slow
                        // job look "refreshed" while it was still running,
                        // drifting scheduling away from actual completion.
                        // A failed (or cancelled) job is stamped too — it
                        // must never tight-loop retrying; it waits out a
                        // full interval just like a successful refresh
                        // does. Only touch the record if it's still
                        // registered: the source may have been
                        // unregistered mid-flight, and completion must
                        // never re-insert a removed record. Clearing
                        // `refresh_inflight` in the same write as the stamp
                        // means `tick` always sees either "suppressed" or
                        // "freshly stamped", never the stale-timestamp gap
                        // between them (PR #229 round-7 review).
                        let mut records = records_for_wait.write().await;
                        if let Some(r) = records.get_mut(&source_id_for_wait) {
                            r.last_refreshed = Some(Instant::now());
                            r.refresh_inflight = false;
                        }
                    });
                }
                Err(e) => log_submit_failure(&record.source_id, &e),
            }
        }
    }

    /// Run the scheduler loop, calling `tick()` at the given poll interval.
    ///
    /// This function runs forever (until the task is cancelled/dropped).
    pub async fn run(self, poll_interval: Duration) {
        info!(
            "URL refresh scheduler started (poll interval: {:?})",
            poll_interval
        );
        loop {
            tokio::time::sleep(poll_interval).await;
            self.tick().await;
        }
    }

    /// Number of registered URL sources.
    pub async fn source_count(&self) -> usize {
        self.records.read().await.len()
    }
}

/// Wait until `job_id` reaches a terminal state (issue #218-followups Fix
/// C), observed via its progress-event channel closing — mirrors
/// `cli::job_attach::drive_embedded_job`'s wait pattern, but discards the
/// progress events themselves; only the channel's closure matters here.
/// Per `HandleRegistry`'s doc comment in `job_queue.rs`, that closure is
/// guaranteed to happen only *after* `job_queue::process_job` has already
/// committed the job's terminal state to the registry — so by the time this
/// returns, the caller can trust the job is genuinely done, failed, or
/// cancelled, regardless of which. If `subscribe` returns `None` the job
/// was already terminal (and torn down) by the time this ran — nothing more
/// to wait for.
async fn wait_for_job_terminal(queue: &JobQueue, job_id: &str) {
    if let Some(mut rx) = queue.subscribe(job_id).await {
        loop {
            match rx.recv().await {
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// Log a failed job submission for a due source at the right level.
///
/// A5/A8 (issue #207): `last_refreshed` is only stamped on completion (see
/// `tick`'s doc comment), so a source stays "due" until its job actually
/// finishes. Under normal (fast, ~2s) jobs, re-submission racing an
/// already-running job was a rare timing coincidence. Under real pacing
/// (backon/governor slowing per-host requests to ~1 req/s), a single job can
/// legitimately run for 50s+ — comfortably longer than this scheduler's 60s
/// tick interval — so *every* tick re-submits while the previous run is
/// still in flight and hits the per-store in-flight guard
/// (`Error::IndexInProgress`). That is an expected outcome of pacing, not a
/// failure worth a `warn!` on every tick; every other submission error still
/// warns normally.
fn log_submit_failure(source_id: &str, err: &Error) {
    if matches!(err, Error::IndexInProgress) {
        debug!(
            "URL refresh scheduler: job already in progress for source '{}', \
             skipping this tick",
            source_id
        );
    } else {
        warn!(
            "URL refresh scheduler: failed to submit job for source '{}': {}",
            source_id, err
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::IndexJobState;
    use std::time::Duration;

    fn make_scheduler() -> UrlRefreshScheduler {
        let queue = JobQueue::new();
        UrlRefreshScheduler::new(queue)
    }

    /// Poll `scheduler.records` until `source_id`'s `last_refreshed` is
    /// set, up to 5s. Since issue #218-followups Fix C, the stamp lands on
    /// a separate spawned task (`wait_for_job_terminal` + the stamp itself)
    /// woken by the job's own terminal transition, independently scheduled
    /// from whatever poll a test itself uses to observe that same terminal
    /// state — so a test that needs to see the stamp must poll for it in
    /// its own right, not assume it's already visible the instant the
    /// job's state is.
    async fn wait_for_last_refreshed_stamp(scheduler: &UrlRefreshScheduler, source_id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let records = scheduler.records.read().await;
                if records
                    .get(source_id)
                    .is_some_and(|r| r.last_refreshed.is_some())
                {
                    return;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("last_refreshed for '{source_id}' was never stamped in time");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn register_and_count() {
        let scheduler = make_scheduler();
        assert_eq!(scheduler.source_count().await, 0);

        scheduler
            .register(
                "src-1".to_string(),
                "store-A".to_string(),
                "https://example.com/feed".to_string(),
                Some(3600),
            )
            .await;

        assert_eq!(scheduler.source_count().await, 1);
    }

    #[tokio::test]
    async fn unregister_removes_source() {
        let scheduler = make_scheduler();
        scheduler
            .register(
                "src-1".to_string(),
                "store-A".to_string(),
                "https://example.com/feed".to_string(),
                Some(3600),
            )
            .await;

        scheduler.unregister("src-1").await;
        assert_eq!(scheduler.source_count().await, 0);
    }

    #[tokio::test]
    async fn tick_submits_job_for_due_sources() {
        // A source with interval=0 is always due.
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        scheduler
            .register(
                "src-refresh".to_string(),
                "my-store".to_string(),
                "https://example.com/docs".to_string(),
                Some(0), // 0-second interval → always due
            )
            .await;

        scheduler.tick().await;

        // Give the job queue worker time to pick up the job.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let jobs = queue.list_jobs().await;
        assert_eq!(
            jobs.len(),
            1,
            "tick() should have submitted one job for the due source"
        );
        let job = &jobs[0];
        assert_eq!(job.store_id, "my-store");
        assert!(
            matches!(
                &job.scope,
                localdb_core::IndexJobScope::Source { source_id }
                    if source_id == "src-refresh"
            ),
            "job scope should reference the source: {:?}",
            job.scope
        );
    }

    #[tokio::test]
    async fn tick_does_not_submit_job_for_sources_without_interval() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        // No interval → never auto-refreshed.
        scheduler
            .register(
                "src-manual".to_string(),
                "my-store".to_string(),
                "https://example.com/page".to_string(),
                None,
            )
            .await;

        scheduler.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let jobs = queue.list_jobs().await;
        assert!(
            jobs.is_empty(),
            "tick() should not submit jobs for sources with no interval"
        );
    }

    #[tokio::test]
    async fn tick_twice_only_submits_once_when_not_due_yet() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        // Interval = 1 hour → only due on the first tick (never refreshed).
        scheduler
            .register(
                "src-hourly".to_string(),
                "my-store".to_string(),
                "https://example.com/data".to_string(),
                Some(3600),
            )
            .await;

        // First tick: source was never refreshed → is due → submits job.
        scheduler.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let after_first_tick = queue.list_jobs().await.len();
        assert_eq!(after_first_tick, 1, "first tick should submit one job");

        // Second tick immediately after: `last_refreshed` is ~now, interval not reached.
        scheduler.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let after_second_tick = queue.list_jobs().await.len();
        assert_eq!(
            after_second_tick, 1,
            "second tick should not re-submit (interval not elapsed)"
        );
    }

    /// Without `attach_state`, `tick()` still submits and the job still
    /// reaches a terminal state — but honestly: `Failed`, with a clear
    /// error, never a fabricated `Done` with zero stats (issue #187 §1).
    #[tokio::test]
    async fn submitted_job_without_attached_state_fails_honestly() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        scheduler
            .register(
                "src-complete".to_string(),
                "store-Z".to_string(),
                "https://example.com/".to_string(),
                Some(0),
            )
            .await;

        scheduler.tick().await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("refresh job did not reach a terminal state in time");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            let jobs = queue.list_jobs().await;
            if let Some(job) = jobs.first() {
                if job.state == IndexJobState::Failed {
                    assert!(
                        job.error
                            .as_deref()
                            .is_some_and(|e| e.contains("no state attached")),
                        "expected a 'no state attached' error, got: {:?}",
                        job.error
                    );
                    break;
                }
                assert_ne!(
                    job.state,
                    IndexJobState::Done,
                    "a job with no attached state must never report Done"
                );
            }
        }
    }

    /// #187 review F1: `tick()` submits the job, but must not stamp
    /// `last_refreshed` until the job actually reaches a terminal state.
    /// Uses the same deterministic fast-failure path as
    /// `submitted_job_without_attached_state_fails_honestly` above — no
    /// state attached, so the job fails immediately without needing a real
    /// store/embedder — rather than a real, slow ingestion, so the
    /// "submitted but not yet complete" window is reliably observable on a
    /// single-threaded test runtime (the worker task doesn't get to run
    /// until this test task itself yields, e.g. via `sleep`).
    ///
    /// Since issue #218-followups Fix C, the stamp itself happens on a
    /// separate spawned task that wakes up once the job's progress channel
    /// closes (see `wait_for_job_terminal`) — deliberately *not* inline
    /// with the job's own future — so "the job is Failed" and "the stamp
    /// has landed" are two independently-scheduled events; the final
    /// assertion below polls for the stamp with its own bounded deadline
    /// rather than asserting it's already visible the instant this test's
    /// own poll first observes `Failed`.
    #[tokio::test]
    async fn last_refreshed_is_recorded_on_completion_not_on_submission() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        scheduler
            .register(
                "src-timing".to_string(),
                "store-T".to_string(),
                "https://example.com/".to_string(),
                Some(0),
            )
            .await;

        scheduler.tick().await;

        // Immediately after `tick()` returns: the job has been submitted
        // (it exists in the queue) but the worker — a separate task on this
        // current-thread runtime — has not yet had a chance to run it.
        assert_eq!(
            queue.list_jobs().await.len(),
            1,
            "tick() should have submitted the job"
        );
        {
            let records = scheduler.records.read().await;
            assert_eq!(
                records.get("src-timing").unwrap().last_refreshed,
                None,
                "last_refreshed must not be set merely because the job was submitted"
            );
        }

        // Drive the job to its terminal (failed, no state attached) state.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("refresh job did not reach a terminal state in time");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            let jobs = queue.list_jobs().await;
            if let Some(job) = jobs.first() {
                if job.state == IndexJobState::Failed {
                    break;
                }
            }
        }

        // The stamp lands asynchronously (a separate task, woken by the
        // same terminal transition) — poll for it with its own deadline.
        wait_for_last_refreshed_stamp(&scheduler, "src-timing").await;
    }

    /// PR #229 round-7 review: a source whose refresh job has been
    /// submitted but whose completion watcher has not yet stamped
    /// `last_refreshed` is never due — even when its timestamp is stale
    /// and the job queue would accept a submission. The queue's per-store
    /// in-flight guard is released by `process_job` *before* the watcher
    /// task gets scheduled, so the guard alone cannot suppress a resubmit
    /// in that window; `refresh_inflight` must. Stages the window directly
    /// (flag set, queue empty, timestamp stale) — the real interleaving
    /// depends on task scheduling order and can't be forced
    /// deterministically, but the flag's set/clear wiring is covered by
    /// the stamp-lifecycle tests around this one.
    #[tokio::test]
    async fn tick_skips_sources_with_an_unstamped_inflight_refresh() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());
        scheduler
            .register(
                "src-window".to_string(),
                "store-W".to_string(),
                "https://example.com/".to_string(),
                Some(0),
            )
            .await;

        // The exact race window: a previous refresh was submitted
        // (`refresh_inflight` set by `tick`), its job has fully left the
        // queue (guard released — the queue is empty), and the watcher has
        // not yet stamped (`last_refreshed` still `None`, maximally stale).
        {
            let mut records = scheduler.records.write().await;
            records.get_mut("src-window").unwrap().refresh_inflight = true;
        }

        scheduler.tick().await;

        assert_eq!(
            queue.list_jobs().await.len(),
            0,
            "a tick in the unstamped-inflight window must not submit a refresh"
        );

        // Watcher completion: stamp + clear in one write, exactly as the
        // real watcher does. With interval 0 the source is immediately due
        // again by timestamp — the next tick may submit normally.
        {
            let mut records = scheduler.records.write().await;
            let r = records.get_mut("src-window").unwrap();
            r.last_refreshed = Some(Instant::now());
            r.refresh_inflight = false;
        }
        scheduler.tick().await;
        assert_eq!(
            queue.list_jobs().await.len(),
            1,
            "once stamped and cleared, a due source submits normally again"
        );
    }

    /// #187 review F1: a failed job must still be stamped with
    /// `last_refreshed`, so it waits out a full interval before being
    /// retried rather than tight-looping.
    #[tokio::test]
    async fn failed_job_is_not_resubmitted_before_a_full_interval_elapses() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        scheduler
            .register(
                "src-retry".to_string(),
                "store-R".to_string(),
                "https://example.com/".to_string(),
                Some(3600), // 1 hour — long enough to never elapse in-test.
            )
            .await;

        scheduler.tick().await;

        // Drive the job to Failed (no state attached).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("refresh job did not fail in time");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            let jobs = queue.list_jobs().await;
            if let Some(job) = jobs.first() {
                if job.state == IndexJobState::Failed {
                    break;
                }
            }
        }

        // The stamp itself lands on a separate task (Fix C) — wait for it
        // before ticking again, or the second tick could race a
        // not-yet-stamped record and wrongly consider it still due.
        wait_for_last_refreshed_stamp(&scheduler, "src-retry").await;

        // A second tick immediately after the failure must not resubmit:
        // the interval (1 hour) has not elapsed since `last_refreshed` was
        // stamped at completion.
        scheduler.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let jobs = queue.list_jobs().await;
        assert_eq!(
            jobs.len(),
            1,
            "a failed job must not be resubmitted before a full interval has elapsed"
        );
    }

    /// Issue #218-followups Fix C: a cancelled refresh job must not be
    /// resubmitted on the tick immediately following its cancellation —
    /// before this fix, `last_refreshed` was stamped as the tail
    /// expression of the job's own submitted closure, which
    /// `job_queue::process_job` drops entirely (along with everything else
    /// still pending inside it) when it `handle.abort()`s a cancelled
    /// task. A cancelled refresh was therefore never stamped and got
    /// resubmitted on the very next tick — undoing the backoff the
    /// cancellation was supposed to buy. Constructed deterministically: a
    /// blocker job on a *different* store occupies the queue's sole worker
    /// (mirrors `job_queue::tests::cancellation`'s pending-cancel test)
    /// so the scheduler's own tick-submitted refresh job is guaranteed to
    /// still be `Pending` when this test cancels it.
    #[tokio::test]
    async fn cancelled_refresh_job_is_not_resubmitted_on_the_immediately_following_tick() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        scheduler
            .register(
                "src-cancel".to_string(),
                "store-C".to_string(),
                "https://example.com/".to_string(),
                Some(3600), // 1 hour — long enough that only the cancel-stamp, not a real elapsed interval, could suppress resubmission.
            )
            .await;

        // Occupy the queue's sole worker with an unrelated job parked on a
        // gate this test controls, so the scheduler's own submission
        // (below) is guaranteed to still be Pending when this test cancels
        // it.
        let (blocker_release_tx, blocker_release_rx) = tokio::sync::oneshot::channel::<()>();
        queue
            .submit(
                "blocker-store",
                IndexJobScope::Store,
                move |_progress| async move {
                    let _ = blocker_release_rx.await;
                    Ok(localdb_core::IndexJobStats::default())
                },
            )
            .await
            .unwrap();

        scheduler.tick().await;

        let jobs = queue.list_jobs().await;
        let refresh_job = jobs
            .iter()
            .find(|j| j.store_id == "store-C")
            .expect("tick() should have submitted the refresh job")
            .clone();
        assert_eq!(
            refresh_job.state,
            IndexJobState::Pending,
            "the refresh job must still be queued behind the blocker job"
        );

        queue.cancel(&refresh_job.id).await.unwrap();

        // Release the blocker so the worker can move on to (not-)running
        // the refresh job.
        let _ = blocker_release_tx.send(());

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("cancelled refresh job did not reach a terminal state in time");
            }
            let job = queue.get_job(&refresh_job.id).await.unwrap();
            if job.state == IndexJobState::Failed {
                assert_eq!(job.error_code.as_deref(), Some("job_cancelled"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The stamp lands on a separate task (Fix C) — wait for it before
        // ticking again, or the next tick could race a not-yet-stamped
        // record and wrongly consider it still due.
        wait_for_last_refreshed_stamp(&scheduler, "src-cancel").await;

        // A tick immediately after the cancellation must not resubmit: the
        // interval (1 hour) has not elapsed since `last_refreshed` was
        // stamped at cancellation — exactly the same backoff an ordinary
        // failure gets
        // (`failed_job_is_not_resubmitted_before_a_full_interval_elapses`
        // above), now also honored for cancellation.
        scheduler.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let jobs = queue.list_jobs().await;
        let refresh_jobs_for_source = jobs.iter().filter(|j| j.store_id == "store-C").count();
        assert_eq!(
            refresh_jobs_for_source, 1,
            "a cancelled refresh job must not be resubmitted before a full interval has elapsed"
        );
    }

    /// The real regression test for #187 §1 on the scheduler path: with a
    /// real `AppState` attached (real store, real path source with content,
    /// fake embedder), `tick()` on a due source must produce genuine,
    /// nonzero stats — not the old stub's `IndexJobStats::default()`.
    #[tokio::test]
    async fn tick_with_attached_state_runs_real_ingestion_and_produces_nonzero_stats() {
        let content_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            content_dir.path().join("doc.md"),
            "rust programming language performance tips",
        )
        .unwrap();

        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        let mut yaml_config = localdb_core::config::schema::RawConfig::default();
        yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
            provider: "fake".to_string(),
            model: "default".to_string(),
        };
        let state_dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            yaml_config,
            state_dir.path().to_path_buf(),
            state_dir.path().join("models"),
            queue.clone(),
            scheduler.clone(),
        )
        .await
        .unwrap();

        state.add_store("notes", "private").await.unwrap();
        let source = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": content_dir.path().to_string_lossy()}),
                "prose",
                None,
            )
            .await
            .unwrap();

        scheduler.attach_state(state.clone()).await;
        // The scheduler's own bookkeeping doesn't care about source *kind* —
        // real ingestion re-reads the persisted `SourceRow` (a `path`
        // source here) via `job_exec::run_job`, not this record's `url`.
        scheduler
            .register(
                source.id.clone(),
                "notes".to_string(),
                "https://example.com".to_string(),
                Some(0),
            )
            .await;

        scheduler.tick().await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("refresh job did not complete within timeout");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            let jobs = queue.list_jobs().await;
            if let Some(job) = jobs.first() {
                if job.state == IndexJobState::Failed {
                    panic!("refresh job failed: {:?}", job.error);
                }
                if job.state == IndexJobState::Done {
                    assert!(
                        job.stats.docs_indexed > 0,
                        "expected nonzero docs_indexed, got {:?}",
                        job.stats
                    );
                    assert!(
                        job.stats.chunks_written > 0,
                        "expected nonzero chunks_written, got {:?}",
                        job.stats
                    );
                    // #187 review F1: `last_refreshed` must be recorded
                    // once the job actually completes, not back when it was
                    // merely submitted. The stamp itself lands on a
                    // separate task (issue #218-followups Fix C), so poll
                    // for it rather than asserting it's already visible the
                    // instant this loop observes `Done`.
                    wait_for_last_refreshed_stamp(&scheduler, &source.id).await;
                    break;
                }
            }
        }
    }

    /// Codex review finding G1 (issue #187): the scheduler's refresh closure
    /// used to call `state.get_or_build_embedder` unconditionally before
    /// `run_job` ran. For `IndexJobScope::Source`, an unresolvable source
    /// (e.g. deleted since it was registered for refresh) makes
    /// `resolve_job_sources` return `Err(SourceNotFound)` rather than an
    /// empty list — but under the old ordering that error surfaced only
    /// *after* an embedder had already been built and thrown away. Registers
    /// a refresh record for a source id that was never actually added to the
    /// store, ticks, and asserts the job fails with that source unresolved
    /// while the daemon's embedder cache is never built.
    #[tokio::test]
    async fn tick_with_unresolvable_source_never_builds_embedder() {
        let queue = JobQueue::new();
        let scheduler = UrlRefreshScheduler::new(queue.clone());

        let mut yaml_config = localdb_core::config::schema::RawConfig::default();
        yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
            provider: "fake".to_string(),
            model: "default".to_string(),
        };
        let state_dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            yaml_config,
            state_dir.path().to_path_buf(),
            state_dir.path().join("models"),
            queue.clone(),
            scheduler.clone(),
        )
        .await
        .unwrap();

        state.add_store("notes", "private").await.unwrap();
        scheduler.attach_state(state.clone()).await;
        // No source was ever added to "notes" — this id resolves to nothing.
        scheduler
            .register(
                "missing-source".to_string(),
                "notes".to_string(),
                "https://example.com".to_string(),
                Some(0),
            )
            .await;

        scheduler.tick().await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("refresh job did not reach a terminal state in time");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            let jobs = queue.list_jobs().await;
            if let Some(job) = jobs.first() {
                if job.state == IndexJobState::Done {
                    panic!("job with an unresolvable source must not report Done: {job:?}");
                }
                if job.state == IndexJobState::Failed {
                    assert!(
                        job.error
                            .as_deref()
                            .is_some_and(|e| e.contains("missing-source")),
                        "expected a source-not-found error naming the missing source, got: {:?}",
                        job.error
                    );
                    break;
                }
            }
        }

        assert_eq!(
            state.embedder_build_count(),
            0,
            "an unresolvable source scope must never trigger an embedder build"
        );
    }
}
