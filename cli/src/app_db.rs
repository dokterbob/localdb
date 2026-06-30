use std::sync::Arc;

use localdb_core::{
    config::{
        loader::{
            load_config, load_config_from_str, resolve_config_path, ConfigLoader, LoadOptions,
            ResolvedPaths,
        },
        policy::compute_policy_version,
        schema::{EmbeddingPolicy, IndexingPolicyConfig, ProviderConfig},
    },
    store_factory,
    types::StoreVisibility,
    Error, StoreBackend, StoreBackendConfig, StoreRow,
};
use store_libsql::SqliteBackend;

use crate::{daemon_client::CliContext, normalize::exit_err};

pub struct AppDb {
    backend: Arc<dyn StoreBackend>,
    default_indexing_policy: IndexingPolicyConfig,
    default_policy_version: String,
}

impl AppDb {
    pub async fn open(
        paths: &ResolvedPaths,
        embedding_policy: &EmbeddingPolicy,
        providers: &[ProviderConfig],
        default_indexing_policy: IndexingPolicyConfig,
    ) -> Result<Self, Error> {
        let (dim, encoding) =
            embed::infer_dim_encoding(embedding_policy, providers).map_err(|e| {
                Error::InvalidConfig {
                    message: format!("cannot determine embedding shape: {e}"),
                }
            })?;
        let config = StoreBackendConfig::local_path(paths.db_path(), dim, encoding);
        let backend = Arc::new(SqliteBackend::open(config).await?) as Arc<dyn StoreBackend>;
        let default_policy_version = compute_policy_version(&default_indexing_policy);
        Ok(Self {
            backend,
            default_indexing_policy,
            default_policy_version,
        })
    }

    pub fn backend(&self) -> &dyn StoreBackend {
        self.backend.as_ref()
    }

    pub fn backend_arc(&self) -> Arc<dyn StoreBackend> {
        self.backend.clone()
    }

    pub fn default_indexing_policy(&self) -> &IndexingPolicyConfig {
        &self.default_indexing_policy
    }

    pub fn default_policy_version(&self) -> &str {
        &self.default_policy_version
    }

    pub async fn resolve_store_id(&self, name: &str) -> Result<String, Error> {
        match self.backend.get_store_by_name(name).await? {
            Some(row) => Ok(row.id),
            None => Err(Error::StoreNotFound {
                id: name.to_string(),
            }),
        }
    }
}

pub(crate) fn default_store_row(name: &str, db: &AppDb) -> Result<StoreRow, Error> {
    store_factory::default_store_row(
        name,
        StoreVisibility::Private,
        db.default_indexing_policy(),
        db.default_policy_version(),
    )
}

pub(crate) async fn open_app_db_from_loader(config_loader: &ConfigLoader) -> Result<AppDb, Error> {
    AppDb::open(
        &config_loader.paths,
        &config_loader.config.defaults.indexing.embedding,
        &config_loader.config.providers,
        config_loader.config.defaults.indexing.clone(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Common setup helpers
// ---------------------------------------------------------------------------

/// Load config and open the AppDb. Exits on failure.
///
/// SQLite WAL mode allows concurrent readers and writers, so the DB can be
/// opened directly regardless of whether the daemon is also running. Commands
/// that detect a running daemon will route mutations through the HTTP API;
/// they still open the real DB for read operations (store list, etc.).
pub(crate) async fn load_app_db(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options, ctx.config_env.as_deref()) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    };

    let db = match open_app_db_from_loader(&config_loader).await {
        Ok(d) => d,
        Err(e) => exit_err(&e, ctx.json),
    };
    (config_loader, db)
}

/// F1-cli: Load config with fallback to platform defaults for read-only commands.
///
/// When the config file is malformed or unreadable, read-only commands (search,
/// store list, status) should still work using platform default config and an
/// empty temp DB, rather than hard failing.
pub(crate) async fn load_app_db_lenient(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options, ctx.config_env.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            // If the intended config file exists, it's malformed — fail hard (exit 2).
            let config_path = resolve_config_path(&options, ctx.config_env.as_deref());
            if matches!(&config_path, Ok(p) if p.exists()) {
                exit_err(&e, ctx.json);
            }
            // File genuinely absent — try platform default config.
            let options_default = LoadOptions::default();
            match load_config(&options_default, None) {
                Ok(c) => c,
                Err(_) => {
                    // Platform default also absent — construct minimal fallback ConfigLoader
                    // using platform paths and empty config. `store list` etc. will open/create
                    // a fresh DB at the platform data dir and show 0 results.
                    match localdb_core::config::PlatformPaths::resolve() {
                        Some(platform) => {
                            let config = load_config_from_str("version: 1\n")
                                .expect("minimal config is always valid");
                            ConfigLoader {
                                config,
                                paths: ResolvedPaths {
                                    config_file: platform.config_file,
                                    data_dir: platform.data_dir,
                                    models_dir: platform.models_dir,
                                    logs_dir: platform.logs_dir,
                                },
                            }
                        }
                        None => exit_err(
                            &localdb_core::Error::InvalidConfig {
                                message: "cannot determine platform paths (no home directory)"
                                    .to_string(),
                            },
                            ctx.json,
                        ),
                    }
                }
            }
        }
    };

    let db = match open_app_db_from_loader(&config_loader).await {
        Ok(d) => d,
        Err(Error::RuntimeStateLocked) => {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match open_app_db_from_loader(&config_loader).await {
                Ok(d) => d,
                Err(Error::RuntimeStateLocked) => exit_err(&Error::RuntimeStateLocked, ctx.json),
                Err(e) => exit_err(&e, ctx.json),
            }
        }
        Err(e) => exit_err(&e, ctx.json),
    };
    (config_loader, db)
}

/// Resolve the target store name from --store flags or runtime DB.
pub(crate) async fn resolve_store_name(ctx: &CliContext, db: &AppDb) -> String {
    if let Some(name) = ctx.stores.first() {
        return name.clone();
    }
    match db.backend().list_stores().await {
        Ok(stores) if !stores.is_empty() => stores[0].name.clone(),
        Ok(_) => exit_err(
            &Error::InvalidRequest {
                message: "no stores; run `localdb store add <name>` or pass --store".to_string(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::config::schema::{DefaultsConfig, PathsConfig, RawConfig, ServerConfig};
    use localdb_core::{ids::new_ulid, ingestion::now_rfc3339, types::SourceKind, SourceRow};
    use tempfile::TempDir;

    async fn tmp_app_db(dir: &TempDir) -> AppDb {
        let mut defaults = DefaultsConfig::default();
        defaults.indexing.embedding = EmbeddingPolicy {
            provider: "fake".into(),
            model: "default".into(),
        };
        let config = RawConfig {
            version: 1,
            server: ServerConfig::default(),
            paths: PathsConfig::default(),
            defaults,
            providers: vec![],
        };
        let paths = ResolvedPaths {
            config_file: dir.path().join("config.yaml"),
            data_dir: dir.path().to_path_buf(),
            models_dir: dir.path().join("models"),
            logs_dir: dir.path().join("logs"),
        };
        AppDb::open(
            &paths,
            &config.defaults.indexing.embedding,
            &config.providers,
            config.defaults.indexing.clone(),
        )
        .await
        .unwrap()
    }

    fn test_store_row(name: &str, db: &AppDb) -> StoreRow {
        default_store_row(name, db).unwrap()
    }

    fn test_source_row(store_id: &str, root: &str) -> SourceRow {
        SourceRow {
            id: new_ulid(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            root: Some(root.to_string()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: now_rfc3339(),
        }
    }

    #[tokio::test]
    async fn app_db_store_add_list_remove() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        assert!(db.backend().list_stores().await.unwrap().is_empty());
        let store = test_store_row("mystore", &db);
        let id = store.id.clone();
        db.backend().upsert_store(&store).await.unwrap();
        let stores = db.backend().list_stores().await.unwrap();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].name, "mystore");
        assert!(db.backend().delete_store(&id).await.unwrap());
    }

    #[tokio::test]
    async fn app_db_source_upsert_list_delete() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store = test_store_row("s1", &db);
        db.backend().upsert_store(&store).await.unwrap();
        let store_id = db.resolve_store_id("s1").await.unwrap();
        let src = test_source_row(&store_id, "/tmp");
        db.backend().upsert_source(&src).await.unwrap();
        let list = db.backend().list_sources(&store_id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, src.id);
        assert!(db.backend().delete_source(&src.id).await.unwrap());
    }
}
