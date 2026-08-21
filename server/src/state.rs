use std::path::{Path, PathBuf};
use std::sync::Arc;

use fetch::HttpUrlFetcher;
use tokio::sync::RwLock;

use localdb_core::{
    config::{
        policy::compute_policy_version,
        schema::{EmbeddingPolicy, HttpConfig, IndexingPolicyConfig, ProviderConfig, RawConfig},
    },
    get_document_detail_scoped,
    ingestion::now_rfc3339,
    resolve_named_stores, store_factory, DeletionPolicy, DocumentDetail, DocumentInfo, Embedder,
    Error, IndexJobScope, IndexJobStats, ProgressSink, SourceRow, Store, StoreBackend,
    StoreBackendConfig, StoreRow, StoreVisibility,
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

/// Cached HTTP fetcher pair (`(unrestricted, public_only)`, the split
/// `fetch::HttpUrlFetcher::new_pair` returns — see `job_exec::JobExecDeps::fetchers`'s
/// doc comment for why that split must never collapse) plus the `HttpConfig`
/// that produced it. See `Inner::fetcher_cache` / `AppState::get_or_build_fetchers`.
type FetcherCacheEntry = (HttpConfig, Arc<(HttpUrlFetcher, HttpUrlFetcher)>);

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
    /// Single-slot HTTP fetcher-pair cache, keyed by `HttpConfig` alone —
    /// unlike `embedder_cache`, a fetcher pair's identity depends on nothing
    /// but the outbound HTTP policy (issue #208 PR #227 review): with
    /// `server.job_workers` > 1, each job used to build its own fresh
    /// `HttpUrlFetcher::new_pair` (and so its own fresh `HostLimiter`),
    /// which multiplied `http.rate_limit.requests_per_second` by however
    /// many jobs for different stores happened to run concurrently against
    /// the same destination host, and meant one job observing a
    /// `Retry-After` cooldown never slowed the others — both violate issue
    /// #207's "per destination host, process-wide" pacing contract. Mirrors
    /// `embedder_cache`'s invalidation exactly: an unchanged `http:` block
    /// hits the cache; a changed one (operator edits
    /// `requests_per_second`/`burst`/`max_retries` and the daemon's
    /// config-file watcher reloads it, `reload_yaml_config`) misses and
    /// rebuilds on the next call — no explicit flush needed. Wrapped in an
    /// `Arc` (unlike `embedder_cache`'s bare `Arc<dyn Embedder>`, which is
    /// already reference-counted on its own) purely so
    /// `AppState::get_or_build_fetchers` can hand back one cheap `Arc::clone`
    /// per call and so tests can assert sharing via `Arc::ptr_eq` — the pair
    /// itself (`HttpUrlFetcher`) is already `Clone` internally, so this
    /// isn't needed for correctness, only for a cheap, directly-testable
    /// identity check. See `AppState::get_or_build_fetchers`.
    fetcher_cache: RwLock<Option<FetcherCacheEntry>>,
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
                fetcher_cache: RwLock::new(None),
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
        // guarantees at most one embedder is ever built per policy change —
        // the write-lock + double-checked-cache pattern serializes concurrent
        // builders safely. With `server.job_workers` > 1 (issue #208), two
        // cross-store jobs can race to build the cache on a cold start (or
        // after a policy change) and one simply waits behind the lock;
        // correctness is unchanged, the only cost is transient latency for
        // whichever job waits.
        let policy_owned = policy.clone();
        let providers_owned = providers.clone();
        let providers_for_build = providers_owned.clone();
        let http_owned = http.clone();
        let http_settings_for_build = fetch::http::HttpSettings::from(&http_owned);
        let models_dir = self.inner.models_dir.clone();
        let built = localdb_core::run_blocking(move || {
            // The cache key is `(EmbeddingPolicy, Vec<ProviderConfig>, HttpConfig)` —
            // `DownloadProgress` isn't part of it. That's fine only because the
            // daemon always passes the constant `Silent` here; if this ever
            // varies per call, the cache key must grow to include it.
            embed::create_embedder(
                &policy_owned,
                &providers_for_build,
                Some(&models_dir),
                &http_settings_for_build,
                embed::DownloadProgress::Silent,
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

    /// Get the HTTP fetcher pair for `yaml.http`, building it only when the
    /// outbound HTTP policy has changed since the last build (issue #208 PR
    /// #227 review — see `Inner::fetcher_cache`'s doc comment for the full
    /// rationale).
    ///
    /// Same double-checked-locking shape as [`Self::get_or_build_embedder`]
    /// immediately above, for the same reason: with `server.job_workers` > 1
    /// two cross-store jobs can race to build the cache on a cold start (or
    /// after an `http:` config reload), and one simply waits behind the
    /// write lock — correctness is unchanged, the only cost is transient
    /// latency for whichever job waits. Returns the cache's own `Arc` (not
    /// an unwrapped tuple) so a caller building many jobs in sequence only
    /// ever pays one cheap `Arc::clone` per call, and so a test can assert
    /// sharing directly via `Arc::ptr_eq` — see
    /// `get_or_build_fetchers_builds_once_across_repeated_calls` below.
    pub async fn get_or_build_fetchers(
        &self,
        yaml: &RawConfig,
    ) -> Result<Arc<(HttpUrlFetcher, HttpUrlFetcher)>, Error> {
        let http = &yaml.http;

        // Fast path: an unchanged http snapshot only ever needs a read lock.
        {
            let cache = self.inner.fetcher_cache.read().await;
            if let Some((cached_http, pair)) = cache.as_ref() {
                if cached_http == http {
                    return Ok(pair.clone());
                }
            }
        }

        let mut cache = self.inner.fetcher_cache.write().await;
        // Re-check under the write lock: another caller may have already
        // rebuilt for this exact http snapshot while we were waiting on it.
        if let Some((cached_http, pair)) = cache.as_ref() {
            if cached_http == http {
                return Ok(pair.clone());
            }
        }

        let http_owned = http.clone();
        let settings = fetch::http::HttpSettings::from(&http_owned);
        // `new_pair` shares one `HostLimiter` between the two fetchers
        // (issue #207) so per-host pacing holds across both surfaces a run
        // touches a host through — see `job_exec::JobExecDeps::fetchers`'s
        // doc comment for why the unrestricted/public-only split itself
        // must be preserved by every caller of this cache.
        let pair = Arc::new(HttpUrlFetcher::new_pair(&settings)?);

        *cache = Some((http_owned, pair.clone()));
        Ok(pair)
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
    /// `sources` before deciding whether to build an embedder or fetch a
    /// fetcher pair — never pay for a (potentially huge) embedding model, or
    /// populate the fetcher cache, just to discover the scope is empty or
    /// unresolvable (Codex review finding G1, issue #187).
    ///
    /// This is *the* call site (issue #208 PR #227 review) that makes every
    /// daemon job share one `AppState`-cached fetcher pair per `http:`
    /// config, closing the per-host pacing gap `server.job_workers` > 1
    /// otherwise opened — see `Inner::fetcher_cache`'s doc comment.
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
        let fetchers = if sources.is_empty() {
            None
        } else {
            Some((*self.get_or_build_fetchers(&yaml).await?).clone())
        };
        let deps = job_exec::JobExecDeps {
            backend: self.backend(),
            yaml: &yaml,
            models_dir: self.models_dir(),
            embedder,
            fetchers,
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

    /// List a page of documents in a store, ordered by `uri`, optionally
    /// filtered to a single source, plus the un-paginated total.
    ///
    /// Returns `Error::StoreNotFound` if the store doesn't exist. An unknown
    /// `source_id` is a pure filter, not an error — see
    /// `StoreBackend::list_documents`'s doc comment. `limit`/`offset` are
    /// forwarded to the backend, which performs the pagination in its own
    /// query rather than this loading every document in the store.
    pub async fn list_documents(
        &self,
        store_name: &str,
        source_id: Option<&str>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<(Vec<DocumentInfo>, u64), Error> {
        let store = self
            .inner
            .backend
            .get_store_by_name(store_name)
            .await?
            .ok_or_else(|| Error::StoreNotFound {
                id: store_name.to_string(),
            })?;
        let items = self
            .inner
            .backend
            .list_documents(&store.id, source_id, limit, offset)
            .await?;
        let total = self
            .inner
            .backend
            .count_documents(&store.id, source_id)
            .await?;
        Ok((items, total))
    }

    /// Look up a single document by id, optionally scoped to a caller-visible
    /// set of store names.
    ///
    /// `store_names` resolves through the same `resolve_named_stores` helper
    /// `?store=` scoping uses elsewhere (`resolve_status_scope`) — an unknown
    /// name is `Error::StoreNotFound` (→ 404). The resolved store ids are
    /// then handed to `get_document_detail_scoped`, which applies its own
    /// 0/1/many semantics: an empty list preserves the existing cross-store
    /// ambiguity error, one id SQL-scopes the lookup, and more than one id
    /// resolves unscoped followed by a membership check.
    pub async fn get_document(
        &self,
        doc_id: &str,
        store_names: &[String],
    ) -> Result<DocumentDetail, Error> {
        let stores = resolve_named_stores(self.backend(), store_names).await?;
        let store_ids: Vec<String> = stores.into_iter().map(|s| s.id).collect();
        get_document_detail_scoped(self.backend(), doc_id, &store_ids, true).await
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
mod tests;
