use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use localdb_core::{
    config::{
        policy::compute_policy_version,
        schema::{IndexingPolicyConfig, RawConfig},
    },
    ingestion::now_rfc3339,
    store_factory, Error, SourceRow, Store, StoreBackend, StoreBackendConfig, StoreRow,
    StoreVisibility,
};
use store_libsql::{LibsqlAuthStore, SqliteBackend};

use crate::{
    auth::{AuthMode, ServerAuthService},
    job_queue::JobQueue,
    scheduler::UrlRefreshScheduler,
};

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
    /// The YAML config as loaded at startup. Plain (no lock): config is read
    /// once at process startup and never reloaded — the file-watcher-based
    /// hot-reload was removed in T3 (specs/03-config.md §5). A change to the
    /// file takes effect on the next daemon restart.
    yaml_config: RawConfig,
    data_dir: PathBuf,
    backend: Arc<dyn StoreBackend>,
    /// The auth policy service over the same unified on-disk database as
    /// `backend` (`<data_dir>/localdb.db`) — users/keys persist across
    /// restarts, and break-glass CLI writes (`localdb user add`) are visible
    /// to a subsequently started daemon.
    auth: Arc<ServerAuthService>,
    /// The raw `AuthStore` behind `auth`, for queries the policy layer does
    /// not wrap (e.g. `count_users` for the setup-code bootstrap).
    auth_store: Arc<LibsqlAuthStore>,
    /// Resolved at startup from `server.auth` + the actually-bound address
    /// (`daemon::resolve_auth_mode`).
    auth_mode: AuthMode,
    /// blake3 hash of the one-time setup code, when one was generated at
    /// startup (`auth::generate_setup_code_if_needed`). T4's `/authorize`
    /// redeems it; nothing consumes it in T3.
    setup_code_hash: RwLock<Option<String>>,
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
        auth_mode: AuthMode,
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
        let backend = Arc::new(SqliteBackend::open(config).await?);
        // Auth shares the unified database connection (persistent on disk),
        // so the daemon and the break-glass CLI see the same users/keys.
        let auth_store = Arc::new(backend.auth_store());
        let auth = Arc::new(ServerAuthService::new(auth_store.clone()));
        let backend = backend as Arc<dyn StoreBackend>;
        let default_indexing_policy = yaml_config.defaults.indexing.clone();
        let default_policy_version = compute_policy_version(&default_indexing_policy);

        Ok(Self {
            inner: Arc::new(Inner {
                yaml_config,
                data_dir,
                backend,
                auth,
                auth_store,
                auth_mode,
                setup_code_hash: RwLock::new(None),
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

    /// The YAML config as loaded at startup. There is no hot-reload
    /// (specs/03-config.md §5): this reflects startup state for the process
    /// lifetime.
    pub fn yaml_config(&self) -> &RawConfig {
        &self.inner.yaml_config
    }

    /// The configured `server.public_url`, if any (specs/03-config.md §1) —
    /// the OAuth discovery base-URL resolution's preferred source
    /// (`server::auth::base_url::resolve_base_url`, T7).
    pub fn public_url(&self) -> Option<&str> {
        self.inner.yaml_config.server.public_url.as_deref()
    }

    /// The auth policy service, backed by the same persistent unified
    /// database as `backend()`.
    pub fn auth(&self) -> &Arc<ServerAuthService> {
        &self.inner.auth
    }

    /// The raw `AuthStore` behind `auth()` (setup-code bootstrap, tests).
    pub fn auth_store(&self) -> &Arc<LibsqlAuthStore> {
        &self.inner.auth_store
    }

    /// The auth enforcement mode resolved at startup.
    pub fn auth_mode(&self) -> AuthMode {
        self.inner.auth_mode
    }

    /// Record the blake3 hash of the one-time setup code (D3b). Called once
    /// at startup by `auth::generate_setup_code_if_needed`.
    pub fn set_setup_code_hash(&self, hash: String) {
        *self
            .inner
            .setup_code_hash
            .write()
            .expect("setup_code_hash lock poisoned") = Some(hash);
    }

    /// The held setup-code hash, if a code was generated at startup. This is
    /// the T4 seam: `/authorize` verifies a presented code against this hash
    /// to bootstrap the first admin user.
    pub fn setup_code_hash(&self) -> Option<String> {
        self.inner
            .setup_code_hash
            .read()
            .expect("setup_code_hash lock poisoned")
            .clone()
    }

    /// Atomically verify `presented_hash` (the blake3 hash of a caller-typed
    /// setup code) against the held setup-code hash and, on an exact match,
    /// consume it — clearing it so it can never be redeemed a second time.
    /// Returns `true` only on a match; a non-matching guess leaves the held
    /// hash untouched so a legitimate follow-up attempt still works. Used by
    /// `POST /authorize`'s bootstrap path (`server::auth::oauth`).
    pub fn consume_setup_code_if_matches(&self, presented_hash: &str) -> bool {
        let mut guard = self
            .inner
            .setup_code_hash
            .write()
            .expect("setup_code_hash lock poisoned");
        if guard.as_deref() == Some(presented_hash) {
            *guard = None;
            true
        } else {
            false
        }
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
        let row = store_factory::default_store_row(
            name,
            vis_enum.clone(),
            &self.inner.default_indexing_policy,
            &self.inner.default_policy_version,
        )?;
        let id = row.id.clone();

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
        // Unregister all sources before cascade delete.
        let src_rows = self.inner.backend.list_sources(&row.id).await?;
        for src in &src_rows {
            self.inner.url_scheduler.unregister(&src.id).await;
        }
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
        let (kind_enum, root, url, include, exclude) =
            localdb_core::source::parse_source_spec(kind, &spec)?;

        // Validate refresh interval before persisting anything.
        let interval_secs = match refresh {
            Some(r) => localdb_core::config::validate_refresh_interval(r)?,
            None => None,
        };

        if refresh.is_some() && kind_enum != localdb_core::types::SourceKind::Url {
            return Err(Error::InvalidRequest {
                message: "refresh is only supported for URL sources".to_string(),
            });
        }

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
        self.inner.url_scheduler.unregister(source_id).await;
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
impl AppState {
    async fn scheduler_source_count(&self) -> usize {
        self.inner.url_scheduler.source_count().await
    }
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
            AuthMode::Open,
        )
        .await
        .unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn consume_setup_code_if_matches_only_on_exact_match() {
        let (_dir, state) = make_state().await;
        state.set_setup_code_hash("hash-of-real-code".to_string());

        assert!(
            !state.consume_setup_code_if_matches("wrong-hash"),
            "a mismatching guess must not consume the code"
        );
        assert_eq!(
            state.setup_code_hash().as_deref(),
            Some("hash-of-real-code"),
            "a failed guess leaves the hash in place for a legitimate retry"
        );

        assert!(state.consume_setup_code_if_matches("hash-of-real-code"));
        assert!(
            state.setup_code_hash().is_none(),
            "a matching presentation consumes (clears) the hash"
        );

        assert!(
            !state.consume_setup_code_if_matches("hash-of-real-code"),
            "the code cannot be redeemed a second time"
        );
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
            resource_id: "doc-1".to_string(),
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
            ingestor_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            window_block_seqs: vec![],
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

    // --- WS2: Validate refresh interval before persisting ---

    #[tokio::test]
    async fn add_source_invalid_refresh_is_rejected() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let result = state
            .add_source(
                "notes",
                "url",
                serde_json::json!({ "url": "https://example.com" }),
                "prose",
                Some("badvalue"),
            )
            .await;
        assert!(
            matches!(result, Err(localdb_core::Error::InvalidRequest { .. })),
            "expected InvalidRequest for invalid refresh, got: {:?}",
            result
        );
        // Nothing should have been persisted.
        let sources = state.list_sources("notes").await.unwrap();
        assert!(
            sources.is_empty(),
            "no source should be stored after invalid refresh"
        );
    }

    #[tokio::test]
    async fn add_source_zero_refresh_is_rejected() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        for zero in &["0", "0s", "0m", "0h"] {
            let result = state
                .add_source(
                    "notes",
                    "url",
                    serde_json::json!({ "url": "https://example.com" }),
                    "prose",
                    Some(zero),
                )
                .await;
            assert!(
                matches!(result, Err(localdb_core::Error::InvalidRequest { .. })),
                "expected InvalidRequest for zero refresh '{zero}', got: {:?}",
                result
            );
        }
        let sources = state.list_sources("notes").await.unwrap();
        assert!(
            sources.is_empty(),
            "no source should be stored after zero refresh"
        );
    }

    #[tokio::test]
    async fn add_source_refresh_on_path_source_is_rejected() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let result = state
            .add_source(
                "notes",
                "path",
                serde_json::json!({"root": "/tmp/notes", "include": [], "exclude": []}),
                "prose",
                Some("1h"),
            )
            .await;
        assert!(
            matches!(result, Err(localdb_core::Error::InvalidRequest { .. })),
            "expected InvalidRequest for refresh on path source, got: {:?}",
            result
        );
        let sources = state.list_sources("notes").await.unwrap();
        assert!(
            sources.is_empty(),
            "no source should be stored when refresh on path source is rejected"
        );
    }

    #[tokio::test]
    async fn add_source_valid_refresh_is_accepted() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        state
            .add_source(
                "notes",
                "url",
                serde_json::json!({ "url": "https://example.com" }),
                "prose",
                Some("1h"),
            )
            .await
            .unwrap();
        let sources = state.list_sources("notes").await.unwrap();
        assert_eq!(sources.len(), 1);
    }

    // --- WS3: Unregister scheduler records on delete ---

    #[tokio::test]
    async fn remove_source_unregisters_from_scheduler() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let src = state
            .add_source(
                "notes",
                "url",
                serde_json::json!({ "url": "https://example.com" }),
                "prose",
                Some("1h"),
            )
            .await
            .unwrap();
        assert_eq!(state.scheduler_source_count().await, 1);
        state.remove_source(&src.id).await.unwrap();
        assert_eq!(
            state.scheduler_source_count().await,
            0,
            "url_scheduler should have 0 sources after remove_source"
        );
    }

    #[tokio::test]
    async fn remove_store_unregisters_all_sources() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        state
            .add_source(
                "notes",
                "url",
                serde_json::json!({ "url": "https://example.com/a" }),
                "prose",
                Some("1h"),
            )
            .await
            .unwrap();
        state
            .add_source(
                "notes",
                "url",
                serde_json::json!({ "url": "https://example.com/b" }),
                "prose",
                Some("2h"),
            )
            .await
            .unwrap();
        assert_eq!(state.scheduler_source_count().await, 2);
        state.remove_store("notes").await.unwrap();
        assert_eq!(
            state.scheduler_source_count().await,
            0,
            "url_scheduler should have 0 sources after remove_store"
        );
    }
}
