//! Shared test helpers for `daemon` tests.

use std::path::Path;

use tempfile::TempDir;

use localdb_core::config::{loader::ResolvedPaths, schema::RawConfig};

use crate::state::AppState;

pub(in crate::daemon::tests) async fn make_state() -> (TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut yaml_config = RawConfig::default();
    yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    let queue = crate::job_queue::JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
    )
    .await
    .unwrap();
    (dir, state)
}

pub(in crate::daemon::tests) fn make_resolved_paths(dir: &Path) -> ResolvedPaths {
    ResolvedPaths {
        config_file: dir.join("config.yaml"),
        data_dir: dir.join("data"),
        models_dir: dir.join("models"),
        logs_dir: dir.join("logs"),
    }
}
