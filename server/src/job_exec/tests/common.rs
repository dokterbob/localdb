//! Shared test helpers for job_exec tests.

use std::sync::Arc;

use localdb_core::config::schema::{
    DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, RawConfig,
};
use localdb_core::StoreBackend;
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

/// Like [`test_state`], but also hands back the concrete `SqliteBackend`
/// `AppState` otherwise stores type-erased behind `Arc<dyn StoreBackend>`.
///
/// A test that needs `SqliteBackend::set_last_checked_at_for_test` (feature
/// `test-support`, see `store-libsql/src/backend.rs`'s doc comment on that
/// method) — to backdate `resources.last_checked_at` past the recheck floor
/// between two `run_job` calls — has no way to reach it through
/// `AppState::backend()`'s `&dyn StoreBackend`, since `StoreBackend` has no
/// downcast path. Duplicates `AppState::new`'s embedding-shape derivation
/// (`embed::infer_dim_encoding`) and DB path convention
/// (`data_dir.join("localdb.db")`) because `AppState::new` builds and
/// discards its own `SqliteBackend` internally rather than handing it back.
pub(in crate::job_exec) async fn test_state_with_backend(
) -> (TempDir, AppState, Arc<store_libsql::SqliteBackend>) {
    let dir = tempfile::tempdir().unwrap();
    let queue = crate::job_queue::JobQueue::new();
    let yaml = fake_yaml();
    let embedding_policy = &yaml.defaults.indexing.embedding;
    let providers = &yaml.providers;
    let (dim, encoding) = embed::infer_dim_encoding(embedding_policy, providers).unwrap();
    let db_path = dir.path().join("localdb.db");
    let config = localdb_core::StoreBackendConfig::local_path(db_path, dim, encoding);
    let backend = Arc::new(store_libsql::SqliteBackend::open(config).await.unwrap());
    let state = AppState::from_backend(
        yaml,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        backend.clone() as Arc<dyn localdb_core::StoreBackend>,
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
    );
    (dir, state, backend)
}
