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

use localdb_core::{Error, IndexJobScope, IndexJobStats, ProgressSink};

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
    /// watcher has not yet stamped `last_refreshed`. `tick` skips sources with
    /// this set: `last_refreshed` is
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

impl UrlRefreshRecord {
    /// Whether this source is due for a refresh at `now`.
    ///
    /// A source is due when it has an `interval` configured AND either it has
    /// never been refreshed or `now - last_refreshed >= interval`.
    ///
    /// A source whose previous refresh hasn't been stamped yet
    /// (`refresh_inflight`) is never due, regardless of its timestamp: the
    /// stamp lands after the job queue's in-flight guard is already released,
    /// so the guard alone can't suppress a resubmit in that window (see the
    /// field's doc comment).
    fn is_due(&self, now: Instant) -> bool {
        if self.refresh_inflight {
            return false;
        }
        let Some(interval) = self.interval else {
            return false;
        };
        match self.last_refreshed {
            None => true,
            Some(last) => now.duration_since(last) >= interval,
        }
    }
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
    /// fabricating success (see [`run_refresh_job`]).
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

    /// Check all registered sources and submit refresh jobs for those that
    /// are due (see [`UrlRefreshRecord::is_due`]); each submitted job runs
    /// [`run_refresh_job`].
    pub async fn tick(&self) {
        for record in self.due_records(Instant::now()).await {
            info!(
                "URL refresh due for source '{}' ({}), submitting job",
                record.source_id, record.url
            );

            let state = self.state.read().await.clone();
            let store_name = record.store_name.clone();
            let source_id = record.source_id.clone();
            let submit_result = self
                .queue
                .submit(
                    &record.store_name,
                    IndexJobScope::Source {
                        source_id: record.source_id.clone(),
                    },
                    move |progress| run_refresh_job(state, store_name, source_id, progress),
                )
                .await;

            match submit_result {
                Ok(job) => {
                    self.mark_inflight(&record.source_id).await;
                    self.spawn_stamp_watcher(record.source_id.clone(), job.id.clone());
                }
                Err(e) => log_submit_failure(&record.source_id, &e),
            }
        }
    }

    /// Snapshot of all registered sources due for a refresh at `now` (see
    /// [`UrlRefreshRecord::is_due`]).
    async fn due_records(&self, now: Instant) -> Vec<UrlRefreshRecord> {
        let records = self.records.read().await;
        records
            .values()
            .filter(|r| r.is_due(now))
            .cloned()
            .collect()
    }

    /// Suppress `source_id` from due-checks until its stamp watcher clears
    /// the flag.
    ///
    /// Runs right after a successful submit, before the watcher exists to
    /// clear it: from submit until the watcher stamps, `tick` must not
    /// consider the source due — the queue's own in-flight guard stops
    /// covering it the moment `process_job` finishes, which can be before
    /// the watcher ever runs. Serial `tick`s (one `run` loop) mean no
    /// due-check can interleave between the submit and this write.
    async fn mark_inflight(&self, source_id: &str) {
        let mut records = self.records.write().await;
        if let Some(r) = records.get_mut(source_id) {
            r.refresh_inflight = true;
        }
    }

    /// Spawn a detached task that waits for `job_id` to reach a terminal
    /// state, then stamps `last_refreshed` and clears `refresh_inflight` in
    /// one write.
    ///
    /// The stamp lives here, entirely outside the job's own submitted
    /// future: a cancelled job's future is `handle.abort()`ed by
    /// `job_queue::process_job`, which drops everything still pending inside
    /// it — a stamp there would never run for a cancelled refresh, which
    /// would then be resubmitted on the very next tick, silently undoing the
    /// backoff a cancellation is supposed to buy. Watching from a separate
    /// task instead observes the registry's terminal write (`process_job`
    /// commits it before tearing down the progress channel — see
    /// `HandleRegistry`'s doc comment in `job_queue.rs`) regardless of *how*
    /// the job got there: normal completion, a real failure, and
    /// cancellation all stamp the same way.
    fn spawn_stamp_watcher(&self, source_id: String, job_id: String) {
        let queue = self.queue.clone();
        let records = self.records.clone();
        tokio::spawn(async move {
            wait_for_job_terminal(&queue, &job_id).await;
            // Record completion time now, not at submit time: stamping at
            // submit makes a slow job look "refreshed" while it is still
            // running, drifting scheduling away from actual completion. A
            // failed (or cancelled) job is stamped too — it must never
            // tight-loop retrying; it waits out a full interval just like a
            // successful refresh does. Only touch the record if it's still
            // registered: the source may have been unregistered mid-flight,
            // and completion must never re-insert a removed record. Clearing
            // `refresh_inflight` in the same write as the stamp means `tick`
            // always sees either "suppressed" or "freshly stamped", never
            // the stale-timestamp gap between them.
            let mut records = records.write().await;
            if let Some(r) = records.get_mut(&source_id) {
                r.last_refreshed = Some(Instant::now());
                r.refresh_inflight = false;
            }
        });
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

/// Run one scheduled refresh: real ingestion via `AppState::run_scoped_job`,
/// scoped to just `source_id`.
///
/// Shared with `handlers::jobs::create_job` via `AppState::run_scoped_job`:
/// resolves the scoped source before deciding whether to build/reuse an
/// embedder — a scope that fails to resolve (e.g. the source was deleted)
/// surfaces that error before paying for a (potentially huge) embedding
/// model build, and a resolved-but-empty scope never builds one at all. Only
/// the deletion policy differs between the two callers: a scheduled refresh
/// always uses `Retain` — it never prunes documents on its own; that stays
/// an explicit, opt-in CLI/HTTP action (issues #156/#185).
///
/// `state` is `None` until `attach_state` runs; the job then fails with a
/// clear error rather than fabricating success.
async fn run_refresh_job(
    state: Option<AppState>,
    store_name: String,
    source_id: String,
    progress: ProgressSink,
) -> Result<IndexJobStats, Error> {
    debug!(
        "URL refresh job running for source '{}' ({})",
        source_id, store_name
    );

    let state = state.ok_or_else(|| Error::Internal {
        message: "URL refresh scheduler has no state attached".to_string(),
        correlation_id: "url_refresh_no_state".to_string(),
    })?;
    let store_row = state
        .backend()
        .get_store_by_name(&store_name)
        .await?
        .ok_or_else(|| Error::StoreNotFound {
            id: store_name.clone(),
        })?;
    let refresh_scope = IndexJobScope::Source { source_id };
    state
        .run_scoped_job(
            &store_row,
            refresh_scope,
            localdb_core::DeletionPolicy::Retain,
            false,
            progress,
        )
        .await
}

/// Wait until `job_id` reaches a terminal state, observed via its
/// progress-event channel closing — mirrors
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
mod tests;
