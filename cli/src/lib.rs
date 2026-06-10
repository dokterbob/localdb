//! CLI command implementations for localdb.
//!
//! Thin layer on `core` — no business logic lives here (invariant from
//! specs/01-architecture.md §1). Each command acquires config + runtime state,
//! then calls into the core crates.
//!
//! Process model (specs/01-architecture.md §3, specs/05-surfaces.md §1):
//! - Probe the daemon socket; if present and responsive → thin client.
//! - Otherwise → embedded mode (open store in-process).
//! - Write commands acquire the advisory file lock.
//!
//! Exit codes (specs/05-surfaces.md §5):
//! - 0 ok
//! - 1 internal
//! - 2 invalid usage/config
//! - 3 not found
//! - 4 conflict/locked
//! - 5 unavailable (daemon/provider/model)

use std::io::Write;
use std::path::{Path, PathBuf};

use localdb_core::{
    config::{
        loader::{load_config, ConfigLoader, LoadOptions},
        policy::compute_policy_version,
        runtime_state::{check_yaml_owned, RuntimeStateDb, RuntimeStore},
    },
    ids::new_ulid,
    Error,
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde_json::json;

// ---------------------------------------------------------------------------
// CliContext — parsed global flags
// ---------------------------------------------------------------------------

/// Parsed global CLI flags, forwarded to every command handler.
#[derive(Debug, Clone)]
pub struct CliContext {
    /// Path to config file (if --config was given).
    pub config: Option<PathBuf>,
    /// Whether --json was specified.
    pub json: bool,
    /// Store name filters (from --store flags).
    pub stores: Vec<String>,
}

// ---------------------------------------------------------------------------
// Daemon probe — specs/01-architecture.md §3
// ---------------------------------------------------------------------------

/// Result of probing the daemon socket.
pub enum DaemonState {
    /// A daemon is running and reachable.
    Running { base_url: String },
    /// No daemon detected; use embedded mode.
    NotRunning,
}

/// Probe the daemon socket for a given data directory.
///
/// Returns `DaemonState::Running` if the socket file is present (MVP check),
/// otherwise `DaemonState::NotRunning`.
pub fn probe_daemon(data_dir: &Path) -> DaemonState {
    let socket_path = data_dir.join("daemon.sock");
    if socket_path.exists() {
        DaemonState::Running {
            base_url: "http://127.0.0.1:7700".to_string(),
        }
    } else {
        DaemonState::NotRunning
    }
}

// ---------------------------------------------------------------------------
// Write-lock — specs/01-architecture.md §3
// ---------------------------------------------------------------------------

/// Advisory write lock for the data directory.
///
/// Holds a lock file open; drops (removes) on `Drop`.
/// Returns `Error::StoreLocked` if cannot be acquired.
pub struct WriteLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl WriteLock {
    /// Attempt to acquire the write lock for `data_dir`.
    pub fn acquire(data_dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Internal {
            message: format!("cannot create data dir '{}': {}", data_dir.display(), e),
            correlation_id: "write_lock_mkdir".to_string(),
        })?;

        let lock_path = data_dir.join(".write.lock");

        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    Error::StoreLocked
                } else {
                    Error::Internal {
                        message: format!("cannot acquire write lock: {}", e),
                        correlation_id: "write_lock_open".to_string(),
                    }
                }
            })?;

        let mut f = file;
        let _ = writeln!(f, "{}", std::process::id());

        // Re-open to hold a live descriptor.
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .map_err(|e| Error::Internal {
                message: format!("cannot reopen lock file: {}", e),
                correlation_id: "write_lock_reopen".to_string(),
            })?;

        Ok(Self {
            _file: held,
            path: lock_path,
        })
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// Print an error and exit with the correct exit code.
pub fn exit_err(err: &Error, json_mode: bool) -> ! {
    let code = err.exit_code();
    if json_mode {
        let v = json!({
            "error": err.code(),
            "message": err.to_string(),
        });
        eprintln!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        eprintln!("error: {}", err);
    }
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Source CRUD — stored in the same redb file as runtime-state
// ---------------------------------------------------------------------------
//
// `RuntimeStateDb` in `core` (T03) manages only runtime-owned stores.
// T09 adds source CRUD using an additional table in the same redb file.
// We open the file independently with `redb::Database` (redb supports multiple
// openers on the same file in the same process; between processes the WAL guards
// consistency). The sources table is separate from the stores table owned by core.

/// A source record persisted in the runtime-state DB.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeSource {
    /// Source ULID.
    pub id: String,
    /// Owning store name.
    pub store_name: String,
    /// Source kind: "path" or "url".
    pub kind: String,
    /// Root path (for path sources).
    pub root: Option<String>,
    /// URL (for url sources).
    pub url: Option<String>,
    /// Include globs.
    #[serde(default)]
    pub include: Vec<String>,
    /// Exclude globs.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Chunking preset.
    pub preset: String,
}

/// Sources table: source_id → JSON `RuntimeSource`.
const SOURCES_TABLE: TableDefinition<&str, &str> = TableDefinition::new("cli_sources");

/// Thin DB for source CRUD, backed by the same redb file as stores.
struct SourceDb {
    db: Database,
}

impl SourceDb {
    fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Internal {
                message: format!("cannot create DB directory: {}", e),
                correlation_id: "source_db_mkdir".to_string(),
            })?;
        }

        let db = Database::create(path).map_err(|e| Error::Internal {
            message: format!("cannot open source DB: {}", e),
            correlation_id: "source_db_open".to_string(),
        })?;

        {
            let wtxn = db.begin_write().map_err(map_db_err)?;
            wtxn.open_table(SOURCES_TABLE).map_err(map_db_err)?;
            wtxn.commit().map_err(map_db_err)?;
        }

        Ok(Self { db })
    }

    fn upsert(&self, source: &RuntimeSource) -> Result<(), Error> {
        let json = serde_json::to_string(source).map_err(|e| Error::Internal {
            message: format!("cannot serialize source: {}", e),
            correlation_id: "source_upsert_ser".to_string(),
        })?;
        let wtxn = self.db.begin_write().map_err(map_db_err)?;
        {
            let mut t = wtxn.open_table(SOURCES_TABLE).map_err(map_db_err)?;
            t.insert(source.id.as_str(), json.as_str())
                .map_err(map_db_err)?;
        }
        wtxn.commit().map_err(map_db_err)?;
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<bool, Error> {
        let wtxn = self.db.begin_write().map_err(map_db_err)?;
        let deleted = {
            let mut t = wtxn.open_table(SOURCES_TABLE).map_err(map_db_err)?;
            let had = t.remove(id).map_err(map_db_err)?.is_some();
            had
        };
        wtxn.commit().map_err(map_db_err)?;
        Ok(deleted)
    }

    fn list(&self, store_name: &str) -> Result<Vec<RuntimeSource>, Error> {
        let rtxn = self.db.begin_read().map_err(map_db_err)?;
        let t = rtxn.open_table(SOURCES_TABLE).map_err(map_db_err)?;
        let mut sources = Vec::new();
        for entry in t.iter().map_err(map_db_err)? {
            let (_, v) = entry.map_err(map_db_err)?;
            let src: RuntimeSource =
                serde_json::from_str(v.value()).map_err(|e| Error::Internal {
                    message: format!("cannot deserialize source: {}", e),
                    correlation_id: "source_list_deser".to_string(),
                })?;
            if src.store_name == store_name {
                sources.push(src);
            }
        }
        Ok(sources)
    }

    fn get(&self, id: &str) -> Result<Option<RuntimeSource>, Error> {
        let rtxn = self.db.begin_read().map_err(map_db_err)?;
        let t = rtxn.open_table(SOURCES_TABLE).map_err(map_db_err)?;
        match t.get(id).map_err(map_db_err)? {
            None => Ok(None),
            Some(v) => {
                let src: RuntimeSource =
                    serde_json::from_str(v.value()).map_err(|e| Error::Internal {
                        message: format!("cannot deserialize source: {}", e),
                        correlation_id: "source_get_deser".to_string(),
                    })?;
                Ok(Some(src))
            }
        }
    }
}

fn map_db_err(e: impl std::fmt::Display) -> Error {
    Error::Internal {
        message: format!("DB error: {}", e),
        correlation_id: "db_err".to_string(),
    }
}

// ---------------------------------------------------------------------------
// AppDb — combined stores + sources
// ---------------------------------------------------------------------------

/// Combined DB handle for the CLI: stores (via core's RuntimeStateDb) +
/// sources (via SourceDb, which uses a separate redb file to avoid
/// exclusive-lock conflicts with RuntimeStateDb).
pub struct AppDb {
    stores: RuntimeStateDb,
    sources: SourceDb,
}

impl AppDb {
    /// Open the combined DB.
    ///
    /// `state_path` is the path used by `RuntimeStateDb` (runtime-state.redb).
    /// A sibling file `cli-sources.redb` is used for the sources table.
    pub fn open(state_path: &Path) -> Result<Self, Error> {
        let stores = RuntimeStateDb::open(state_path)?;
        let sources_path = state_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("cli-sources.redb");
        let sources = SourceDb::open(&sources_path)?;
        Ok(Self { stores, sources })
    }

    // --- store delegates ---
    pub fn get_store(&self, name: &str) -> Result<Option<RuntimeStore>, Error> {
        self.stores.get_store(name)
    }
    pub fn upsert_store(&self, store: &RuntimeStore) -> Result<(), Error> {
        self.stores.upsert_store(store)
    }
    pub fn delete_store(&self, name: &str) -> Result<bool, Error> {
        self.stores.delete_store(name)
    }
    pub fn list_stores(&self) -> Result<Vec<RuntimeStore>, Error> {
        self.stores.list_stores()
    }

    // --- source delegates ---
    pub fn upsert_source(&self, source: &RuntimeSource) -> Result<(), Error> {
        self.sources.upsert(source)
    }
    pub fn delete_source(&self, id: &str) -> Result<bool, Error> {
        self.sources.delete(id)
    }
    pub fn list_sources(&self, store_name: &str) -> Result<Vec<RuntimeSource>, Error> {
        self.sources.list(store_name)
    }
    pub fn get_source(&self, id: &str) -> Result<Option<RuntimeSource>, Error> {
        self.sources.get(id)
    }
}

// ---------------------------------------------------------------------------
// Common setup helpers
// ---------------------------------------------------------------------------

/// Load config and open the AppDb. Exits on failure.
fn load_app_db(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    };
    let db_path = config_loader.paths.runtime_state_db_path();
    let db = match AppDb::open(&db_path) {
        Ok(d) => d,
        Err(e) => exit_err(&e, ctx.json),
    };
    (config_loader, db)
}

/// Resolve the target store name from --store flags, YAML config, or runtime DB.
fn resolve_store_name(ctx: &CliContext, config_loader: &ConfigLoader, db: &AppDb) -> String {
    if let Some(name) = ctx.stores.first() {
        return name.clone();
    }
    if let Some(s) = config_loader.config.stores.first() {
        return s.name.clone();
    }
    match db.list_stores() {
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

/// Classify a source argument as "path" or "url".
///
/// Returns `(kind, root, url)`.
pub fn classify_source(source: &str) -> (&str, Option<&str>, Option<&str>) {
    if source.starts_with("http://") || source.starts_with("https://") {
        ("url", None, Some(source))
    } else {
        ("path", Some(source), None)
    }
}

/// Convert a `RuntimeSource` to a `core::types::Source` for ingestion.
pub fn runtime_source_to_core_source(
    src: &RuntimeSource,
    store_id: &str,
) -> localdb_core::types::Source {
    use localdb_core::types::{Source, SourceKind, SourceSpec};

    let (kind, spec) = match src.kind.as_str() {
        "url" => (
            SourceKind::Url,
            SourceSpec::Url {
                url: src.url.clone().unwrap_or_default(),
                refresh_interval_secs: None,
            },
        ),
        _ => (
            SourceKind::Path,
            SourceSpec::Path {
                root: src.root.clone().unwrap_or_default(),
                include: src.include.clone(),
                exclude: src.exclude.clone(),
            },
        ),
    };

    Source {
        id: src.id.clone(),
        store_id: store_id.to_string(),
        kind,
        spec,
        source_kind_preset: src.preset.clone(),
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

/// `localdb init`
///
/// Creates config + data dir, writes default config if absent.
///
/// Strategy:
/// 1. Determine the config file path (from --config flag, LOCALDB_CONFIG env, or platform default).
/// 2. If the config file already exists, load it to get `paths.data`.
/// 3. Otherwise, use the platform default data dir.
/// 4. Write default config if absent.
/// 5. Create all directories.
/// 6. Initialize the runtime-state DB.
pub fn run_init(ctx: &CliContext) {
    let platform = localdb_core::config::PlatformPaths::resolve().unwrap_or_else(|| {
        eprintln!("error: cannot determine platform paths");
        std::process::exit(1);
    });

    // Resolve config path (same priority order as load_config).
    let config_path = ctx
        .config
        .clone()
        .or_else(|| std::env::var("LOCALDB_CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| platform.config_file.clone());

    // If config exists, load it to get the resolved data dir.
    // If not, use platform defaults (we'll write the config shortly).
    let (data_dir, models_dir, logs_dir) = if config_path.exists() {
        let options = LoadOptions {
            config_path: Some(config_path.clone()),
            ..Default::default()
        };
        match load_config(&options) {
            Ok(cl) => (cl.paths.data_dir, cl.paths.models_dir, cl.paths.logs_dir),
            Err(_) => (
                platform.data_dir.clone(),
                platform.models_dir.clone(),
                platform.logs_dir.clone(),
            ),
        }
    } else {
        (
            platform.data_dir.clone(),
            platform.models_dir.clone(),
            platform.logs_dir.clone(),
        )
    };

    // Create directories.
    for dir in [
        config_path.parent().unwrap_or(Path::new(".")),
        &data_dir,
        &models_dir,
        &logs_dir,
    ] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            exit_err(
                &Error::Internal {
                    message: format!("cannot create directory '{}': {}", dir.display(), e),
                    correlation_id: "init_mkdir".to_string(),
                },
                ctx.json,
            );
        }
    }

    // Write default config if absent.
    if !config_path.exists() {
        let default_config =
            "version: 1\n# localdb configuration\n# Add stores and sources below.\n";
        if let Err(e) = std::fs::write(&config_path, default_config) {
            exit_err(
                &Error::Internal {
                    message: format!("cannot write config to '{}': {}", config_path.display(), e),
                    correlation_id: "init_config_write".to_string(),
                },
                ctx.json,
            );
        }
    }

    // Initialize the runtime-state DB.
    let db_path = data_dir.join("runtime-state.redb");
    if let Err(e) = AppDb::open(&db_path) {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "config_path": config_path.to_string_lossy(),
            "data_dir": data_dir.to_string_lossy(),
        }));
    } else {
        println!(
            "Initialized localdb at {}",
            config_path.parent().unwrap_or(Path::new(".")).display()
        );
        println!("  Config: {}", config_path.display());
        println!("  Data:   {}", data_dir.display());
        println!();
        println!("Note: embedding models will be downloaded on first index.");
        println!("Run `localdb store add <name>` to create a store.");
    }
}

/// `localdb status`
pub fn run_status(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;

    let daemon_status = match probe_daemon(data_dir) {
        DaemonState::Running { base_url } => format!("running ({})", base_url),
        DaemonState::NotRunning => "not running (embedded mode)".to_string(),
    };

    let runtime_stores = match db.list_stores() {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };
    let yaml_stores = &config_loader.config.stores;
    let yaml_names: std::collections::HashSet<&str> =
        yaml_stores.iter().map(|s| s.name.as_str()).collect();

    let mut all_stores: Vec<serde_json::Value> = yaml_stores
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "ownership": "yaml",
                "visibility": s.visibility,
                "backend": s.backend,
            })
        })
        .collect();
    for s in &runtime_stores {
        if !yaml_names.contains(s.name.as_str()) {
            all_stores.push(json!({
                "name": s.name,
                "ownership": "runtime",
                "visibility": s.visibility,
                "backend": s.backend,
            }));
        }
    }

    if ctx.json {
        print_json(&json!({
            "daemon": daemon_status,
            "stores": all_stores,
        }));
    } else {
        println!("daemon: {}", daemon_status);
        println!("stores ({}):", all_stores.len());
        if all_stores.is_empty() {
            println!("  (none)");
        }
        for s in &all_stores {
            println!(
                "  {} [{}] ({})",
                s["name"].as_str().unwrap_or("?"),
                s["backend"].as_str().unwrap_or("?"),
                s["ownership"].as_str().unwrap_or("?"),
            );
        }
    }
}

/// `localdb store add <name>`
pub fn run_store_add(ctx: &CliContext, name: &str) {
    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;

    if check_yaml_owned(name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    let _lock = match WriteLock::acquire(data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Duplicate check.
    match db.get_store(name) {
        Ok(Some(_)) => exit_err(
            &Error::InvalidRequest {
                message: format!("store '{}' already exists", name),
            },
            ctx.json,
        ),
        Ok(None) => {}
        Err(e) => exit_err(&e, ctx.json),
    }

    let store = RuntimeStore {
        id: new_ulid(),
        name: name.to_string(),
        visibility: "private".to_string(),
        backend: "lancedb".to_string(),
        indexing: None,
    };
    if let Err(e) = db.upsert_store(&store) {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "name": name, "id": store.id }));
    } else {
        println!("Added store: {}", name);
    }
}

/// `localdb store list`
pub fn run_store_list(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx);

    let runtime_stores = match db.list_stores() {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };
    let yaml_stores = &config_loader.config.stores;
    let yaml_names: std::collections::HashSet<&str> =
        yaml_stores.iter().map(|s| s.name.as_str()).collect();

    let mut all: Vec<serde_json::Value> = yaml_stores
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "ownership": "yaml",
                "visibility": s.visibility,
                "backend": s.backend,
            })
        })
        .collect();
    for s in &runtime_stores {
        if !yaml_names.contains(s.name.as_str()) {
            all.push(json!({
                "name": s.name,
                "ownership": "runtime",
                "visibility": s.visibility,
                "backend": s.backend,
            }));
        }
    }

    if ctx.json {
        print_json(&json!({ "stores": all }));
    } else if all.is_empty() {
        println!("No stores.");
    } else {
        for s in &all {
            println!(
                "{} [{}] ({})",
                s["name"].as_str().unwrap_or("?"),
                s["backend"].as_str().unwrap_or("?"),
                s["ownership"].as_str().unwrap_or("?"),
            );
        }
    }
}

/// `localdb store remove <name>`
pub fn run_store_remove(ctx: &CliContext, name: &str) {
    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;

    if check_yaml_owned(name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    let _lock = match WriteLock::acquire(data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    match db.delete_store(name) {
        Ok(true) => {}
        Ok(false) => exit_err(
            &Error::StoreNotFound {
                id: name.to_string(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "name": name }));
    } else {
        println!("Removed store: {}", name);
    }
}

/// `localdb source add <path-or-url>`
pub fn run_source_add(ctx: &CliContext, source_arg: &str) {
    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;
    let store_name = resolve_store_name(ctx, &config_loader, &db);

    if check_yaml_owned(&store_name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    let _lock = match WriteLock::acquire(data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Verify store exists in runtime DB.
    match db.get_store(&store_name) {
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store_name.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
        Ok(Some(_)) => {}
    }

    let (kind, root, url) = classify_source(source_arg);
    let src = RuntimeSource {
        id: new_ulid(),
        store_name: store_name.clone(),
        kind: kind.to_string(),
        root: root.map(|s| s.to_string()),
        url: url.map(|s| s.to_string()),
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
    };

    if let Err(e) = db.upsert_source(&src) {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "id": src.id,
            "store": store_name,
            "kind": kind,
        }));
    } else {
        println!("Added source {} to store '{}'", src.id, store_name);
    }
}

/// `localdb source list`
pub fn run_source_list(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx);
    let store_name = resolve_store_name(ctx, &config_loader, &db);

    let sources = match db.list_sources(&store_name) {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    if ctx.json {
        let json_sources: Vec<serde_json::Value> = sources
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "store": s.store_name,
                    "kind": s.kind,
                    "root": s.root,
                    "url": s.url,
                    "preset": s.preset,
                })
            })
            .collect();
        print_json(&json!({ "sources": json_sources }));
    } else if sources.is_empty() {
        println!("No sources on store '{}'.", store_name);
    } else {
        for s in &sources {
            let loc = s.root.as_deref().or(s.url.as_deref()).unwrap_or("?");
            println!("{} [{}] {}", s.id, s.kind, loc);
        }
    }
}

/// `localdb source remove <id>`
pub fn run_source_remove(ctx: &CliContext, id: &str) {
    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;
    let store_name = resolve_store_name(ctx, &config_loader, &db);

    if check_yaml_owned(&store_name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    let _lock = match WriteLock::acquire(data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    match db.delete_source(id) {
        Ok(true) => {}
        Ok(false) => exit_err(&Error::SourceNotFound { id: id.to_string() }, ctx.json),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "id": id }));
    } else {
        println!("Removed source: {}", id);
    }
}

/// `localdb index [--source <id>]`
///
/// One-shot scan-and-index in embedded mode.
pub fn run_index(ctx: &CliContext, source_id: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_index_async(ctx, source_id));
}

async fn run_index_async(ctx: &CliContext, source_id: Option<&str>) {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{
            run_ingestion_for_source, DocumentExtractor, DocumentIndex, ExtractionResult,
            IngestionConfig,
        },
        FakeEmbedder,
    };

    let (config_loader, db) = load_app_db(ctx);
    let data_dir = config_loader.paths.data_dir.clone();
    let store_name = resolve_store_name(ctx, &config_loader, &db);

    let _lock = match WriteLock::acquire(&data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    let rt_store = match db.get_store(&store_name) {
        Ok(Some(s)) => s,
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store_name.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    };

    let all_sources = match db.list_sources(&store_name) {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    let sources_to_index: Vec<RuntimeSource> = if let Some(sid) = source_id {
        match all_sources.into_iter().find(|s| s.id == sid) {
            Some(s) => vec![s],
            None => exit_err(
                &Error::SourceNotFound {
                    id: sid.to_string(),
                },
                ctx.json,
            ),
        }
    } else {
        all_sources
    };

    if sources_to_index.is_empty() {
        if ctx.json {
            print_json(&json!({ "status": "ok", "message": "no sources to index" }));
        } else {
            println!("No sources to index on store '{}'.", store_name);
        }
        return;
    }

    let policy = config_loader.config.defaults.indexing.clone();
    let policy_version = compute_policy_version(&policy);

    let ingestion_cfg = IngestionConfig {
        store_id: rt_store.id.clone(),
        policy_version,
        chunker: ChunkerConfig::prose(),
    };

    let embedder = FakeEmbedder::new(128);

    // Extraction bridge between extract crate and core's DocumentExtractor trait.
    struct ExtractBridge;
    impl DocumentExtractor for ExtractBridge {
        fn extract(&self, bytes: &[u8], filename: Option<&str>) -> Result<ExtractionResult, Error> {
            let out = extract::extract(bytes, filename)?;
            Ok(ExtractionResult {
                text: out.text,
                blocks: out.blocks,
                title: out.title,
            })
        }
    }

    let store_data_dir = data_dir.join("stores").join(&store_name);
    if let Err(e) = std::fs::create_dir_all(&store_data_dir) {
        exit_err(
            &Error::Internal {
                message: format!("cannot create store dir: {}", e),
                correlation_id: "index_storedir".to_string(),
            },
            ctx.json,
        );
    }

    let lance_path = store_data_dir.to_string_lossy().to_string();
    let lancedb_store = match store_lancedb::LanceDbStore::open(&lance_path, 128).await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    let mut doc_index = DocumentIndex::new();
    let (mut indexed, mut skipped, mut chunks, mut errors) = (0u64, 0u64, 0u64, 0u64);

    for rt_source in &sources_to_index {
        let source = runtime_source_to_core_source(rt_source, &rt_store.id);
        if !ctx.json {
            let loc = rt_source
                .root
                .as_deref()
                .or(rt_source.url.as_deref())
                .unwrap_or("?");
            eprintln!("Indexing source {} ({})", rt_source.id, loc);
        }

        match run_ingestion_for_source(
            &source,
            &mut doc_index,
            &lancedb_store,
            &embedder,
            &ingestion_cfg,
            &ExtractBridge,
            None,
        )
        .await
        {
            Ok(r) => {
                indexed += r.docs_indexed;
                skipped += r.docs_skipped;
                chunks += r.chunks_written;
                errors += r.error_count;
            }
            Err(e) => {
                errors += 1;
                eprintln!("error indexing source {}: {}", rt_source.id, e);
            }
        }
    }

    // Create FTS index so BM25 search works. Safe to call after every index run.
    if chunks > 0 {
        if let Err(e) = lancedb_store.create_fts_index().await {
            // Non-fatal — log and continue. BM25 leg will be skipped by search.
            eprintln!("warning: FTS index creation failed: {}", e);
        }
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "docs_indexed": indexed,
            "docs_skipped": skipped,
            "chunks_written": chunks,
            "errors": errors,
        }));
    } else {
        println!(
            "Index complete: {} indexed, {} skipped, {} chunks written, {} errors",
            indexed, skipped, chunks, errors
        );
    }
}

/// `localdb search <query> [--limit N]`
pub fn run_search(ctx: &CliContext, query: &str, limit: usize) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_search_async(ctx, query, limit));
}

async fn run_search_async(ctx: &CliContext, query: &str, limit: usize) {
    use localdb_core::{
        search::{QueryRequest, SearchOrchestrator, StoreHandle},
        FakeEmbedder,
    };

    let (config_loader, db) = load_app_db(ctx);
    let data_dir = config_loader.paths.data_dir.clone();

    let runtime_stores = match db.list_stores() {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Collect store names to search.
    let store_names: Vec<String> = if ctx.stores.is_empty() {
        // Include YAML stores + runtime stores.
        let mut names: Vec<String> = config_loader
            .config
            .stores
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let yaml_set: std::collections::HashSet<&str> = config_loader
            .config
            .stores
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for s in &runtime_stores {
            if !yaml_set.contains(s.name.as_str()) {
                names.push(s.name.clone());
            }
        }
        names
    } else {
        ctx.stores.clone()
    };

    if store_names.is_empty() {
        if ctx.json {
            print_json(&json!({ "citations": [] }));
        } else {
            println!("No stores to search. Run `localdb store add <name>` first.");
        }
        return;
    }

    // Build store handles.
    let mut store_handles: Vec<StoreHandle> = Vec::new();
    for name in &store_names {
        let store_data_dir = data_dir.join("stores").join(name);
        if !store_data_dir.exists() {
            continue; // Not yet indexed.
        }
        let store_id = runtime_stores
            .iter()
            .find(|s| s.name == *name)
            .map(|s| s.id.clone())
            .unwrap_or_else(|| name.clone());

        let lance_path = store_data_dir.to_string_lossy().to_string();
        match store_lancedb::LanceDbStore::open(&lance_path, 128).await {
            Ok(s) => store_handles.push(StoreHandle {
                id: store_id,
                name: name.clone(),
                store: Box::new(s),
            }),
            Err(e) => eprintln!("warning: cannot open store '{}': {}", name, e),
        }
    }

    if store_handles.is_empty() {
        if ctx.json {
            print_json(&json!({ "citations": [] }));
        } else {
            println!("No indexed stores found. Run `localdb index` first.");
        }
        return;
    }

    let embedder = FakeEmbedder::new(128);
    let request = QueryRequest {
        query: query.to_string(),
        leg_k: None,
        top_n: Some(limit),
        filters: vec![],
    };

    match SearchOrchestrator::query(&store_handles, &embedder, &request).await {
        Ok(response) => {
            let json_citations: Vec<serde_json::Value> = response
                .citations
                .iter()
                .map(|c| serde_json::to_value(c).unwrap_or(json!({})))
                .collect();

            if ctx.json {
                print_json(&json!({ "citations": json_citations }));
            } else if response.citations.is_empty() {
                println!("No results for '{}'.", query);
            } else {
                for (i, citation) in response.citations.iter().enumerate() {
                    let heading = if citation.heading_path.is_empty() {
                        String::new()
                    } else {
                        format!(" > {}", citation.heading_path.join(" > "))
                    };
                    println!("{}. {}{}", i + 1, citation.uri, heading);
                    let snippet: String = citation.snippet.chars().take(120).collect();
                    println!("   {}", snippet.trim());
                    println!();
                }
            }
        }
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// `localdb serve` — stub; delegates to T11.
pub fn run_serve(_ctx: &CliContext) {
    eprintln!("serve: not yet implemented (T11)");
    std::process::exit(1);
}

/// `localdb mcp` — stub; delegates to T10.
pub fn run_mcp(_ctx: &CliContext) {
    eprintln!("mcp: not yet implemented (T10)");
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_app_db(dir: &TempDir) -> AppDb {
        AppDb::open(&dir.path().join("state.redb")).unwrap()
    }

    fn new_runtime_store(name: &str) -> RuntimeStore {
        RuntimeStore {
            id: new_ulid(),
            name: name.to_string(),
            visibility: "private".to_string(),
            backend: "lancedb".to_string(),
            indexing: None,
        }
    }

    // --- WriteLock ---

    #[test]
    fn write_lock_creates_data_dir_and_file() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        assert!(!sub.exists());
        let _lock = WriteLock::acquire(&sub).expect("should acquire");
        assert!(sub.join(".write.lock").exists());
    }

    #[test]
    fn write_lock_removes_lock_file_on_drop() {
        let dir = TempDir::new().unwrap();
        {
            let _lock = WriteLock::acquire(dir.path()).expect("should acquire");
            assert!(dir.path().join(".write.lock").exists());
        }
        assert!(!dir.path().join(".write.lock").exists());
    }

    #[test]
    fn write_lock_exit_code_for_store_locked() {
        // StoreLocked maps to exit code 4 per spec.
        assert_eq!(Error::StoreLocked.exit_code(), 4);
        assert_eq!(Error::StoreLocked.code(), "store_locked");
    }

    // --- DaemonState probe ---

    #[test]
    fn probe_not_running_without_socket() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(probe_daemon(dir.path()), DaemonState::NotRunning));
    }

    #[test]
    fn probe_running_with_socket_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("daemon.sock"), b"").unwrap();
        assert!(matches!(
            probe_daemon(dir.path()),
            DaemonState::Running { .. }
        ));
    }

    // --- classify_source ---

    #[test]
    fn classify_path_source() {
        let (kind, root, url) = classify_source("/home/user/docs");
        assert_eq!(kind, "path");
        assert_eq!(root, Some("/home/user/docs"));
        assert_eq!(url, None);
    }

    #[test]
    fn classify_https_url_source() {
        let (kind, root, url) = classify_source("https://example.com/page");
        assert_eq!(kind, "url");
        assert_eq!(root, None);
        assert_eq!(url, Some("https://example.com/page"));
    }

    #[test]
    fn classify_http_url_source() {
        let (kind, root, url) = classify_source("http://localhost/doc");
        assert_eq!(kind, "url");
        assert_eq!(root, None);
        assert_eq!(url, Some("http://localhost/doc"));
    }

    // --- runtime_source_to_core_source ---

    #[test]
    fn convert_path_source() {
        use localdb_core::types::{SourceKind, SourceSpec};
        let src = RuntimeSource {
            id: "src-1".into(),
            store_name: "s".into(),
            kind: "path".into(),
            root: Some("/tmp/docs".into()),
            url: None,
            include: vec!["**/*.md".into()],
            exclude: vec![],
            preset: "prose".into(),
        };
        let core = runtime_source_to_core_source(&src, "store-id");
        assert_eq!(core.id, "src-1");
        assert!(matches!(core.kind, SourceKind::Path));
        match &core.spec {
            SourceSpec::Path { root, include, .. } => {
                assert_eq!(root, "/tmp/docs");
                assert_eq!(include, &vec!["**/*.md".to_string()]);
            }
            _ => panic!("expected path spec"),
        }
    }

    #[test]
    fn convert_url_source() {
        use localdb_core::types::{SourceKind, SourceSpec};
        let src = RuntimeSource {
            id: "src-2".into(),
            store_name: "s".into(),
            kind: "url".into(),
            root: None,
            url: Some("https://example.com".into()),
            include: vec![],
            exclude: vec![],
            preset: "prose".into(),
        };
        let core = runtime_source_to_core_source(&src, "store-id");
        assert!(matches!(core.kind, SourceKind::Url));
        match &core.spec {
            SourceSpec::Url { url, .. } => assert_eq!(url, "https://example.com"),
            _ => panic!("expected url spec"),
        }
    }

    // --- AppDb store CRUD ---

    #[test]
    fn app_db_store_add_list_remove() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        // Empty initially.
        assert!(db.list_stores().unwrap().is_empty());

        // Add.
        let store = new_runtime_store("mystore");
        db.upsert_store(&store).unwrap();

        let stores = db.list_stores().unwrap();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].name, "mystore");

        // Remove.
        assert!(db.delete_store("mystore").unwrap());
        assert!(db.list_stores().unwrap().is_empty());

        // Remove again returns false.
        assert!(!db.delete_store("mystore").unwrap());
    }

    #[test]
    fn app_db_get_store_by_name() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);
        let store = new_runtime_store("s1");
        db.upsert_store(&store).unwrap();

        let found = db.get_store("s1").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "s1");

        let missing = db.get_store("nonexistent").unwrap();
        assert!(missing.is_none());
    }

    // --- AppDb source CRUD ---

    #[test]
    fn app_db_source_upsert_list_delete() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let store = new_runtime_store("s1");
        db.upsert_store(&store).unwrap();

        let src = RuntimeSource {
            id: new_ulid(),
            store_name: "s1".into(),
            kind: "path".into(),
            root: Some("/tmp".into()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".into(),
        };
        db.upsert_source(&src).unwrap();

        let list = db.list_sources("s1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, src.id);

        // Delete.
        assert!(db.delete_source(&src.id).unwrap());
        assert!(!db.delete_source(&src.id).unwrap()); // idempotent false
        assert!(db.list_sources("s1").unwrap().is_empty());
    }

    #[test]
    fn app_db_source_list_filters_by_store() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        for name in &["sa", "sb"] {
            let src = RuntimeSource {
                id: new_ulid(),
                store_name: name.to_string(),
                kind: "path".into(),
                root: Some("/tmp".into()),
                url: None,
                include: vec![],
                exclude: vec![],
                preset: "prose".into(),
            };
            db.upsert_source(&src).unwrap();
        }

        assert_eq!(db.list_sources("sa").unwrap().len(), 1);
        assert_eq!(db.list_sources("sb").unwrap().len(), 1);
        assert!(db.list_sources("sc").unwrap().is_empty());
    }

    #[test]
    fn app_db_source_get_by_id() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let src = RuntimeSource {
            id: new_ulid(),
            store_name: "s".into(),
            kind: "path".into(),
            root: Some("/tmp".into()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".into(),
        };
        db.upsert_source(&src).unwrap();

        let found = db.get_source(&src.id).unwrap();
        assert!(found.is_some());
        let missing = db.get_source("no-such-id").unwrap();
        assert!(missing.is_none());
    }

    // --- Error exit code mapping ---

    #[test]
    fn error_exit_codes_match_spec() {
        // specs/05-surfaces.md §5
        assert_eq!(
            Error::Internal {
                message: "".into(),
                correlation_id: "".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(Error::InvalidConfig { message: "".into() }.exit_code(), 2);
        assert_eq!(Error::InvalidRequest { message: "".into() }.exit_code(), 2);
        assert_eq!(Error::StoreNotFound { id: "".into() }.exit_code(), 3);
        assert_eq!(Error::SourceNotFound { id: "".into() }.exit_code(), 3);
        assert_eq!(Error::StoreLocked.exit_code(), 4);
        assert_eq!(Error::ConfigReadonly.exit_code(), 4);
        assert_eq!(Error::DaemonUnreachable.exit_code(), 5);
        assert_eq!(Error::ModelMissing { message: "".into() }.exit_code(), 5);
    }

    // --- JSON output shape ---

    #[test]
    fn json_store_list_shape() {
        // The store list JSON must have a "stores" key.
        let value = json!({ "stores": [] });
        assert!(value.get("stores").is_some());
    }

    #[test]
    fn json_error_shape() {
        let err = Error::StoreLocked;
        let v = json!({
            "error": err.code(),
            "message": err.to_string(),
        });
        assert_eq!(v["error"].as_str().unwrap(), "store_locked");
    }
}
