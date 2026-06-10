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
use tracing::{debug, info};

use localdb_core::IndexJobScope;

use crate::job_queue::JobQueue;

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
}

impl UrlRefreshScheduler {
    /// Create a new scheduler backed by the given job queue.
    pub fn new(queue: JobQueue) -> Self {
        Self {
            records: Arc::new(RwLock::new(HashMap::new())),
            queue,
        }
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

            self.queue
                .submit(
                    &store_name_for_submit,
                    IndexJobScope::Source {
                        source_id: source_id.clone(),
                    },
                    move || {
                        // In production this would call the URL fetcher + ingestion pipeline.
                        // For MVP the job submission itself is the deliverable;
                        // the actual HTTP fetch + index is done by the T07 pipeline
                        // when it is wired in.
                        debug!(
                            "URL refresh job running for source '{}' ({})",
                            source_id_for_closure, store_name_for_closure
                        );
                        Ok(localdb_core::IndexJobStats::default())
                    },
                )
                .await;

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

    #[tokio::test]
    async fn submitted_job_eventually_completes() {
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
                panic!("refresh job did not complete within timeout");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            let jobs = queue.list_jobs().await;
            if let Some(job) = jobs.first() {
                if job.state == IndexJobState::Done {
                    break;
                }
            }
        }
    }
}
