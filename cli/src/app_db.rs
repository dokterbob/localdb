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

/// Load config only — no store open, no embedder construction.
///
/// Used exclusively by the `db status`/`db migrate`/`db downgrade`
/// maintenance commands (specs/05-surfaces.md §2.1). Those commands must
/// never go through `AppDb::open`/`SqliteBackend::open`: `LibsqlDb::open`
/// refuses on a schema-version mismatch, which is exactly the state these
/// commands exist to repair. They must also never construct an embedder —
/// the default `local` provider would trigger a one-time ~706 MB model
/// download just to inspect or migrate a store's schema. `embed::infer_dim_encoding`
/// gives the `(embedding_dim, encoding)` pair a `MigrationContext` needs from
/// config alone, the same cheap static lookup `AppDb::open` itself uses
/// before ever touching the embedder.
pub(crate) fn load_config_for_maintenance(ctx: &CliContext) -> ConfigLoader {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    match load_config(&options, ctx.config_env.as_deref()) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// Name of the implicit default store used by `DefaultStore`-scoped commands
/// (`source add`/`list`/`remove`) when no `--store` flag is given.
pub(crate) const DEFAULT_STORE_NAME: &str = "default";

/// How a command resolves its target store(s) when no explicit `--store`
/// flags narrow the scope.
pub(crate) enum StoreScopePolicy {
    /// No `--store` -> every store in the database (search, index, status).
    AllStores,
    /// No `--store` -> exactly the store named "default" (source add/list/remove).
    DefaultStore,
}

/// Reject `--store` on commands that operate on the whole database file
/// (`db status`/`db migrate`/`db downgrade`), per specs/05-surfaces.md §2.2.
///
/// This is a standalone, `AppDb`-free counterpart to `resolve_store_scope`
/// rather than a third `StoreScopePolicy` variant: those commands never open
/// an `AppDb` (see `load_config_for_maintenance`'s doc comment above), so
/// they have no handle to pass the async resolver. Exits the process (via
/// `exit_err`) on error; see `reject_store_flag_inner` for the pure check.
pub(crate) fn reject_store_flag(ctx: &CliContext) {
    if let Err(e) = reject_store_flag_inner(ctx) {
        exit_err(&e, ctx.json);
    }
}

/// Pure decision logic behind `reject_store_flag`, factored out so the
/// rejection can be unit-tested without going through `exit_err`'s
/// `process::exit`.
fn reject_store_flag_inner(ctx: &CliContext) -> Result<(), Error> {
    if ctx.stores.is_empty() {
        return Ok(());
    }
    Err(Error::InvalidRequest {
        message: "`db` commands operate on the whole database file; --store is not applicable"
            .to_string(),
    })
}

/// Resolve a store-scope policy against a running daemon's own store set,
/// genuinely asking it rather than treating it as a rubber stamp.
///
/// Used by every daemon-routing path that needs a store scope (`source add`'s
/// daemon branch, `index`'s daemon branch): the daemon — not this process —
/// is the authority on which stores exist.
///
/// A running daemon need not share our database at all: `LOCALDB_DAEMON_URL`
/// (see `CliContext::daemon_url`) can point at a daemon on another host with
/// its own data directory, in which case a local `StoreRow` lookup would
/// reject perfectly valid store names (or, worse, silently resolve an
/// all-stores/default-store scope against the *wrong* store set). So this
/// walks `GET {base_url}/v1/stores`, paginating to exhaustion — `PaginatedList`
/// truncates each page to `default_limit()` (20), so a single unpaginated
/// call would quietly drop stores 21+ from an all-stores scope — and resolves
/// the policy against the daemon's answer.
///
/// Exits the process (via `exit_err`) on any error; see
/// `resolve_daemon_store_scope_inner` for the pure decision logic.
pub(crate) async fn resolve_daemon_store_scope(
    base_url: &str,
    ctx: &CliContext,
    policy: StoreScopePolicy,
) -> Vec<String> {
    match resolve_daemon_store_scope_inner(base_url, ctx, policy).await {
        Ok(names) => names,
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// Fetch every store name the daemon knows about, following `next_cursor` to
/// exhaustion.
///
/// Guards against a non-advancing cursor (a malformed or hostile response)
/// with `Error::Internal` so a broken daemon response can't spin this loop
/// forever.
async fn fetch_all_daemon_store_names(base_url: &str) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let url = match &cursor {
            Some(c) => format!("{base_url}/v1/stores?cursor={c}"),
            None => format!("{base_url}/v1/stores"),
        };
        let resp =
            crate::daemon_client::daemon_request_async(reqwest::Method::GET, &url, None).await?;
        let items = resp
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for item in &items {
            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                names.push(name.to_string());
            }
        }
        let next_cursor = resp
            .get("next_cursor")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        match next_cursor {
            None => return Ok(names),
            Some(next) => {
                if cursor.as_deref() == Some(next.as_str()) {
                    return Err(Error::Internal {
                        message: format!(
                            "daemon returned a non-advancing pagination cursor '{next}' for \
                             GET /v1/stores"
                        ),
                        correlation_id: "daemon_store_scope_cursor".to_string(),
                    });
                }
                cursor = Some(next);
            }
        }
    }
}

/// Pure-ish decision logic behind `resolve_daemon_store_scope` (the only
/// non-purity is the daemon HTTP round trip itself), factored out so the
/// branch logic is unit-testable independent of `exit_err`'s `process::exit`.
///
/// Order matters: names are syntax-validated *before* any network call (a
/// malformed name never hits the wire), then the daemon's full store list is
/// fetched exactly once, then the policy is applied against it.
///
/// The implicit-vs-explicit distinction (Codex review round 2, finding 4):
/// an *implicit* `default` (no `--store` given, `DefaultStore` policy) that
/// the daemon doesn't have is `Error::InvalidRequest`, exit 2 — matching
/// `resolve_store_scope_inner`'s embedded-mode message exactly. An *explicit*
/// `--store default` (or any other name) absent from the daemon's list is
/// `Error::StoreNotFound`, exit 3, same as any other unknown explicit name —
/// collapsing these two into one case was the reviewer's framing error.
async fn resolve_daemon_store_scope_inner(
    base_url: &str,
    ctx: &CliContext,
    policy: StoreScopePolicy,
) -> Result<Vec<String>, Error> {
    for name in &ctx.stores {
        crate::normalize::validate_store_name(name)?;
    }

    let daemon_names = fetch_all_daemon_store_names(base_url).await?;

    if !ctx.stores.is_empty() {
        let mut names: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for name in &ctx.stores {
            if !daemon_names.iter().any(|n| n == name) {
                return Err(Error::StoreNotFound { id: name.clone() });
            }
            if seen.insert(name.as_str()) {
                names.push(name.clone());
            }
        }
        return Ok(names);
    }

    match policy {
        StoreScopePolicy::AllStores => {
            if daemon_names.is_empty() {
                return Err(Error::InvalidRequest {
                    message: "no stores; run `localdb store add <name>` or pass --store"
                        .to_string(),
                });
            }
            Ok(daemon_names)
        }
        StoreScopePolicy::DefaultStore => {
            if daemon_names.iter().any(|n| n == DEFAULT_STORE_NAME) {
                Ok(vec![DEFAULT_STORE_NAME.to_string()])
            } else {
                Err(Error::InvalidRequest {
                    message: "no store named 'default'; pass --store <name>".to_string(),
                })
            }
        }
    }
}

/// Resolve the set of stores a command should operate on, from `--store`
/// flags and/or the resolution policy. Exits the process (via `exit_err`) on
/// any error; see `resolve_store_scope_inner` for the pure decision logic.
pub(crate) async fn resolve_store_scope(
    ctx: &CliContext,
    db: &AppDb,
    policy: StoreScopePolicy,
) -> Vec<StoreRow> {
    match resolve_store_scope_inner(ctx, db, policy).await {
        Ok(rows) => rows,
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// Pure decision logic behind `resolve_store_scope`, factored out so each
/// branch can be unit-tested without going through `exit_err`'s
/// `process::exit`.
async fn resolve_store_scope_inner(
    ctx: &CliContext,
    db: &AppDb,
    policy: StoreScopePolicy,
) -> Result<Vec<StoreRow>, Error> {
    for name in &ctx.stores {
        crate::normalize::validate_store_name(name)?;
    }

    if !ctx.stores.is_empty() {
        let mut rows: Vec<StoreRow> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for name in &ctx.stores {
            let row = db
                .backend()
                .get_store_by_name(name)
                .await?
                .ok_or_else(|| Error::StoreNotFound { id: name.clone() })?;
            if seen_ids.insert(row.id.clone()) {
                rows.push(row);
            }
        }
        return Ok(rows);
    }

    match policy {
        StoreScopePolicy::AllStores => {
            let stores = db.backend().list_stores().await?;
            if stores.is_empty() {
                return Err(Error::InvalidRequest {
                    message: "no stores; run `localdb store add <name>` or pass --store"
                        .to_string(),
                });
            }
            Ok(stores)
        }
        StoreScopePolicy::DefaultStore => {
            match db.backend().get_store_by_name(DEFAULT_STORE_NAME).await? {
                Some(row) => Ok(vec![row]),
                None => Err(Error::InvalidRequest {
                    message: "no store named 'default'; pass --store <name>".to_string(),
                }),
            }
        }
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
            config_json: None,
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

    fn test_ctx(stores: Vec<&str>) -> CliContext {
        CliContext {
            config: None,
            json: false,
            stores: stores.into_iter().map(String::from).collect(),
            yes: false,
            daemon_url: None,
            config_env: None,
        }
    }

    #[test]
    fn reject_store_flag_inner_with_store_errors() {
        let ctx = test_ctx(vec!["a"]);
        let err = reject_store_flag_inner(&ctx).unwrap_err();
        assert_eq!(
            err,
            Error::InvalidRequest {
                message:
                    "`db` commands operate on the whole database file; --store is not applicable"
                        .to_string(),
            }
        );
    }

    #[test]
    fn reject_store_flag_inner_without_store_is_ok() {
        let ctx = test_ctx(vec![]);
        assert!(reject_store_flag_inner(&ctx).is_ok());
    }

    #[tokio::test]
    async fn scope_explicit_names_resolved_in_order_and_deduped() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let a = test_store_row("a", &db);
        let b = test_store_row("b", &db);
        db.backend().upsert_store(&a).await.unwrap();
        db.backend().upsert_store(&b).await.unwrap();

        let ctx = test_ctx(vec!["a", "b", "a"]);
        let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a");
        assert_eq!(rows[1].name, "b");
    }

    #[tokio::test]
    async fn scope_explicit_unknown_name_errors_store_not_found() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let ctx = test_ctx(vec!["nope"]);
        let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::StoreNotFound {
                id: "nope".to_string()
            }
        );
    }

    #[tokio::test]
    async fn scope_explicit_traversal_name_rejected_by_validate_store_name() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let ctx = test_ctx(vec!["../evil"]);
        let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[tokio::test]
    async fn scope_all_stores_empty_errors_no_stores() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let ctx = test_ctx(vec![]);
        let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::InvalidRequest {
                message: "no stores; run `localdb store add <name>` or pass --store".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn scope_default_store_missing_with_other_store_present_errors() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let other = test_store_row("other", &db);
        db.backend().upsert_store(&other).await.unwrap();

        let ctx = test_ctx(vec![]);
        let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::DefaultStore)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::InvalidRequest {
                message: "no store named 'default'; pass --store <name>".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn scope_default_store_present_returns_it() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let default_row = test_store_row(DEFAULT_STORE_NAME, &db);
        db.backend().upsert_store(&default_row).await.unwrap();

        let ctx = test_ctx(vec![]);
        let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::DefaultStore)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, DEFAULT_STORE_NAME);
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
