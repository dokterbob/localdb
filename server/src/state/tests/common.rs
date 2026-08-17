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

impl AppState {
    pub(in crate::state::tests) async fn scheduler_source_count(&self) -> usize {
        self.inner.url_scheduler.source_count().await
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
