//! Shared test helpers for `state` tests.

use tempfile::TempDir;

use localdb_core::config::schema::RawConfig;

use crate::job_queue::JobQueue;
use crate::scheduler::UrlRefreshScheduler;
use crate::state::AppState;

pub(in crate::state::tests) async fn make_state() -> (TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut yaml_config = RawConfig::default();
    yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    let queue = JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue.clone(),
        UrlRefreshScheduler::new(queue),
    )
    .await
    .unwrap();
    (dir, state)
}

/// Like [`make_state`], but also attaches this state's own scheduler to
/// itself — the same two-step wiring `build_daemon_state` performs in the
/// real daemon — so a scheduler tick can run a refresh job through to
/// completion instead of failing with "no state attached".
pub(in crate::state::tests) async fn make_attached_state() -> (TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut yaml_config = RawConfig::default();
    yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue,
        scheduler.clone(),
    )
    .await
    .unwrap();
    scheduler.attach_state(state.clone()).await;
    (dir, state)
}

impl AppState {
    pub(in crate::state::tests) async fn scheduler_source_count(&self) -> usize {
        self.inner.url_scheduler.source_count().await
    }

    /// Drive one scheduler tick against this state's own `url_scheduler`,
    /// submitting refresh jobs for any due source through the same code
    /// path the daemon's background loop uses.
    pub(in crate::state::tests) async fn tick_scheduler(&self) {
        self.inner.url_scheduler.tick().await;
    }

    /// Number of times this `AppState`'s embedder cache has actually called
    /// `embed::create_embedder` (Codex review finding F2, issue #187). See
    /// `Inner::embedder_build_count`'s doc comment for why this is
    /// per-instance rather than a shared static.
    pub(crate) fn embedder_build_count(&self) -> usize {
        self.inner
            .embedder_build_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}
