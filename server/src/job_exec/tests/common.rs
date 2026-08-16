//! Shared test helpers for job_exec tests.

use localdb_core::config::schema::{
    DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, RawConfig,
};
use tempfile::TempDir;

use crate::state::AppState;

pub(in crate::job_exec) fn fake_yaml() -> RawConfig {
    RawConfig {
        defaults: DefaultsConfig {
            indexing: IndexingPolicyConfig {
                chunking: Default::default(),
                embedding: EmbeddingPolicy {
                    provider: "fake".to_string(),
                    model: "default".to_string(),
                },
                ..Default::default()
            },
        },
        ..Default::default()
    }
}

/// Real backend + state, wired exactly like `AppState::new` (fake
/// embedder, no network/model download) — mirrors
/// `server/src/handlers/tests/common.rs::make_state_with_fake_config`,
/// duplicated here rather than shared because that helper is private to
/// the `handlers::tests` module tree.
pub(in crate::job_exec) async fn test_state() -> (TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let queue = crate::job_queue::JobQueue::new();
    let state = AppState::new(
        fake_yaml(),
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
    )
    .await
    .unwrap();
    (dir, state)
}
