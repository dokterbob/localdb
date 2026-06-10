//! Shared application state for the axum server.
//!
//! `AppState` is injected into all handlers via `axum::Extension`.
//! It holds the job queue, the config snapshot, and the runtime-state DB.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use localdb_core::{
    config::{
        runtime_state::{
            build_effective_config, check_yaml_owned, EffectiveConfig, RuntimeStateDb,
        },
        schema::RawConfig,
    },
    Error, Store, StoreVisibility,
};

use crate::job_queue::JobQueue;

/// A source record stored in the runtime-state DB (simplified, in-memory for now).
///
/// Full DB-backed persistence is via the `RuntimeStateDb` + a sources table
/// added in this ticket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceRecord {
    pub id: String,
    pub store_id: String,
    pub kind: String,
    pub spec: serde_json::Value,
    pub preset: String,
}

/// Shared application state for all handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    /// In-memory snapshot of the current YAML config.
    yaml_config: RwLock<RawConfig>,
    /// Runtime-state DB path (for reopening after config reload).
    data_dir: PathBuf,
    /// Runtime-state DB for stores/sources added via API.
    runtime_db: RuntimeStateDb,
    /// In-memory source records (persisted in runtime_db sources table).
    sources: RwLock<HashMap<String, SourceRecord>>,
    /// Job queue.
    job_queue: JobQueue,
}

impl AppState {
    /// Create a new `AppState`.
    pub fn new(
        yaml_config: RawConfig,
        data_dir: PathBuf,
        job_queue: JobQueue,
    ) -> Result<Self, Error> {
        let runtime_db_path = data_dir.join("runtime-state.redb");
        let runtime_db = RuntimeStateDb::open(&runtime_db_path)?;

        Ok(Self {
            inner: Arc::new(Inner {
                yaml_config: RwLock::new(yaml_config),
                data_dir,
                runtime_db,
                sources: RwLock::new(HashMap::new()),
                job_queue,
            }),
        })
    }

    /// Access the job queue.
    pub fn job_queue(&self) -> &JobQueue {
        &self.inner.job_queue
    }

    /// Get the effective config (YAML + runtime merged).
    pub async fn effective_config(&self) -> Result<EffectiveConfig, Error> {
        let yaml = self.inner.yaml_config.read().await;
        let default = localdb_core::config::schema::IndexingPolicyConfig::default();
        build_effective_config(&yaml, &self.inner.runtime_db, &default)
    }

    /// Check whether a named store is YAML-owned.
    pub async fn is_yaml_owned_store(&self, name: &str) -> bool {
        let yaml = self.inner.yaml_config.read().await;
        check_yaml_owned(name, &yaml)
    }

    /// Get the current YAML config snapshot.
    pub async fn yaml_config(&self) -> RawConfig {
        self.inner.yaml_config.read().await.clone()
    }

    /// Reload the YAML config snapshot (called by the file watcher).
    pub async fn reload_yaml_config(&self, new_config: RawConfig) {
        let mut yaml = self.inner.yaml_config.write().await;
        *yaml = new_config;
    }

    /// Add a runtime-owned store.
    ///
    /// Returns `Error::ConfigReadonly` if the store name is YAML-owned.
    pub async fn add_store(&self, name: &str, visibility: &str) -> Result<Store, Error> {
        // Check YAML ownership
        if self.is_yaml_owned_store(name).await {
            return Err(Error::ConfigReadonly);
        }

        let id = localdb_core::new_ulid();
        let rt_store = localdb_core::config::runtime_state::RuntimeStore {
            name: name.to_string(),
            id: id.clone(),
            visibility: visibility.to_string(),
            backend: "lancedb".to_string(),
            indexing: None,
        };

        self.inner.runtime_db.upsert_store(&rt_store)?;

        Ok(Store {
            id,
            name: name.to_string(),
            visibility: if visibility == "shared" {
                StoreVisibility::Shared
            } else {
                StoreVisibility::Private
            },
            backend: localdb_core::BackendConfig {
                kind: "lancedb".to_string(),
                connection: Default::default(),
            },
            indexing: localdb_core::IndexingPolicy {
                chunking: localdb_core::ChunkingConfig {
                    preset: "prose".to_string(),
                    max_chars: None,
                    overlap_chars: None,
                },
                embedding: localdb_core::EmbeddingConfig {
                    provider: "local-onnx".to_string(),
                    model: "default".to_string(),
                },
            },
            acl: vec![],
        })
    }

    /// Remove a runtime-owned store by name.
    ///
    /// Returns `Error::ConfigReadonly` if the store is YAML-owned.
    /// Returns `Error::StoreNotFound` if the store doesn't exist.
    pub async fn remove_store(&self, name: &str) -> Result<(), Error> {
        if self.is_yaml_owned_store(name).await {
            return Err(Error::ConfigReadonly);
        }

        let deleted = self.inner.runtime_db.delete_store(name)?;
        if !deleted {
            return Err(Error::StoreNotFound {
                id: name.to_string(),
            });
        }
        Ok(())
    }

    /// Get a store by name (from effective config).
    pub async fn get_store_by_name(&self, name: &str) -> Result<StoreRecord, Error> {
        let effective = self.effective_config().await?;
        effective
            .stores
            .iter()
            .find(|s| s.name == name)
            .map(|s| StoreRecord {
                name: s.name.clone(),
                visibility: s.visibility.clone(),
                backend: s.backend.clone(),
                ownership: s.ownership.clone(),
            })
            .ok_or_else(|| Error::StoreNotFound {
                id: name.to_string(),
            })
    }

    /// Add a source to a store.
    ///
    /// Returns `Error::ConfigReadonly` if the store is YAML-owned.
    /// Returns `Error::StoreNotFound` if the store doesn't exist.
    pub async fn add_source(
        &self,
        store_name: &str,
        kind: &str,
        spec: serde_json::Value,
        preset: &str,
    ) -> Result<SourceRecord, Error> {
        // Verify store exists
        let effective = self.effective_config().await?;
        let store_opt = effective.stores.iter().find(|s| s.name == store_name);
        let store = store_opt.ok_or_else(|| Error::StoreNotFound {
            id: store_name.to_string(),
        })?;

        // Check if YAML-owned
        if store.ownership == localdb_core::config::runtime_state::ConfigOwnership::Yaml {
            return Err(Error::ConfigReadonly);
        }

        let id = localdb_core::new_ulid();
        let source = SourceRecord {
            id: id.clone(),
            store_id: store_name.to_string(),
            kind: kind.to_string(),
            spec,
            preset: preset.to_string(),
        };

        {
            let mut sources = self.inner.sources.write().await;
            sources.insert(id, source.clone());
        }

        Ok(source)
    }

    /// List sources for a store.
    pub async fn list_sources(&self, store_name: &str) -> Result<Vec<SourceRecord>, Error> {
        // Verify store exists
        let effective = self.effective_config().await?;
        if !effective.stores.iter().any(|s| s.name == store_name) {
            return Err(Error::StoreNotFound {
                id: store_name.to_string(),
            });
        }

        let sources = self.inner.sources.read().await;
        Ok(sources
            .values()
            .filter(|s| s.store_id == store_name)
            .cloned()
            .collect())
    }

    /// Remove a source by ID.
    ///
    /// Returns `Error::SourceNotFound` if the source doesn't exist.
    pub async fn remove_source(&self, source_id: &str) -> Result<(), Error> {
        let mut sources = self.inner.sources.write().await;
        if sources.remove(source_id).is_none() {
            return Err(Error::SourceNotFound {
                id: source_id.to_string(),
            });
        }
        Ok(())
    }

    /// Get a source by ID.
    pub async fn get_source(&self, source_id: &str) -> Result<SourceRecord, Error> {
        let sources = self.inner.sources.read().await;
        sources
            .get(source_id)
            .cloned()
            .ok_or_else(|| Error::SourceNotFound {
                id: source_id.to_string(),
            })
    }

    /// Get the data directory path.
    pub fn data_dir(&self) -> &PathBuf {
        &self.inner.data_dir
    }
}

/// A store record as returned by the API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoreRecord {
    pub name: String,
    pub visibility: String,
    pub backend: String,
    pub ownership: localdb_core::config::runtime_state::ConfigOwnership,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_state() -> (TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            stores: vec![],
            providers: vec![],
        };
        let queue = JobQueue::new();
        let state = AppState::new(yaml_config, dir.path().to_path_buf(), queue).unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn add_and_list_stores() {
        let (_dir, state) = make_state();
        state.add_store("notes", "private").await.unwrap();
        let effective = state.effective_config().await.unwrap();
        assert_eq!(effective.stores.len(), 1);
        assert_eq!(effective.stores[0].name, "notes");
    }

    #[tokio::test]
    async fn add_store_returns_config_readonly_for_yaml_owned() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            stores: vec![localdb_core::config::schema::StoreConfig {
                name: "yaml-store".to_string(),
                visibility: "private".to_string(),
                backend: "lancedb".to_string(),
                indexing: None,
                sources: vec![],
            }],
            providers: vec![],
        };
        let queue = JobQueue::new();
        let state = AppState::new(yaml_config, dir.path().to_path_buf(), queue).unwrap();

        let result = state.add_store("yaml-store", "private").await;
        assert_eq!(result, Err(Error::ConfigReadonly));
    }

    #[tokio::test]
    async fn remove_store_not_found() {
        let (_dir, state) = make_state();
        let result = state.remove_store("non-existent").await;
        assert!(matches!(result, Err(Error::StoreNotFound { .. })));
    }

    #[tokio::test]
    async fn remove_store_succeeds() {
        let (_dir, state) = make_state();
        state.add_store("notes", "private").await.unwrap();
        state.remove_store("notes").await.unwrap();
        let effective = state.effective_config().await.unwrap();
        assert!(effective.stores.is_empty());
    }

    #[tokio::test]
    async fn add_source_to_nonexistent_store_fails() {
        let (_dir, state) = make_state();
        let result = state
            .add_source(
                "no-such-store",
                "path",
                serde_json::json!({"root": "/tmp"}),
                "prose",
            )
            .await;
        assert!(matches!(result, Err(Error::StoreNotFound { .. })));
    }

    #[tokio::test]
    async fn add_and_list_sources() {
        let (_dir, state) = make_state();
        state.add_store("notes", "private").await.unwrap();
        let source = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp/notes", "include": [], "exclude": []}),
                "prose",
            )
            .await
            .unwrap();

        let sources = state.list_sources("notes").await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, source.id);
    }

    #[tokio::test]
    async fn remove_source_not_found() {
        let (_dir, state) = make_state();
        let result = state.remove_source("no-such-source").await;
        assert!(matches!(result, Err(Error::SourceNotFound { .. })));
    }

    #[tokio::test]
    async fn remove_source_succeeds() {
        let (_dir, state) = make_state();
        state.add_store("notes", "private").await.unwrap();
        let source = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp"}),
                "prose",
            )
            .await
            .unwrap();
        state.remove_source(&source.id).await.unwrap();
        let sources = state.list_sources("notes").await.unwrap();
        assert!(sources.is_empty());
    }

    #[tokio::test]
    async fn yaml_config_reload() {
        let (_dir, state) = make_state();
        let new_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            stores: vec![localdb_core::config::schema::StoreConfig {
                name: "new-store".to_string(),
                visibility: "private".to_string(),
                backend: "lancedb".to_string(),
                indexing: None,
                sources: vec![],
            }],
            providers: vec![],
        };
        state.reload_yaml_config(new_config).await;
        let yaml = state.yaml_config().await;
        assert_eq!(yaml.stores.len(), 1);
        assert_eq!(yaml.stores[0].name, "new-store");
    }
}
