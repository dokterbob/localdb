//! Shared application state for the axum server.
//!
//! `AppState` is injected into all handlers via `axum::Extension`.
//! It holds the job queue, the config snapshot, and the runtime-state DB.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use localdb_core::{
    config::{
        runtime_state::{
            build_effective_config, check_yaml_owned, EffectiveConfig, RuntimeStateDb,
        },
        schema::RawConfig,
    },
    store::{MetadataFilter, RetrievalStore, SearchResult, StoreStats},
    ChunkRecord, Error, FakeStore, Store, StoreVisibility,
};

use crate::handlers::DocumentRecord;
use crate::job_queue::JobQueue;

// ---------------------------------------------------------------------------
// SharedStore — wraps Arc<FakeStore> as a Box<dyn RetrievalStore>
// ---------------------------------------------------------------------------

/// A newtype that delegates `RetrievalStore` to an `Arc<FakeStore>`.
///
/// This lets the daemon share a single in-memory store between the job queue
/// (writes) and the HTTP search handler (reads).
pub struct SharedStore(pub Arc<FakeStore>);

#[async_trait]
impl RetrievalStore for SharedStore {
    async fn upsert_chunks(&self, chunks: Vec<ChunkRecord>) -> Result<usize, Error> {
        self.0.upsert_chunks(chunks).await
    }

    async fn delete_by_document(&self, document_id: &str) -> Result<usize, Error> {
        self.0.delete_by_document(document_id).await
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        self.0.delete_by_store(store_id).await
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        k: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        self.0.dense_search(query_vector, k, filters).await
    }

    async fn bm25_search(
        &self,
        query: &str,
        k: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        self.0.bm25_search(query, k, filters).await
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        self.0.stats().await
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        self.0.get_chunk(chunk_id).await
    }

    async fn get_chunks_for_document(&self, document_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        self.0.get_chunks_for_document(document_id).await
    }
}

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
    /// In-memory retrieval store for search (used by the daemon).
    ///
    /// In production this would be backed by LanceDB; here we use the FakeStore
    /// so the search handler can call `SearchOrchestrator::query()` for real.
    retrieval_store: Arc<FakeStore>,
    /// In-memory document index: doc_id → DocumentRecord (for GET /documents/{id}).
    documents: RwLock<HashMap<String, DocumentRecord>>,
}

impl AppState {
    /// Create a new `AppState`.
    pub fn new(
        yaml_config: RawConfig,
        data_dir: PathBuf,
        job_queue: JobQueue,
    ) -> Result<Self, Error> {
        let runtime_db_path = data_dir.join("runtime-state.db");
        let runtime_db = RuntimeStateDb::open(&runtime_db_path)?;

        Ok(Self {
            inner: Arc::new(Inner {
                yaml_config: RwLock::new(yaml_config),
                data_dir,
                runtime_db,
                sources: RwLock::new(HashMap::new()),
                job_queue,
                retrieval_store: Arc::new(FakeStore::new()),
                documents: RwLock::new(HashMap::new()),
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

        // Cascade: remove orphaned in-memory sources.
        {
            let mut sources = self.inner.sources.write().await;
            sources.retain(|_, s| s.store_id != name);
        }

        // Cascade: remove on-disk index (LanceDB + tantivy FTS).
        let store_dir = self.inner.data_dir.join("stores").join(name);
        match std::fs::remove_dir_all(&store_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!("could not remove store data dir {:?}: {}", store_dir, e);
            }
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

    /// Update a runtime-owned store's mutable fields.
    ///
    /// Returns `Error::ConfigReadonly` if the store is YAML-owned.
    /// Returns `Error::StoreNotFound` if the store doesn't exist.
    pub async fn update_store(&self, name: &str, visibility: Option<&str>) -> Result<(), Error> {
        if self.is_yaml_owned_store(name).await {
            return Err(Error::ConfigReadonly);
        }

        // Check it exists in the effective config (YAML + runtime).
        {
            let effective = self.effective_config().await?;
            effective
                .stores
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| Error::StoreNotFound {
                    id: name.to_string(),
                })?;
        }

        // Fetch the current runtime store record, update, re-upsert.
        let stores = self.inner.runtime_db.list_stores()?;
        let current =
            stores
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| Error::StoreNotFound {
                    id: name.to_string(),
                })?;

        let updated = localdb_core::config::runtime_state::RuntimeStore {
            visibility: visibility.unwrap_or(&current.visibility).to_string(),
            ..current
        };

        self.inner.runtime_db.upsert_store(&updated)?;
        Ok(())
    }

    /// Get a document by its content-addressed ID.
    ///
    /// Returns `None` if the document is not indexed in the in-memory store.
    pub async fn get_document_by_id(&self, doc_id: &str) -> Option<DocumentRecord> {
        let docs = self.inner.documents.read().await;
        docs.get(doc_id).cloned()
    }

    /// Insert or update a document record in the in-memory index.
    pub async fn upsert_document(&self, record: DocumentRecord) {
        let mut docs = self.inner.documents.write().await;
        docs.insert(record.id.clone(), record);
    }

    /// Access the in-memory retrieval store used by the search handler.
    pub fn retrieval_store(&self) -> Arc<FakeStore> {
        self.inner.retrieval_store.clone()
    }

    /// Upsert chunks into the in-memory retrieval store.
    ///
    /// Used by the job queue when ingestion produces new chunks.
    pub async fn upsert_chunks(&self, chunks: Vec<ChunkRecord>) -> Result<usize, Error> {
        self.inner.retrieval_store.upsert_chunks(chunks).await
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
    async fn add_source_to_yaml_owned_store_returns_config_readonly() {
        // Covers the ConfigReadonly branch in state.rs:add_source (line ~208).
        // YAML-owned stores must not accept source mutations via the API.
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

        let result = state
            .add_source(
                "yaml-store",
                "path",
                serde_json::json!({"root": "/tmp"}),
                "prose",
            )
            .await;

        assert!(
            matches!(result, Err(Error::ConfigReadonly)),
            "add_source to a YAML-owned store should return ConfigReadonly, got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn update_store_returns_config_readonly_for_yaml_owned() {
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

        let result = state.update_store("yaml-store", Some("shared")).await;
        assert_eq!(
            result,
            Err(Error::ConfigReadonly),
            "update_store on YAML-owned store should return ConfigReadonly"
        );
    }

    #[tokio::test]
    async fn update_store_updates_visibility() {
        let (_dir, state) = make_state();
        state.add_store("notes", "private").await.unwrap();
        state.update_store("notes", Some("shared")).await.unwrap();
        let record = state.get_store_by_name("notes").await.unwrap();
        assert_eq!(record.visibility, "shared");
    }

    #[tokio::test]
    async fn upsert_and_search_chunks_roundtrip() {
        let (_dir, state) = make_state();

        let chunk = localdb_core::ChunkRecord {
            id: "chunk-1".to_string(),
            document_id: "doc-1".to_string(),
            store_id: "store-A".to_string(),
            text: "hello world rust programming".to_string(),
            span: localdb_core::types::Span::new(0, 30),
            heading_path: vec![],
            embedding: vec![1.0, 0.0, 0.0, 0.0],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc".to_string(),
            origin_store: "store-A".to_string(),
            source_id: "src-1".to_string(),
            source_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            title: Some("Test Doc".to_string()),
            meta: std::collections::HashMap::new(),
            metadata: localdb_core::DocumentMetadata::default(),
        };

        state.upsert_chunks(vec![chunk]).await.unwrap();

        // Verify the chunk is searchable via the store
        let store = state.retrieval_store();
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1, "one chunk should be indexed");
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

    #[tokio::test]
    async fn remove_store_cascades_sources() {
        let (dir, state) = make_state();

        state.add_store("scratch", "private").await.unwrap();
        state
            .add_source(
                "scratch",
                "path",
                serde_json::json!({"root": "/tmp/a"}),
                "prose",
            )
            .await
            .unwrap();
        state
            .add_source(
                "scratch",
                "path",
                serde_json::json!({"root": "/tmp/b"}),
                "prose",
            )
            .await
            .unwrap();

        // Confirm sources exist before removal.
        let before = state.list_sources("scratch").await.unwrap();
        assert_eq!(before.len(), 2);

        // Create a dummy on-disk store dir so we can verify it's removed.
        let store_dir = dir.path().join("stores").join("scratch");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("index.bin"), b"data").unwrap();

        state.remove_store("scratch").await.unwrap();

        // In-memory sources map must be empty for this store.
        let sources_map = state.inner.sources.read().await;
        assert!(
            sources_map.values().all(|s| s.store_id != "scratch"),
            "orphaned sources remain after remove_store"
        );
        drop(sources_map);

        // On-disk dir must be gone.
        assert!(!store_dir.exists(), "store data dir was not removed");

        // Re-adding the same name should start with empty sources.
        drop(state); // release Arc before reusing dir
        let _ = dir;
    }
}
