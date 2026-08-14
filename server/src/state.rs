use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

use localdb_core::{
    config::{
        policy::compute_policy_version,
        schema::{EmbeddingPolicy, HttpConfig, IndexingPolicyConfig, ProviderConfig, RawConfig},
    },
    ingestion::now_rfc3339,
    store_factory, DeletionPolicy, Embedder, Error, IndexJobScope, IndexJobStats, ProgressSink,
    SourceRow, Store, StoreBackend, StoreBackendConfig, StoreRow, StoreVisibility,
};
use store_libsql::SqliteBackend;

use crate::{job_exec, job_queue::JobQueue, scheduler::UrlRefreshScheduler};

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
    /// Raw refresh-interval string as given at creation time (e.g. "24h").
    /// Persisted for url and feed sources; `None` otherwise. #116: surfaced
    /// here so both surfaces (server response, `cli source list --json`)
    /// can report it without a separate lookup.
    #[serde(default)]
    pub refresh: Option<String>,
}

/// Shared application state for all handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

/// Cached embedder plus the `(EmbeddingPolicy, providers snapshot, http
/// config)` key that produced it. See `Inner::embedder_cache` /
/// `AppState::get_or_build_embedder`.
type EmbedderCacheEntry = (
    EmbeddingPolicy,
    Vec<ProviderConfig>,
    HttpConfig,
    Arc<dyn Embedder>,
);

struct Inner {
    yaml_config: RwLock<RawConfig>,
    data_dir: PathBuf,
    models_dir: PathBuf,
    backend: Arc<dyn StoreBackend>,
    default_indexing_policy: IndexingPolicyConfig,
    default_policy_version: String,
    job_queue: JobQueue,
    url_scheduler: UrlRefreshScheduler,
    /// Single-slot embedder cache, keyed by the `EmbeddingPolicy` plus the
    /// full `providers` snapshot that together determined the cached
    /// embedder's identity (Codex review finding F2, issue #187; provider
    /// settings added for finding H1, issue #212 — a hosted provider's
    /// `base_url`/`api_key_env` can change under an unchanged policy).
    /// `http` (issue #207 adversarial review, finding 1) is in the key for
    /// the same reason as `providers`: a hosted provider's client is built
    /// from `http:` too (user agent, retry count), so an operator changing
    /// `http.max_retries` via config reload with an otherwise-unchanged
    /// policy/providers must still rebuild — without this, the stale cached
    /// embedder would keep using the *old* `http:` settings indefinitely.
    /// See `AppState::get_or_build_embedder`.
    embedder_cache: RwLock<Option<EmbedderCacheEntry>>,
    /// Test-only construction counter for the `embed::create_embedder` call
    /// made by `get_or_build_embedder`, so tests can assert the embedder is
    /// built once per distinct `EmbeddingPolicy` rather than once per job.
    /// Scoped to this `AppState`'s own `Inner` rather than a shared
    /// process-wide static (contrast `cli::cmds::index::EMBEDDER_BUILD_COUNT`,
    /// which is safe as a static only because it is exercised by exactly one
    /// test in that crate) — nearly every job-executing test in this crate
    /// exercises `get_or_build_embedder` indirectly, so a shared static would
    /// have every one of them stomp on the same counter under `cargo test`'s
    /// default parallel test threads. Per-instance sidesteps that: each
    /// test's own `AppState` counts only its own builds. Compiled out
    /// entirely in non-test builds.
    #[cfg(test)]
    embedder_build_count: std::sync::atomic::AtomicUsize,
}

impl AppState {
    /// Create a new `AppState`, opening its own connection to `localdb.db`.
    ///
    /// This is the daemon's own constructor — used by `start_daemon`, where
    /// no connection to the store exists yet. Delegates the actual field
    /// assembly to [`Self::from_backend`] once the connection is open.
    pub async fn new(
        yaml_config: RawConfig,
        data_dir: PathBuf,
        models_dir: PathBuf,
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

        Ok(Self::from_backend(
            yaml_config,
            data_dir,
            models_dir,
            backend,
            job_queue,
            url_scheduler,
        ))
    }

    /// Create a new `AppState` around an already-open backend (issue #187
    /// stage 3).
    ///
    /// For embedded/in-process use: the CLI's `index` and `source add`
    /// commands already hold an open `StoreBackend` connection to
    /// `localdb.db` (via `AppDb::open`/`load_app_db`) by the time they need
    /// to run a job through `job_exec::run_job` — opening a second
    /// connection here would be wasteful, and more importantly,
    /// `SqliteBackend::open` (what `Self::new` calls) enforces a
    /// schema-version migration guard on every open; embedded mode must pay
    /// that cost at most once per process, not once for the CLI's own
    /// `AppDb::open` *and again* for a second `AppState`-owned connection to
    /// the same file.
    ///
    /// No I/O happens here — everything this constructor does is derived
    /// from `yaml_config` alone (mirroring `Self::new`'s
    /// `default_indexing_policy`/`default_policy_version` derivation
    /// exactly, so the two constructors can never drift apart on what an
    /// `AppState`'s default indexing policy is) or is simply stored as
    /// given. In particular, nothing here assumes the backend was *just*
    /// opened: `default_indexing_policy`/`default_policy_version` come from
    /// the YAML config, not from a query against the backend, and every
    /// other `Inner` field is either caller-supplied already or has no
    /// dependency on connection freshness.
    pub fn from_backend(
        yaml_config: RawConfig,
        data_dir: PathBuf,
        models_dir: PathBuf,
        backend: Arc<dyn StoreBackend>,
        job_queue: JobQueue,
        url_scheduler: UrlRefreshScheduler,
    ) -> Self {
        let default_indexing_policy = yaml_config.defaults.indexing.clone();
        let default_policy_version = compute_policy_version(&default_indexing_policy);

        Self {
            inner: Arc::new(Inner {
                yaml_config: RwLock::new(yaml_config),
                data_dir,
                models_dir,
                backend,
                default_indexing_policy,
                default_policy_version,
                job_queue,
                url_scheduler,
                embedder_cache: RwLock::new(None),
                #[cfg(test)]
                embedder_build_count: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// Access the job queue.
    pub fn job_queue(&self) -> &JobQueue {
        &self.inner.job_queue
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.inner.data_dir
    }

    /// The directory `embed::create_embedder` should cache/load local model
    /// weights from (mirrors the CLI's `ResolvedPaths::models_dir`).
    pub fn models_dir(&self) -> &Path {
        &self.inner.models_dir
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

    /// Get the embedder for `yaml`'s embedding policy, building it only when
    /// the policy, the provider settings it resolves against, or the
    /// outbound HTTP policy have changed since the last build (Codex review
    /// finding F2, issue #187; extended for finding H1, issue #212, and for
    /// `http:` at finding 1 of the issue #207 adversarial review).
    ///
    /// Before this cache existed, every job execution called
    /// `embed::create_embedder` from scratch — for the default local
    /// ONNX/CoreML provider that reloads the model weights on every single
    /// job. The single-slot cache below is keyed by `EmbeddingPolicy`
    /// (`yaml.defaults.indexing.embedding`, the model+provider pair that
    /// determines embedder identity), the full `yaml.providers` snapshot,
    /// and `yaml.http`: the same policy over an unchanged providers list and
    /// `http:` block hits the cache; a changed policy, a changed `providers`
    /// entry (e.g. a hosted provider's `base_url`/`api_key_env` edited under
    /// an otherwise unchanged policy), or a changed `http:` block (e.g.
    /// `max_retries`/`user_agent` edited for a hosted provider's client),
    /// misses and rebuilds. Comparing the whole `Vec`/`HttpConfig` rather
    /// than isolating "the provider this policy resolves to" is deliberate —
    /// simpler, and an unrelated provider/http edit costing one extra
    /// rebuild is an acceptable trade. A config reload (`reload_yaml_config`)
    /// needs no explicit cache flush — the caller always passes the freshly
    /// reloaded `yaml`, so a changed policy, providers list, or http block
    /// simply fails the equality check below on the next call and rebuilds
    /// naturally.
    pub async fn get_or_build_embedder(
        &self,
        yaml: &RawConfig,
    ) -> Result<Arc<dyn Embedder>, Error> {
        let policy = &yaml.defaults.indexing.embedding;
        let providers = &yaml.providers;
        let http = &yaml.http;

        // Fast path: an unchanged policy + providers + http snapshot only
        // ever needs a read lock.
        {
            let cache = self.inner.embedder_cache.read().await;
            if let Some((cached_policy, cached_providers, cached_http, embedder)) = cache.as_ref() {
                if cached_policy == policy && cached_providers == providers && cached_http == http {
                    return Ok(embedder.clone());
                }
            }
        }

        let mut cache = self.inner.embedder_cache.write().await;
        // Re-check under the write lock: another caller may have already
        // rebuilt for this exact policy + providers + http snapshot while we
        // were waiting on it.
        if let Some((cached_policy, cached_providers, cached_http, embedder)) = cache.as_ref() {
            if cached_policy == policy && cached_providers == providers && cached_http == http {
                return Ok(embedder.clone());
            }
        }

        // Build while still holding the write lock. This is deliberate: it
        // guarantees at most one embedder is ever built per policy change,
        // at the cost of serializing concurrent builders behind a cold/
        // changed cache. Acceptable today because the job engine runs a
        // single worker (issue #187) — there is never more than one job in
        // flight to contend for this lock.
        let policy_owned = policy.clone();
        let providers_owned = providers.clone();
        let providers_for_build = providers_owned.clone();
        let http_owned = http.clone();
        let http_settings_for_build = fetch::http::HttpSettings::from(&http_owned);
        let models_dir = self.inner.models_dir.clone();
        let built = localdb_core::run_blocking(move || {
            embed::create_embedder(
                &policy_owned,
                &providers_for_build,
                Some(&models_dir),
                &http_settings_for_build,
            )
        })?;
        let embedder: Arc<dyn Embedder> = Arc::from(built);

        #[cfg(test)]
        self.inner
            .embedder_build_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        *cache = Some((
            policy.clone(),
            providers_owned,
            http_owned,
            embedder.clone(),
        ));
        Ok(embedder)
    }

    /// Run one scoped index job end to end: resolve `scope`'s sources,
    /// build/reuse the cached embedder only if there's actually something to
    /// index, assemble `JobExecDeps`, and hand off to `job_exec::run_job`.
    ///
    /// Factored out of `handlers::jobs::create_job` and
    /// `UrlRefreshScheduler::tick` (#187 review, DRY finding): both ran this
    /// exact sequence — differing only in `deletion` (an HTTP caller's
    /// explicit policy vs. the scheduler's hardcoded `Retain`, issues
    /// #156/#185) and in what happens to the result afterward (the HTTP path
    /// returns it as the job's stats; the scheduler also stamps
    /// `last_refreshed` once it settles). Both call sites still resolve
    /// `sources` before deciding whether to build an embedder — never pay
    /// for a (potentially huge) embedding model just to discover the scope
    /// is empty or unresolvable (Codex review finding G1, issue #187).
    pub(crate) async fn run_scoped_job(
        &self,
        store_row: &StoreRow,
        scope: IndexJobScope,
        deletion: DeletionPolicy,
        progress: ProgressSink,
    ) -> Result<IndexJobStats, Error> {
        let yaml = self.yaml_config().await;
        let sources = job_exec::resolve_job_sources(self.backend(), &store_row.id, &scope).await?;
        let embedder = if sources.is_empty() {
            None
        } else {
            Some(self.get_or_build_embedder(&yaml).await?)
        };
        let deps = job_exec::JobExecDeps {
            backend: self.backend(),
            yaml: &yaml,
            models_dir: self.models_dir(),
            embedder,
            progress: Some(progress),
            on_source_error: None,
        };
        job_exec::run_job(store_row, scope, deletion, deps)
            .await
            .map(|(stats, _embedder)| stats)
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
                id: s.id.clone(),
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
        let localdb_core::source::ParsedSourceSpec {
            kind: kind_enum,
            root,
            url,
            include,
            exclude,
            config_json,
        } = localdb_core::source::parse_source_spec(kind, &spec)?;

        // Validate refresh interval before persisting anything.
        let interval_secs = match refresh {
            Some(r) => localdb_core::config::validate_refresh_interval(r)?,
            None => None,
        };

        // #116: feed sources persist+validate `refresh` like url sources, but
        // scheduler registration below stays url-only — feed refresh is
        // inert until the scheduler is extended (same stub status as the
        // pre-existing url refresh scheduling).
        if refresh.is_some()
            && kind_enum != localdb_core::types::SourceKind::Url
            && kind_enum != localdb_core::types::SourceKind::Feed
        {
            return Err(Error::InvalidRequest {
                message: "refresh is only supported for URL and feed sources".to_string(),
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
            config_json,
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

        // Return the row as persisted, not the raw request — defaults filled
        // in during persistence (or future normalization) must be reflected
        // in the 201 body so it matches a subsequent GET (#197).
        source_row_to_record(source_row)
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

pub(crate) fn store_visibility_to_str(visibility: &StoreVisibility) -> &'static str {
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
        // Mechanical fix to keep this match exhaustive after adding
        // `SourceKind::Feed` (issue #116) — full feed HTTP wiring
        // (scheduler registration, refresh handling) is done elsewhere;
        // this only shapes the JSON `spec` for list/get responses.
        localdb_core::types::SourceKind::Feed => {
            let url = row.url.ok_or_else(|| Error::Internal {
                message: format!("feed source '{}' has no url", row.id),
                correlation_id: "server_source_row_feed".to_string(),
            })?;
            let feed_config =
                localdb_core::source::parse_feed_config_json(row.config_json.as_deref());
            (
                "feed".to_string(),
                serde_json::json!({
                    "url": url,
                    "max_entries": feed_config.max_entries,
                    "fetch_full_content": feed_config.fetch_full_content,
                }),
            )
        }
    };
    Ok(SourceRecord {
        id: row.id,
        store_id: row.store_id,
        kind,
        spec,
        preset: row.preset,
        refresh: row.refresh,
    })
}

/// A store record as returned by the API.
///
/// `id` (issue #187 stage 5): needed so `POST /v1/stores`'s response can
/// carry the same `{status, name, id}` shape the embedded `store add` path
/// has always returned in `--json` mode — without it, the CLI's daemon-aware
/// dispatch table would have no way to render an identical `Outcome` for both
/// transports. Populated on every handler (`list_stores`, `create_store`,
/// `get_store`, `patch_store`) rather than only `create_store`, so the type
/// never has a "sometimes has an id" ambiguity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoreRecord {
    pub name: String,
    pub id: String,
    pub visibility: String,
    pub backend: String,
}

#[cfg(test)]
impl AppState {
    async fn scheduler_source_count(&self) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn make_state() -> (TempDir, AppState) {
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

    #[tokio::test]
    async fn models_dir_returns_the_value_it_was_given() {
        let (dir, state) = make_state().await;
        assert_eq!(state.models_dir(), dir.path().join("models"));
    }

    // --- from_backend (issue #187 stage 3) ---------------------------------

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
            page: None,
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

    // --- #116: feed sources ---

    #[tokio::test]
    async fn add_feed_source_persists_clean_spec_and_config_json() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let source = state
            .add_source(
                "notes",
                "feed",
                serde_json::json!({
                    "url": "https://example.com/feed.xml",
                    "max_entries": 25,
                    "fetch_full_content": false,
                }),
                "prose",
                None,
            )
            .await
            .unwrap();

        let fetched = state.get_source(&source.id).await.unwrap();
        assert_eq!(fetched.kind, "feed");
        assert_eq!(fetched.spec["url"], "https://example.com/feed.xml");
        assert_eq!(fetched.spec["max_entries"], 25);
        assert_eq!(fetched.spec["fetch_full_content"], false);
        // Never leak the raw config_json blob through the reconstructed spec.
        assert!(fetched.spec.get("config_json").is_none());
    }

    #[tokio::test]
    async fn add_feed_source_bad_url_is_rejected() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let result = state
            .add_source(
                "notes",
                "feed",
                serde_json::json!({"url": "ftp://example.com/feed.xml"}),
                "prose",
                None,
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidRequest { .. })));
        assert!(state.list_sources("notes").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_feed_source_max_entries_zero_is_rejected() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let result = state
            .add_source(
                "notes",
                "feed",
                serde_json::json!({
                    "url": "https://example.com/feed.xml",
                    "max_entries": 0,
                }),
                "prose",
                None,
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidRequest { .. })));
        assert!(state.list_sources("notes").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_feed_source_refresh_is_accepted_and_surfaced() {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let source = state
            .add_source(
                "notes",
                "feed",
                serde_json::json!({"url": "https://example.com/feed.xml"}),
                "prose",
                Some("1h"),
            )
            .await
            .unwrap();
        assert_eq!(source.refresh.as_deref(), Some("1h"));

        let fetched = state.get_source(&source.id).await.unwrap();
        assert_eq!(fetched.refresh.as_deref(), Some("1h"));
    }

    #[tokio::test]
    async fn add_feed_source_does_not_register_with_url_scheduler() {
        // Feed refresh is persisted+validated but inert (#116) — the
        // scheduler stays url-only, same stub status as pre-existing url
        // refresh scheduling.
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        state
            .add_source(
                "notes",
                "feed",
                serde_json::json!({"url": "https://example.com/feed.xml"}),
                "prose",
                Some("1h"),
            )
            .await
            .unwrap();
        assert_eq!(state.scheduler_source_count().await, 0);
    }

    #[tokio::test]
    async fn add_source_same_url_across_kinds_is_rejected_known_limitation() {
        // Known limitation (#116): `idx_sources_store_url` is UNIQUE on
        // (store_id, url) regardless of kind, so a url source and a feed
        // source can never coexist on the same URL within a store even
        // though they index semantically different content (raw page vs.
        // feed entries). This pins the current cross-kind ownership
        // behavior; making the constraint kind-aware is a follow-up, not
        // part of #116.
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        state
            .add_source(
                "notes",
                "url",
                serde_json::json!({"url": "https://example.com/same"}),
                "prose",
                None,
            )
            .await
            .unwrap();

        let result = state
            .add_source(
                "notes",
                "feed",
                serde_json::json!({"url": "https://example.com/same"}),
                "prose",
                None,
            )
            .await;
        assert!(
            matches!(result, Err(Error::InvalidRequest { .. })),
            "expected InvalidRequest (duplicate URL across kinds), got: {:?}",
            result
        );
        assert_eq!(state.list_sources("notes").await.unwrap().len(), 1);
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

    // --- get_or_build_embedder (Codex review finding F2, issue #187) -------

    /// Three sequential calls with the same policy must build the embedder
    /// exactly once — the read-lock fast path must hit on calls 2 and 3, and
    /// every call must return the same `Arc`.
    #[tokio::test]
    async fn get_or_build_embedder_builds_once_across_repeated_calls() {
        let (_dir, state) = make_state().await;
        let yaml = state.yaml_config().await;

        let a = state.get_or_build_embedder(&yaml).await.unwrap();
        let b = state.get_or_build_embedder(&yaml).await.unwrap();
        let c = state.get_or_build_embedder(&yaml).await.unwrap();

        assert_eq!(
            state.embedder_build_count(),
            1,
            "embedder should be built exactly once across 3 calls with an unchanged policy"
        );
        assert!(
            Arc::ptr_eq(&a, &b),
            "second call should return the cached Arc"
        );
        assert!(
            Arc::ptr_eq(&a, &c),
            "third call should return the cached Arc"
        );
    }

    /// A changed `EmbeddingPolicy` (different model) must miss the cache and
    /// rebuild, returning a distinct `Arc`.
    #[tokio::test]
    async fn get_or_build_embedder_rebuilds_on_policy_change() {
        let (_dir, state) = make_state().await;
        let mut yaml = state.yaml_config().await;

        let first = state.get_or_build_embedder(&yaml).await.unwrap();

        yaml.defaults.indexing.embedding.model = "different-model".to_string();
        let second = state.get_or_build_embedder(&yaml).await.unwrap();

        assert_eq!(
            state.embedder_build_count(),
            2,
            "a changed embedding policy should trigger a rebuild"
        );
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a rebuilt embedder must not be the same Arc as the stale cached one"
        );
    }

    /// After `reload_yaml_config` swaps in a config with a different
    /// embedding policy, the next `get_or_build_embedder` call (using the
    /// freshly reloaded snapshot, as every real call site does via
    /// `state.yaml_config()`) must rebuild exactly once — no explicit cache
    /// flush needed, since the policy comparison itself misses.
    #[tokio::test]
    async fn get_or_build_embedder_rebuilds_once_after_config_reload() {
        let (_dir, state) = make_state().await;
        let old_yaml = state.yaml_config().await;
        let old = state.get_or_build_embedder(&old_yaml).await.unwrap();

        let mut new_yaml = old_yaml.clone();
        new_yaml.defaults.indexing.embedding.model = "reloaded-model".to_string();
        state.reload_yaml_config(new_yaml).await;

        let reloaded_yaml = state.yaml_config().await;
        let rebuilt = state.get_or_build_embedder(&reloaded_yaml).await.unwrap();
        let rebuilt_again = state.get_or_build_embedder(&reloaded_yaml).await.unwrap();

        assert_eq!(
            state.embedder_build_count(),
            2,
            "should build once for the original policy, once more for the reloaded policy"
        );
        assert!(
            !Arc::ptr_eq(&old, &rebuilt),
            "post-reload embedder must not be the stale pre-reload Arc"
        );
        assert!(
            Arc::ptr_eq(&rebuilt, &rebuilt_again),
            "a second call against the same reloaded policy should hit the cache"
        );
    }

    /// An unchanged `EmbeddingPolicy` but a changed `providers` entry (e.g.
    /// editing a hosted provider's `base_url` under `providers:` in the
    /// YAML) must still miss the cache and rebuild — the cache key is
    /// policy *and* the providers snapshot, not policy alone (Codex review
    /// finding H1, issue #212).
    #[tokio::test]
    async fn get_or_build_embedder_rebuilds_on_provider_settings_change() {
        let (_dir, state) = make_state().await;
        let mut old_yaml = state.yaml_config().await;
        old_yaml.providers = vec![localdb_core::config::schema::ProviderConfig {
            name: "hosted".to_string(),
            kind: "openai-compatible".to_string(),
            base_url: Some("https://old.example.com".to_string()),
            api_key_env: Some("OLD_API_KEY".to_string()),
        }];
        state.reload_yaml_config(old_yaml.clone()).await;
        let first = state.get_or_build_embedder(&old_yaml).await.unwrap();

        let mut new_yaml = old_yaml.clone();
        new_yaml.providers[0].base_url = Some("https://new.example.com".to_string());
        state.reload_yaml_config(new_yaml.clone()).await;
        let second = state.get_or_build_embedder(&new_yaml).await.unwrap();

        assert_eq!(
            state.embedder_build_count(),
            2,
            "a changed provider base_url under an unchanged policy should trigger a rebuild"
        );
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a rebuilt embedder must not be the same Arc as the stale cached one"
        );
    }

    /// An unchanged `EmbeddingPolicy` and `providers` but a changed `http:`
    /// block (e.g. editing `max_retries` or `user_agent`) must still miss
    /// the cache and rebuild — the cache key is policy, providers, *and*
    /// `http`, not policy+providers alone (issue #207 adversarial review,
    /// finding 1). Without this, an operator flipping `http.max_retries` via
    /// a live config reload would keep getting an embedder built from the
    /// *old* `http:` snapshot indefinitely, since the policy/providers
    /// equality check alone would report a cache hit.
    #[tokio::test]
    async fn get_or_build_embedder_rebuilds_on_http_config_change() {
        let (_dir, state) = make_state().await;
        let old_yaml = state.yaml_config().await;
        let first = state.get_or_build_embedder(&old_yaml).await.unwrap();

        let mut new_yaml = old_yaml.clone();
        new_yaml.http.max_retries = old_yaml.http.max_retries + 1;
        state.reload_yaml_config(new_yaml.clone()).await;
        let second = state.get_or_build_embedder(&new_yaml).await.unwrap();

        assert_eq!(
            state.embedder_build_count(),
            2,
            "a changed http.max_retries under an unchanged policy/providers should rebuild"
        );
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a rebuilt embedder must not be the same Arc as the stale cached one"
        );

        // A third call with the same (already-changed) http config must hit
        // the cache again — this isn't a "rebuild on every call" regression.
        let third = state.get_or_build_embedder(&new_yaml).await.unwrap();
        assert_eq!(
            state.embedder_build_count(),
            2,
            "an unchanged http config on a subsequent call should hit the cache, not rebuild"
        );
        assert!(
            Arc::ptr_eq(&second, &third),
            "third call should return the cached Arc from the second build"
        );
    }
}
