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

use localdb_core::IndexJobScope;

use crate::job_exec::{self, JobExecDeps};
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
                        let state = state_for_closure.ok_or_else(|| {
                            "URL refresh scheduler has no state attached".to_string()
                        })?;
                        let store_row = state
                            .backend()
                            .get_store_by_name(&store_name_for_closure)
                            .await
                            .map_err(|e| e.to_string())?
                            .ok_or_else(|| format!("store not found: {store_name_for_closure}"))?;
                        let yaml = state.yaml_config().await;
                        let deps = JobExecDeps {
                            backend: state.backend(),
                            yaml: &yaml,
                            models_dir: state.models_dir(),
                            embedder: None,
                            progress: Some(progress),
                        };
                        job_exec::run_job(
                            &store_row,
                            IndexJobScope::Source {
                                source_id: source_id_for_closure.clone(),
                            },
                            localdb_core::DeletionPolicy::Retain,
                            deps,
                        )
                        .await
                        .map(|(stats, _embedder)| stats)
                        .map_err(|e| e.to_string())
                    },
                )
                .await;

            if let Err(e) = submit_result {
                warn!(
                    "URL refresh scheduler: failed to submit job for source '{}': {}",
                    record.source_id, e
                );
                continue;
            }

            // Update last_refreshed timestamp.
            let mut records = self.records.write().await;
            if let Some(r) = records.get_mut(&record.source_id) {
                r.last_refreshed = Some(Instant::now());
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

        let mut yaml_config = localdb_core::config::schema::RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
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
                    break;
                }
            }
        }
    }
}
