//! `AppState::new`/`from_backend` construction tests.

use std::sync::Arc;

use localdb_core::config::policy::compute_policy_version;
use localdb_core::config::schema::RawConfig;
use localdb_core::{store_factory, StoreBackend, StoreBackendConfig, StoreVisibility};
use store_libsql::SqliteBackend;

use crate::job_queue::JobQueue;
use crate::scheduler::UrlRefreshScheduler;
use crate::state::AppState;

use super::common::make_state;

#[tokio::test]
async fn models_dir_returns_the_value_it_was_given() {
    let (dir, state) = make_state().await;
    assert_eq!(state.models_dir(), dir.path().join("models"));
}

// --- from_backend ---------------------------------------------------------

fn fake_yaml_config() -> RawConfig {
    let mut yaml_config = RawConfig::default();
    yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    yaml_config
}

/// `from_backend` must derive the exact same `default_indexing_policy` /
/// `default_policy_version` as `new` — both are pure functions of
/// `yaml_config`, so a store added via either constructor must land on
/// the same policy version.
#[tokio::test]
async fn from_backend_derives_same_default_policy_version_as_new() {
    let dir = tempfile::tempdir().unwrap();
    let yaml_config = fake_yaml_config();

    let (dim, encoding) = embed::infer_dim_encoding(
        &yaml_config.defaults.indexing.embedding,
        &yaml_config.providers,
    )
    .unwrap();
    let db_path = dir.path().join("localdb.db");
    let config = StoreBackendConfig::local_path(db_path, dim, encoding);
    let backend = Arc::new(SqliteBackend::open(config).await.unwrap()) as Arc<dyn StoreBackend>;

    let queue = JobQueue::new();
    let state = AppState::from_backend(
        yaml_config.clone(),
        dir.path().to_path_buf(),
        dir.path().join("models"),
        backend,
        queue.clone(),
        UrlRefreshScheduler::new(queue),
    );

    state.add_store("notes", "private").await.unwrap();
    let row = state
        .backend()
        .get_store_by_name("notes")
        .await
        .unwrap()
        .unwrap();
    let expected_version = compute_policy_version(&yaml_config.defaults.indexing);
    assert_eq!(row.policy_version, expected_version);
}

/// `from_backend` must operate on the exact backend handle it was given
/// — not open a fresh connection of its own — so a store added through
/// the caller's own already-open handle is immediately visible through
/// the resulting `AppState`, and vice versa.
#[tokio::test]
async fn from_backend_shares_the_given_backend_handle() {
    let dir = tempfile::tempdir().unwrap();
    let yaml_config = fake_yaml_config();

    let (dim, encoding) = embed::infer_dim_encoding(
        &yaml_config.defaults.indexing.embedding,
        &yaml_config.providers,
    )
    .unwrap();
    let db_path = dir.path().join("localdb.db");
    let config = StoreBackendConfig::local_path(db_path, dim, encoding);
    let backend = Arc::new(SqliteBackend::open(config).await.unwrap()) as Arc<dyn StoreBackend>;

    // Add a store directly via the caller's own handle, before the
    // `AppState` even exists.
    let row = store_factory::default_store_row(
        "pre-existing",
        StoreVisibility::Private,
        &yaml_config.defaults.indexing,
        &compute_policy_version(&yaml_config.defaults.indexing),
    )
    .unwrap();
    backend.upsert_store(&row).await.unwrap();

    let queue = JobQueue::new();
    let state = AppState::from_backend(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        backend,
        queue.clone(),
        UrlRefreshScheduler::new(queue),
    );

    let effective = state.effective_config().await.unwrap();
    assert_eq!(effective.stores.len(), 1);
    assert_eq!(effective.stores[0].name, "pre-existing");
}
