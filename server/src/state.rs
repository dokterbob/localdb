use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use localdb_core::{
    config::{
        policy::compute_policy_version,
        schema::{IndexingPolicyConfig, RawConfig},
    },
    ingestion::now_rfc3339,
    Error, SourceRow, Store, StoreBackend, StoreBackendConfig, StoreRow, StoreVisibility,
};
use store_libsql::SqliteBackend;

use crate::{daemon::parse_refresh_interval, job_queue::JobQueue, scheduler::UrlRefreshScheduler};

/// Effective config built from the DB.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub stores: Vec<EffectiveStore>,
}

/// A DB-backed store record for search/status use.
#[derive(Debug, Clone)]
pub struct EffectiveStore {
    pub name: String,
    pub id: String,
    pub visibility: String,
    pub backend: String,
    pub indexing: localdb_core::config::schema::IndexingPolicyConfig,
}

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
    yaml_config: RwLock<RawConfig>,
    data_dir: PathBuf,
    backend: Arc<dyn StoreBackend>,
    default_indexing_policy: IndexingPolicyConfig,
    default_policy_version: String,
    job_queue: JobQueue,
    url_scheduler: UrlRefreshScheduler,
}

impl AppState {
    /// Create a new `AppState`.
    pub async fn new(
        yaml_config: RawConfig,
        data_dir: PathBuf,
        job_queue: JobQueue,
        url_scheduler: UrlRefreshScheduler,
    ) -> Result<Self, Error> {
        let embedding_policy = &yaml_config.defaults.indexing.embedding;
        let providers = &yaml_config.providers;
        let (dim, encoding) =
            embed::infer_dim_encoding(embedding_policy, providers).map_err(|e| {
                Error::InvalidConfig {
                    message: format!("cannot determine embedding shape for daemon: {e}"),
                }
            })?;
        let db_path = data_dir.join("localdb.db");
        let config = StoreBackendConfig::local_path(db_path, dim, encoding);
        let backend = Arc::new(SqliteBackend::open(config).await?) as Arc<dyn StoreBackend>;
        let default_indexing_policy = yaml_config.defaults.indexing.clone();
        let default_policy_version = compute_policy_version(&default_indexing_policy);

        Ok(Self {
            inner: Arc::new(Inner {
                yaml_config: RwLock::new(yaml_config),
                data_dir,
                backend,
                default_indexing_policy,
                default_policy_version,
                job_queue,
                url_scheduler,
            }),
        })
    }

    /// Access the job queue.
    pub fn job_queue(&self) -> &JobQueue {
        &self.inner.job_queue
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.inner.data_dir
    }

    pub fn backend(&self) -> &dyn StoreBackend {
        self.inner.backend.as_ref()
    }

    pub fn backend_arc(&self) -> Arc<dyn StoreBackend> {
        self.inner.backend.clone()
    }

    /// Get the effective config (DB-backed stores only).
    pub async fn effective_config(&self) -> Result<EffectiveConfig, Error> {
        let runtime_stores = self.inner.backend.list_stores().await?;
        let mut stores = Vec::new();
        for store in runtime_stores {
            let indexing: localdb_core::config::schema::IndexingPolicyConfig =
                serde_json::from_str(&store.indexing_policy).map_err(|e| Error::Internal {
                    message: format!(
                        "invalid indexing_policy JSON for store '{}': {e}",
                        store.name
                    ),
                    correlation_id: "effective_config_policy_parse".into(),
                })?;
            stores.push(EffectiveStore {
                name: store.name,
                id: store.id,
                visibility: store_visibility_to_str(&store.visibility).to_string(),
                backend: store.backend,
                indexing,
            });
        }
        Ok(EffectiveConfig { stores })
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
    /// Returns `Error::InvalidRequest` if a store with the same name already exists.
    pub async fn add_store(&self, name: &str, visibility: &str) -> Result<Store, Error> {
        if self.inner.backend.get_store_by_name(name).await?.is_some() {
            return Err(Error::InvalidRequest {
                message: format!("store '{name}' already exists"),
            });
        }

        let id = localdb_core::new_ulid();
        let vis_enum = match visibility {
            "shared" => StoreVisibility::Shared,
            "private" => StoreVisibility::Private,
            _ => {
                return Err(Error::InvalidRequest {
                    message: format!(
                        "unknown visibility '{visibility}'; expected 'private' or 'shared'"
                    ),
                })
            }
        };
        let indexing_policy =
            serde_json::to_string(&self.inner.default_indexing_policy).map_err(|e| {
                Error::Internal {
                    message: format!("cannot serialize default indexing policy: {e}"),
                    correlation_id: "appdb_serialize_default_policy".into(),
                }
            })?;
        let row = StoreRow {
            id: id.clone(),
            name: name.to_string(),
            visibility: vis_enum.clone(),
            backend: "libsql".to_string(),
            indexing_policy,
            policy_version: self.inner.default_policy_version.clone(),
            acl: "{}".to_string(),
            created_at: now_rfc3339(),
        };

        self.inner.backend.upsert_store(&row).await?;

        Ok(Store {
            id,
            name: name.to_string(),
            visibility: vis_enum,
            backend: localdb_core::BackendConfig {
                kind: "libsql".to_string(),
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
    /// Returns `Error::StoreNotFound` if the store doesn't exist.
    pub async fn remove_store(&self, name: &str) -> Result<(), Error> {
        let row = self
            .inner
            .backend
            .get_store_by_name(name)
            .await?
            .ok_or_else(|| Error::StoreNotFound {
                id: name.to_string(),
            })?;
        let deleted = self.inner.backend.delete_store(&row.id).await?;
        if !deleted {
            return Err(Error::StoreNotFound {
                id: name.to_string(),
            });
        }
        Ok(())
    }

    /// Get a store by name.
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
            })
            .ok_or_else(|| Error::StoreNotFound {
                id: name.to_string(),
            })
    }

    /// Add a source to a store.
    ///
    /// Returns `Error::StoreNotFound` if the store doesn't exist.
    pub async fn add_source(
        &self,
        store_name: &str,
        kind: &str,
        spec: serde_json::Value,
        preset: &str,
        refresh: Option<&str>,
    ) -> Result<SourceRecord, Error> {
        let store_row = self
            .inner
            .backend
            .get_store_by_name(store_name)
            .await?
            .ok_or_else(|| Error::StoreNotFound {
                id: store_name.to_string(),
            })?;
        let store_id = store_row.id;
        let (kind_enum, root, url, include, exclude) = parse_source_spec(kind, &spec)?;

        let id = localdb_core::new_ulid();
        let source_row = SourceRow {
            id: id.clone(),
            store_id: store_id.clone(),
            kind: kind_enum.clone(),
            root,
            url: url.clone(),
            include,
            exclude,
            preset: preset.to_string(),
            refresh: refresh.map(|s| s.to_string()),
            created_at: now_rfc3339(),
        };
        self.inner.backend.upsert_source(&source_row).await?;

        // Register URL sources with the scheduler so refresh runs without a restart.
        if kind_enum == localdb_core::types::SourceKind::Url {
            if let Some(u) = url {
                let interval_secs = refresh.and_then(parse_refresh_interval);
                self.inner
                    .url_scheduler
                    .register(id.clone(), store_name.to_string(), u, interval_secs)
                    .await;
            }
        }

        Ok(SourceRecord {
            id,
            store_id,
            kind: kind.to_string(),
            spec,
            preset: preset.to_string(),
        })
    }

    /// List sources for a store.
    pub async fn list_sources(&self, store_name: &str) -> Result<Vec<SourceRecord>, Error> {
        let store = self
            .inner
            .backend
            .get_store_by_name(store_name)
            .await?
            .ok_or_else(|| Error::StoreNotFound {
                id: store_name.to_string(),
            })?;
        self.inner
            .backend
            .list_sources(&store.id)
            .await?
            .into_iter()
            .map(source_row_to_record)
            .collect()
    }

    /// Remove a source by ID.
    ///
    /// Returns `Error::SourceNotFound` if the source doesn't exist.
    pub async fn remove_source(&self, source_id: &str) -> Result<(), Error> {
        let deleted = self.inner.backend.delete_source(source_id).await?;
        if !deleted {
            return Err(Error::SourceNotFound {
                id: source_id.to_string(),
            });
        }
        Ok(())
    }

    /// Get a source by ID.
    pub async fn get_source(&self, source_id: &str) -> Result<SourceRecord, Error> {
        let source = self
            .inner
            .backend
            .get_source(source_id)
            .await?
            .ok_or_else(|| Error::SourceNotFound {
                id: source_id.to_string(),
            })?;
        source_row_to_record(source)
    }

    /// Update a runtime-owned store's mutable fields.
    ///
    /// Returns `Error::StoreNotFound` if the store doesn't exist.
    pub async fn update_store(&self, name: &str, visibility: Option<&str>) -> Result<(), Error> {
        let row = self
            .inner
            .backend
            .get_store_by_name(name)
            .await?
            .ok_or_else(|| Error::StoreNotFound {
                id: name.to_string(),
            })?;
        let vis_new = match (visibility, &row.visibility) {
            (Some("shared"), _) => StoreVisibility::Shared,
            (Some("private"), _) => StoreVisibility::Private,
            (Some(other), _) => {
                return Err(Error::InvalidRequest {
                    message: format!("unknown visibility '{other}'"),
                })
            }
            (None, v) => v.clone(),
        };
        let updated = StoreRow {
            visibility: vis_new,
            ..row
        };
        self.inner.backend.upsert_store(&updated).await?;
        Ok(())
    }
}

fn store_visibility_to_str(visibility: &StoreVisibility) -> &'static str {
    match visibility {
        StoreVisibility::Private => "private",
        StoreVisibility::Shared => "shared",
    }
}

type ParsedSourceSpec = (
    localdb_core::types::SourceKind,
    Option<String>,
    Option<String>,
    Vec<String>,
    Vec<String>,
);

fn parse_source_spec(kind: &str, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
    match kind {
        "path" => {
            let root = spec
                .get("root")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: "path source requires 'root'".to_string(),
                })?;
            let include = string_array_field(spec, "include")?;
            let exclude = string_array_field(spec, "exclude")?;
            Ok((
                localdb_core::types::SourceKind::Path,
                Some(root),
                None,
                include,
                exclude,
            ))
        }
        "url" => {
            let url = spec
                .get("url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: "url source requires 'url'".to_string(),
                })?;
            Ok((
                localdb_core::types::SourceKind::Url,
                None,
                Some(url),
                Vec::new(),
                Vec::new(),
            ))
        }
        other => Err(Error::InvalidRequest {
            message: format!("unknown source kind '{other}'"),
        }),
    }
}

fn string_array_field(spec: &serde_json::Value, field: &str) -> Result<Vec<String>, Error> {
    let Some(raw) = spec.get(field) else {
        return Ok(Vec::new());
    };
    let arr = raw.as_array().ok_or_else(|| Error::InvalidRequest {
        message: format!("source spec field '{field}' must be a JSON array of strings"),
    })?;
    arr.iter()
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: format!("source spec field '{field}' contains a non-string value"),
                })
        })
        .collect()
}

fn source_row_to_record(row: SourceRow) -> Result<SourceRecord, Error> {
    let (kind, spec) = match row.kind {
        localdb_core::types::SourceKind::Path => {
            let root = row.root.ok_or_else(|| Error::Internal {
                message: format!("path source '{}' has no root", row.id),
                correlation_id: "server_source_row_path".to_string(),
            })?;
            (
                "path".to_string(),
                serde_json::json!({"root": root, "include": row.include, "exclude": row.exclude}),
            )
        }
        localdb_core::types::SourceKind::Url => {
            let url = row.url.ok_or_else(|| Error::Internal {
                message: format!("url source '{}' has no url", row.id),
                correlation_id: "server_source_row_url".to_string(),
            })?;
            ("url".to_string(), serde_json::json!({"url": url}))
        }
    };
    Ok(SourceRecord {
        id: row.id,
        store_id: row.store_id,
        kind,
        spec,
        preset: row.preset,
    })
}

/// A store record as returned by the API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoreRecord {
    pub name: String,
    pub visibility: String,
    pub backend: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn make_state() -> (TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut yaml_config = RawConfig {
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
        let queue = JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir.path().to_path_buf(),
            queue.clone(),
            UrlRefreshScheduler::new(queue),
        )
        .await
        .unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn add_and_list_stores() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let effective = state.effective_config().await.unwrap();
        assert_eq!(effective.stores.len(), 1);
        assert_eq!(effective.stores[0].name, "notes");
    }

    #[tokio::test]
    async fn add_store_rejects_unknown_visibility() {
        let (_dir, state) = make_state().await;
        let result = state.add_store("notes", "public").await;
        assert!(matches!(result, Err(Error::InvalidRequest { .. })));
    }

    #[tokio::test]
    async fn remove_store_not_found() {
        let (_dir, state) = make_state().await;
        let result = state.remove_store("non-existent").await;
        assert!(matches!(result, Err(Error::StoreNotFound { .. })));
    }

    #[tokio::test]
    async fn remove_store_succeeds() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        state.remove_store("notes").await.unwrap();
        let effective = state.effective_config().await.unwrap();
        assert!(effective.stores.is_empty());
    }

    #[tokio::test]
    async fn add_source_to_nonexistent_store_fails() {
        let (_dir, state) = make_state().await;
        let result = state
            .add_source(
                "no-such-store",
                "path",
                serde_json::json!({"root": "/tmp"}),
                "prose",
                None,
            )
            .await;
        assert!(matches!(result, Err(Error::StoreNotFound { .. })));
    }

    #[tokio::test]
    async fn add_and_list_sources() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let source = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp/notes", "include": [], "exclude": []}),
                "prose",
                None,
            )
            .await
            .unwrap();

        let sources = state.list_sources("notes").await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, source.id);
    }

    #[tokio::test]
    async fn add_source_rejects_non_array_include() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let result = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp/notes", "include": "**/*.md"}),
                "prose",
                None,
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidRequest { .. })));
    }

    #[tokio::test]
    async fn add_source_rejects_non_string_exclude_entry() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let result = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp/notes", "exclude": [42]}),
                "prose",
                None,
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidRequest { .. })));
    }

    #[tokio::test]
    async fn remove_source_not_found() {
        let (_dir, state) = make_state().await;
        let result = state.remove_source("no-such-source").await;
        assert!(matches!(result, Err(Error::SourceNotFound { .. })));
    }

    #[tokio::test]
    async fn remove_source_succeeds() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let source = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp"}),
                "prose",
                None,
            )
            .await
            .unwrap();
        state.remove_source(&source.id).await.unwrap();
        let sources = state.list_sources("notes").await.unwrap();
        assert!(sources.is_empty());
    }

    #[tokio::test]
    async fn update_store_updates_visibility() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        state.update_store("notes", Some("shared")).await.unwrap();
        let record = state.get_store_by_name("notes").await.unwrap();
        assert_eq!(record.visibility, "shared");
    }

    #[tokio::test]
    async fn upsert_and_search_chunks_roundtrip() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let store_id = state
            .backend()
            .get_store_by_name("notes")
            .await
            .unwrap()
            .unwrap()
            .id;
        let source = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp/notes"}),
                "prose",
                None,
            )
            .await
            .unwrap();

        let chunk = localdb_core::ChunkRecord {
            id: "chunk-1".to_string(),
            document_id: "doc-1".to_string(),
            store_id: store_id.clone(),
            text: "hello world rust programming".to_string(),
            span: localdb_core::types::Span::new(0, 30),
            heading_path: vec![],
            embedding: vec![1.0; 128],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc".to_string(),
            origin_store: store_id.clone(),
            source_id: source.id,
            source_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            metadata: localdb_core::DocumentMetadata::default(),
        };

        let handle = state.backend().retrieval_store(&store_id).await.unwrap();
        handle.upsert_chunks(vec![chunk]).await.unwrap();
        let stats = handle.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1, "one chunk should be indexed");
    }

    #[tokio::test]
    async fn add_store_duplicate_name_returns_invalid_request() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let result = state.add_store("notes", "private").await;
        assert!(
            matches!(result, Err(Error::InvalidRequest { .. })),
            "duplicate store name should return InvalidRequest; got: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn remove_store_cascades_sources() {
        let (_dir, state) = make_state().await;

        state.add_store("scratch", "private").await.unwrap();
        state
            .add_source(
                "scratch",
                "path",
                serde_json::json!({"root": "/tmp/a"}),
                "prose",
                None,
            )
            .await
            .unwrap();
        state
            .add_source(
                "scratch",
                "path",
                serde_json::json!({"root": "/tmp/b"}),
                "prose",
                None,
            )
            .await
            .unwrap();

        let before = state.list_sources("scratch").await.unwrap();
        assert_eq!(before.len(), 2);

        state.remove_store("scratch").await.unwrap();
        assert!(
            matches!(
                state.list_sources("scratch").await,
                Err(Error::StoreNotFound { .. })
            ),
            "removed store should not list sources"
        );
        assert!(state.backend().list_stores().await.unwrap().is_empty());
    }
}
