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
///
/// A thin wrapper over [`load_config_for_maintenance`] (the config half) and
/// [`open_app_db_or_exit`] (the DB-open half), kept for the callers that
/// still want both steps back-to-back: `job_attach.rs`, `cmds/surface.rs`,
/// `cmds/init.rs`. `command_table::dispatch`'s call sites use the two halves
/// separately instead, so the DB open can be deferred until the daemon probe
/// says `NotRunning` (issue #187 review, finding G4) — see `dispatch`'s doc
/// comment.
pub(crate) async fn load_app_db(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let config_loader = load_config_for_maintenance(ctx);
    let db = open_app_db_or_exit(ctx, &config_loader).await;
    (config_loader, db)
}

/// Open the `AppDb` half of [`load_app_db`]'s strict behavior. Exits on
/// failure via `exit_err`, exactly as `load_app_db` always did — factored
/// out so `command_table::dispatch` call sites can pass it as a lazily-called
/// `open_db` closure instead of opening the DB before the daemon probe (issue
/// #187 review, finding G4): a broken local store (unwritable, locked,
/// schema-too-new) used to `exit_err` here before a healthy daemon ever got a
/// chance to handle the command.
pub(crate) async fn open_app_db_or_exit(ctx: &CliContext, config_loader: &ConfigLoader) -> AppDb {
    match open_app_db_from_loader(config_loader).await {
        Ok(d) => d,
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// F1-cli: Load config with fallback to platform defaults for read-only commands.
///
/// When the config file is malformed or unreadable, read-only commands (search,
/// store list, status) should still work using platform default config and an
/// empty temp DB, rather than hard failing.
///
/// A thin wrapper over [`load_config_lenient`] (the config half) and
/// [`open_app_db_lenient_or_exit`] (the DB-open half) — see `load_app_db`'s
/// doc comment for why `command_table::dispatch` call sites use the two
/// halves separately instead.
pub(crate) async fn load_app_db_lenient(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let config_loader = load_config_lenient(ctx);
    let db = open_app_db_lenient_or_exit(ctx, &config_loader).await;
    (config_loader, db)
}

/// The config-only half of [`load_app_db_lenient`]'s fallback-to-platform-defaults
/// behavior — see that function's doc comment. Factored out so
/// `command_table::dispatch` call sites can select their DB-open moment
/// independently (issue #187 review, finding G4).
pub(crate) fn load_config_lenient(ctx: &CliContext) -> ConfigLoader {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    match load_config(&options, ctx.config_env.as_deref()) {
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
    }
}

/// The DB-open half of [`load_app_db_lenient`], including its
/// `RuntimeStateLocked` 100ms-retry. Exits on failure via `exit_err`, exactly
/// as `load_app_db_lenient` always did — factored out for the same reason as
/// `open_app_db_or_exit` (issue #187 review, finding G4): so
/// `command_table::dispatch` call sites can defer the open until the daemon
/// probe comes back `NotRunning`.
pub(crate) async fn open_app_db_lenient_or_exit(
    ctx: &CliContext,
    config_loader: &ConfigLoader,
) -> AppDb {
    match open_app_db_from_loader(config_loader).await {
        Ok(d) => d,
        Err(Error::RuntimeStateLocked) => {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match open_app_db_from_loader(config_loader).await {
                Ok(d) => d,
                Err(Error::RuntimeStateLocked) => exit_err(&Error::RuntimeStateLocked, ctx.json),
                Err(e) => exit_err(&e, ctx.json),
            }
        }
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// Load config only — no store open, no embedder construction.
///
/// Used by the `db status`/`db migrate`/`db downgrade` maintenance commands
/// (specs/05-surfaces.md §2.1) — those commands must never go through
/// `AppDb::open`/`SqliteBackend::open`: `LibsqlDb::open` refuses on a
/// schema-version mismatch, which is exactly the state these commands exist
/// to repair. They must also never construct an embedder — the default
/// `local` provider would trigger a one-time ~706 MB model download just to
/// inspect or migrate a store's schema. `embed::infer_dim_encoding` gives the
/// `(embedding_dim, encoding)` pair a `MigrationContext` needs from config
/// alone, the same cheap static lookup `AppDb::open` itself uses before ever
/// touching the embedder.
///
/// Also used as the config-loading half of every strict `command_table::dispatch`
/// call site (issue #187 review, finding G4): those call sites pair this with
/// `open_app_db_or_exit`, called lazily from inside `dispatch`'s `open_db`
/// closure, so a broken local store never preempts a healthy daemon — the
/// same config-only/DB-open split this function was already built for.
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
/// (`source add` and its `add` alias) when no `--store` flag is given.
pub(crate) const DEFAULT_STORE_NAME: &str = "default";

/// How a command resolves its target store(s) when no explicit `--store`
/// flags narrow the scope (specs/05-surfaces.md §2.2).
///
/// This only ever governs the *omitted*-`--store` case: an explicit `-s` is
/// validated and resolved identically under every variant.
pub(crate) enum StoreScopePolicy {
    /// No `--store` -> every store in the database; a database with no stores
    /// at all is exit 2 (`status`, `store list`, `source list`,
    /// `source remove`, `index`).
    AllStores,
    /// No `--store` -> every store in the database; a database with no stores
    /// resolves to an *empty* scope rather than an error (`search`, `mcp`).
    ///
    /// The two commands that need this are the two whose empty answer is a
    /// correct answer: `search` on a fresh install has no results, and an MCP
    /// server that exits non-zero at startup reads as a *broken* server to
    /// its client rather than as an empty one. Every other all-stores command
    /// would be doing silently nothing, which §2.2 makes exit 2.
    AllStoresAllowEmpty,
    /// No `--store` -> exactly the store named "default" (`source add` and
    /// its `add` alias — the one write that must pick a single target).
    DefaultStore,
}

/// Reject `--store` on commands that are not store-scoped at all, per
/// specs/05-surfaces.md §2.2: `db status`/`migrate`/`downgrade`/`vacuum` (they operate
/// on the whole database file), `store add`/`store remove` (the store is
/// named by the command's own argument), `init` (there is no store concept
/// yet) and `serve` (the daemon serves every store regardless).
///
/// `message` is the caller's own explanation of why the flag doesn't apply —
/// it is the entire user-visible error text, so it must read as a complete
/// sentence on its own.
///
/// This is a standalone, `AppDb`-free counterpart to `resolve_store_scope`
/// rather than a fourth `StoreScopePolicy` variant: several of these commands
/// never open an `AppDb` at all (see `load_config_for_maintenance`'s doc
/// comment above), so they have no handle to pass the async resolver, and the
/// rest must reject *before* opening one so misuse never has a side effect.
/// Exits the process (via `exit_err`) on error; see `reject_store_flag_inner`
/// for the pure check.
pub(crate) fn reject_store_flag(ctx: &CliContext, message: &str) {
    if let Err(e) = reject_store_flag_inner(ctx, message) {
        exit_err(&e, ctx.json);
    }
}

/// The `--store`-rejection message for each command that isn't store-scoped.
///
/// Kept together rather than inline at the call sites so the whole family is
/// auditable in one place against specs/05-surfaces.md §2.2's table — these
/// five are the complete set, and each says *why* the flag doesn't apply
/// rather than only that it doesn't.
///
/// `DB_REJECT_MESSAGE` is load-bearing beyond its wording: it is asserted on
/// verbatim by `db_migrate_with_store_flag_exits_2`
/// (`localdb/tests/cli_integration.rs`), so it must not drift.
pub(crate) const DB_REJECT_MESSAGE: &str =
    "`db` commands operate on the whole database file; --store is not applicable";
pub(crate) const STORE_ADD_REJECT_MESSAGE: &str =
    "`store add` names its store as an argument; --store is not applicable";
pub(crate) const STORE_REMOVE_REJECT_MESSAGE: &str =
    "`store remove` names its store as an argument; --store is not applicable";
pub(crate) const INIT_REJECT_MESSAGE: &str =
    "`init` creates the config and data directory before any store exists; --store is not applicable";
pub(crate) const SERVE_REJECT_MESSAGE: &str =
    "`serve` serves every store in the database; --store is not applicable";

/// Pure decision logic behind `reject_store_flag`, factored out so the
/// rejection can be unit-tested without going through `exit_err`'s
/// `process::exit`.
fn reject_store_flag_inner(ctx: &CliContext, message: &str) -> Result<(), Error> {
    if ctx.stores.is_empty() {
        return Ok(());
    }
    Err(Error::InvalidRequest {
        message: message.to_string(),
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
/// Delegates the actual HTTP walk to `daemon_client::walk_daemon_pages`,
/// shared with `cmds::index::daemon_store_has_source`'s owner walk: it bails
/// with `Error::Internal` on a malformed page shape, on *any* repeated
/// pagination cursor (not just an immediate repeat — a daemon alternating
/// between two or more cursors is caught too), and on an absolute page-count
/// cap, so a broken or hostile daemon response can't spin this loop forever.
async fn fetch_all_daemon_store_names(base_url: &str) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    crate::daemon_client::walk_daemon_pages(base_url, "/v1/stores", |items| {
        for item in items {
            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                names.push(name.to_string());
            }
        }
        false
    })
    .await?;
    Ok(names)
}

/// Pure-ish decision logic behind `resolve_daemon_store_scope` (the only
/// non-purity is the daemon HTTP round trip itself), factored out so the
/// branch logic is unit-testable independent of `exit_err`'s `process::exit`.
///
/// Order matters: names are syntax-validated *before* any network call (a
/// malformed name never hits the wire), then the daemon's full store list is
/// fetched exactly once, then [`apply_daemon_store_scope`] applies the policy
/// against it.
///
/// `pub(crate)` (rather than private) so `DaemonAwareCommand::run_daemon`
/// implementations that need a `Result`-returning store-name scope (e.g.
/// `cmds::source::SourceListCmd`) can use it directly instead of going
/// through `resolve_daemon_store_scope`'s `exit_err`-on-error wrapper —
/// `dispatch` is what owns the exit point for those.
pub(crate) async fn resolve_daemon_store_scope_inner(
    base_url: &str,
    ctx: &CliContext,
    policy: StoreScopePolicy,
) -> Result<Vec<String>, Error> {
    // Finding 5 (Codex review): validate before the network round trip, not
    // just before the match against its result. `apply_daemon_store_scope`
    // below validates too (defense in depth for its other direct callers),
    // but that check runs *after* `fetch_all_daemon_store_names` — too late
    // to keep a malformed name from tripping `DaemonUnreachable` (exit 5)
    // against an unreachable daemon instead of `InvalidRequest` (exit 2), as
    // this function's own doc comment above promises. Mirrors
    // `SourceRemoveCmd::run_daemon`'s validate-before-I/O loop in
    // `cmds/source.rs`.
    for name in &ctx.stores {
        crate::normalize::validate_store_name(name)?;
    }
    let daemon_names = fetch_all_daemon_store_names(base_url).await?;
    apply_daemon_store_scope(&daemon_names, |n| n.as_str(), ctx, policy)
}

/// Apply a store-scope policy to an already-fetched list of daemon store
/// names/records — no additional network round trip.
///
/// Shared by every daemon-routed command that needs to filter a full store
/// list by `--store` (`resolve_daemon_store_scope_inner` above; `store
/// list`'s and `status`'s daemon branches, issue #187 stage 5): the single
/// point where "how do we interpret `ctx.stores` against what the daemon
/// has" is decided, so the interpretation can never drift between commands
/// the way the nine hand-rolled branches in issue #187 §2 did. `name_of`
/// projects whatever record type `T` a caller has (a bare `String` for the
/// store-scope resolvers, a richer `StoreRecord`-shaped DTO for `store
/// list`/`status`) down to the name this function matches `--store` against.
///
/// Order matters: names are syntax-validated *before* being matched against
/// `items` (a malformed name is `Error::InvalidRequest`, exit 2, regardless
/// of what the daemon actually has).
///
/// The implicit-vs-explicit distinction (Codex review round 2, finding 4):
/// an *implicit* `default` (no `--store` given, `DefaultStore` policy) that
/// the daemon doesn't have is `Error::InvalidRequest`, exit 2 — matching
/// `resolve_store_scope_inner`'s embedded-mode message exactly. An *explicit*
/// `--store default` (or any other name) absent from `items` is
/// `Error::StoreNotFound`, exit 3, same as any other unknown explicit name —
/// collapsing these two into one case was the reviewer's framing error.
pub(crate) fn apply_daemon_store_scope<T: Clone>(
    items: &[T],
    name_of: impl Fn(&T) -> &str,
    ctx: &CliContext,
    policy: StoreScopePolicy,
) -> Result<Vec<T>, Error> {
    for name in &ctx.stores {
        crate::normalize::validate_store_name(name)?;
    }

    if !ctx.stores.is_empty() {
        let mut result: Vec<T> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for name in &ctx.stores {
            let item = items
                .iter()
                .find(|it| name_of(it) == name.as_str())
                .ok_or_else(|| Error::StoreNotFound { id: name.clone() })?;
            if seen.insert(name.as_str()) {
                result.push(item.clone());
            }
        }
        return Ok(result);
    }

    match policy {
        StoreScopePolicy::AllStores => {
            if items.is_empty() {
                return Err(Error::InvalidRequest {
                    message: "no stores; run `localdb store add <name>` or pass --store"
                        .to_string(),
                });
            }
            Ok(items.to_vec())
        }
        StoreScopePolicy::AllStoresAllowEmpty => Ok(items.to_vec()),
        StoreScopePolicy::DefaultStore => {
            match items.iter().find(|it| name_of(it) == DEFAULT_STORE_NAME) {
                Some(item) => Ok(vec![item.clone()]),
                None => Err(Error::InvalidRequest {
                    message: "no store named 'default'; pass --store <name>".to_string(),
                }),
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
///
/// `pub(crate)` rather than private because `cmds::search`'s
/// `SearchCmd::run_embedded` needs the `Result`-returning form: it is itself
/// fallible and returns to `command_table::dispatch`, which owns the
/// `exit_err` call — going through the exiting wrapper here would move the
/// exit point out of `dispatch` where it belongs.
pub(crate) async fn resolve_store_scope_inner(
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
        StoreScopePolicy::AllStoresAllowEmpty => Ok(db.backend().list_stores().await?),
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
        let err = reject_store_flag_inner(&ctx, DB_REJECT_MESSAGE).unwrap_err();
        assert_eq!(
            err,
            Error::InvalidRequest {
                message:
                    "`db` commands operate on the whole database file; --store is not applicable"
                        .to_string(),
            }
        );
    }

    /// The caller's `message` is the entire user-visible error text — the
    /// helper never prefixes or rewrites it, which is what lets one function
    /// serve `db`, `store add`/`remove`, `init` and `serve` with four
    /// different explanations.
    #[test]
    fn reject_store_flag_inner_uses_the_callers_message_verbatim() {
        let ctx = test_ctx(vec!["a"]);
        let err = reject_store_flag_inner(&ctx, "totally bespoke explanation").unwrap_err();
        assert_eq!(
            err,
            Error::InvalidRequest {
                message: "totally bespoke explanation".to_string(),
            }
        );
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn reject_store_flag_inner_without_store_is_ok() {
        let ctx = test_ctx(vec![]);
        assert!(reject_store_flag_inner(&ctx, DB_REJECT_MESSAGE).is_ok());
    }

    /// `AllStoresAllowEmpty` is the one all-stores policy that resolves a
    /// zero-store database to an empty scope instead of exit 2 — the
    /// difference `search`/`mcp` depend on (specs/05-surfaces.md §2.2).
    #[tokio::test]
    async fn scope_all_stores_allow_empty_resolves_empty_scope() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let ctx = test_ctx(vec![]);
        let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStoresAllowEmpty)
            .await
            .expect("an empty database must resolve, not error, under AllStoresAllowEmpty");
        assert!(rows.is_empty());
    }

    /// `AllStoresAllowEmpty` differs from `AllStores` *only* in the
    /// empty-database case: with stores present it still spans all of them.
    #[tokio::test]
    async fn scope_all_stores_allow_empty_still_spans_every_store() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        for name in ["a", "b"] {
            let row = test_store_row(name, &db);
            db.backend().upsert_store(&row).await.unwrap();
        }
        let ctx = test_ctx(vec![]);
        let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStoresAllowEmpty)
            .await
            .unwrap();
        let names: std::collections::HashSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a", "b"].into_iter().collect());
    }

    /// An explicit unknown `-s` is still exit 3 under `AllStoresAllowEmpty` —
    /// "allow empty" relaxes only the *omitted*-`-s` case, never validation.
    #[tokio::test]
    async fn scope_all_stores_allow_empty_still_rejects_unknown_explicit_name() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let ctx = test_ctx(vec!["nope"]);
        let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStoresAllowEmpty)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::StoreNotFound {
                id: "nope".to_string()
            }
        );
        assert_eq!(err.exit_code(), 3);
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

    /// Finding 5 (Codex review): an invalid `--store` name must be rejected
    /// as `Error::InvalidRequest` (exit 2) *before* the daemon store list is
    /// fetched — the daemon base URL here (`127.0.0.1:0`) is guaranteed
    /// connection-refused (see `daemon_client::tests::probe_stale_removes_both_socket_and_url_file`
    /// for the same idiom), so if validation ran after the fetch this would
    /// surface `Error::DaemonUnreachable` (exit 5) instead — exactly the
    /// ordering bug the function's doc comment already promised was fixed.
    #[tokio::test]
    async fn resolve_daemon_store_scope_inner_validates_before_fetching() {
        let ctx = test_ctx(vec!["../bad"]);
        let err = resolve_daemon_store_scope_inner(
            "http://127.0.0.1:0",
            &ctx,
            StoreScopePolicy::AllStores,
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            matches!(err, Error::InvalidRequest { .. }),
            "expected InvalidRequest, got {err:?}"
        );
    }

    /// Pin the empty-`--store` (no flags passed) daemon-scope behavior: with
    /// nothing to validate, the call proceeds straight to the daemon fetch,
    /// so an unreachable daemon still surfaces as `DaemonUnreachable` (exit
    /// 5) rather than being reinterpreted as a validation error.
    #[tokio::test]
    async fn resolve_daemon_store_scope_inner_empty_stores_still_reaches_daemon() {
        let ctx = test_ctx(vec![]);
        let err = resolve_daemon_store_scope_inner(
            "http://127.0.0.1:0",
            &ctx,
            StoreScopePolicy::AllStores,
        )
        .await
        .unwrap_err();
        assert_eq!(err, Error::DaemonUnreachable);
        assert_eq!(err.exit_code(), 5);
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
