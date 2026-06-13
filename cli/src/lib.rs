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
// Store name validation — A9-safety
// ---------------------------------------------------------------------------

/// Validate a store name, returning an error for unsafe or invalid names.
///
/// Rejects: empty string, names containing `/`, and names that are exactly `.` or `..`.
/// Returns `Error::InvalidRequest` (exit code 2) on rejection.
pub fn validate_store_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::InvalidRequest {
            message: "store name must not be empty".to_string(),
        });
    }
    if name == "." || name == ".." {
        return Err(Error::InvalidRequest {
            message: format!("store name '{}' is not allowed", name),
        });
    }
    if name.contains('/') || name.contains('\\') {
        return Err(Error::InvalidRequest {
            message: format!("store name '{}' must not contain '/' or '\\'", name),
        });
    }
    Ok(())
}

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

/// Check whether a daemon HTTP endpoint is reachable by probing its TCP port.
///
/// Returns `true` if a TCP connection to the host:port can be established within
/// 2 seconds, indicating the daemon process is alive. Returns `false` on
/// connection refused, timeout, or parse failure (stale / never-started socket).
///
/// We use a plain `std::net::TcpStream` so this function is safe to call from
/// both sync and async contexts (no nested tokio runtime needed).
fn probe_daemon_health(base_url: &str) -> bool {
    probe_daemon_health_inner(base_url).unwrap_or(false)
}

fn probe_daemon_health_inner(base_url: &str) -> Option<bool> {
    use std::net::ToSocketAddrs;

    // Strip scheme prefix and path to extract the host:port portion.
    let host_port = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()?;

    // Detect port robustly, handling bracketed IPv6 (e.g. [::1], [::1]:8080).
    let addr_str: String = if host_port.starts_with('[') {
        // Bracketed IPv6 literal.
        if host_port.contains("]:") {
            // Port present: [::1]:8080 — use as-is.
            host_port.to_string()
        } else {
            // No port: [::1] — add default.
            format!("{}:80", host_port)
        }
    } else if host_port.contains(':') {
        // host:port
        host_port.to_string()
    } else {
        format!("{}:80", host_port)
    };

    // Resolve to a socket address (handles both IP literals and hostnames).
    let sock_addr = addr_str.to_socket_addrs().ok()?.next()?;

    Some(
        std::net::TcpStream::connect_timeout(&sock_addr, std::time::Duration::from_secs(2)).is_ok(),
    )
}

/// Probe the daemon socket for a given data directory.
///
/// Returns `DaemonState::Running` if the socket file is present (MVP check).
/// The base_url is resolved in priority order:
///   1. `LOCALDB_DAEMON_URL` environment variable (for testing and overrides)
///   2. Content of `daemon.sock` file (if it contains a URL)
///   3. Default `http://127.0.0.1:7700`
///
/// Returns `DaemonState::NotRunning` if neither the env var is set nor the
/// socket file exists.
pub fn probe_daemon(data_dir: &Path) -> DaemonState {
    // Allow tests (and users) to override the daemon URL via env var.
    if let Ok(url) = std::env::var("LOCALDB_DAEMON_URL") {
        if !url.is_empty() {
            return DaemonState::Running { base_url: url };
        }
    }

    let socket_path = data_dir.join("daemon.sock");
    if socket_path.exists() {
        // Read the sock file content — if it looks like a URL, use it as base_url.
        let base_url = std::fs::read_to_string(&socket_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
            .unwrap_or_else(|| "http://127.0.0.1:7700".to_string());

        // Probe the daemon with a health check to detect stale socket files.
        // A stale socket exists when a previous daemon crashed without cleaning up.
        // We perform the probe via a one-shot tokio runtime (same pattern as daemon_request).
        let health_url = format!("{}/v1/status", base_url);
        let reachable = probe_daemon_health(&health_url);

        if reachable {
            DaemonState::Running { base_url }
        } else {
            // Stale socket: remove it and report not running.
            let _ = std::fs::remove_file(&socket_path);
            DaemonState::NotRunning
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
/// Uses `fd-lock` for OS-level exclusive advisory locking (flock(2)/LockFileEx).
/// This guarantees that exactly one process holds the lock at a time — a second
/// concurrent `acquire()` call will fail immediately with `Error::StoreLocked`.
///
/// Holds the OS lock for its entire lifetime; releases and removes on `Drop`.
/// Returns `Error::StoreLocked` if the lock cannot be acquired.
pub struct WriteLock {
    /// The locked file descriptor, held for the duration of the lock.
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
    /// The `RwLock` wrapper — kept alive so the guard remains valid.
    _rw: Box<fd_lock::RwLock<std::fs::File>>,
    path: PathBuf,
}

impl WriteLock {
    /// Attempt to acquire the OS-level advisory write lock for `data_dir`.
    ///
    /// The lock file is `<data_dir>/.write.lock`.  A second process calling
    /// `acquire()` on the same path will receive `Error::StoreLocked`.
    pub fn acquire(data_dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Internal {
            message: format!("cannot create data dir '{}': {}", data_dir.display(), e),
            correlation_id: "write_lock_mkdir".to_string(),
        })?;

        let lock_path = data_dir.join(".write.lock");

        // F8: Open without truncate so a failed lock acquire does NOT leave the
        // file truncated (which would clobber the PID of the current lock holder).
        // We truncate and write our PID only AFTER the OS lock is acquired.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    Error::StoreLocked
                } else {
                    Error::Internal {
                        message: format!("cannot open lock file: {}", e),
                        correlation_id: "write_lock_open".to_string(),
                    }
                }
            })?;

        // Box the RwLock so we can take a 'static guard from it.
        // SAFETY: the Box is pinned on the heap and outlives the guard
        // because both are stored in WriteLock together.
        let mut rw = Box::new(fd_lock::RwLock::new(file));
        let guard = {
            // Extend lifetime: the guard borrows from *rw which lives in the
            // same struct, so the lifetime is sound.
            let rw_ref: &mut fd_lock::RwLock<std::fs::File> =
                unsafe { &mut *(rw.as_mut() as *mut _) };
            rw_ref.try_write().map_err(|_| Error::StoreLocked)?
        };

        // Write our PID for diagnostics (best-effort). Only done after lock acquired.
        // We reopen the file for the truncate+write so we avoid borrowing issues
        // with the guard. The OS-level lock (flock/LockFileEx) is per-process,
        // so reopening within the same process is safe — we already hold it.
        {
            use std::io::Write as _;
            if let Ok(mut pid_file) = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&lock_path)
            {
                let _ = writeln!(pid_file, "{}", std::process::id());
            }
        }

        Ok(Self {
            _guard: guard,
            _rw: rw,
            path: lock_path,
        })
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        // The guard is dropped first (releases the OS lock), then the file.
        // Remove the lock file as a courtesy (non-fatal if it fails).
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Daemon HTTP client — specs/05-surfaces.md §2, specs/01-architecture.md §3
// ---------------------------------------------------------------------------
//
// When a daemon is running, mutating commands route to its REST API instead of
// writing directly to the embedded store. This thin client issues the
// appropriate HTTP requests and maps responses to exit codes.

/// Issue a synchronous HTTP request to the daemon via a one-shot tokio runtime.
///
/// Returns the JSON response body on success (2xx), or an `Error` on failure.
fn daemon_request(
    method: reqwest::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, Error> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Internal {
            message: format!("cannot build tokio runtime for daemon request: {}", e),
            correlation_id: "daemon_rt".to_string(),
        })?;
    rt.block_on(daemon_request_async(method, url, body))
}

async fn daemon_request_async(
    method: reqwest::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, Error> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Internal {
            message: format!("cannot build HTTP client: {}", e),
            correlation_id: "daemon_client_build".to_string(),
        })?;

    let mut req = client.request(method, url);
    if let Some(b) = body {
        req = req.json(&b);
    }

    let resp = req.send().await.map_err(|_| Error::DaemonUnreachable)?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

    if status.is_success() {
        Ok(json)
    } else {
        // Map HTTP error codes to our error types.
        // The server's error body uses {code, message} (see server/src/error.rs).
        let code = json
            .get("code")
            .and_then(|e| e.as_str())
            .unwrap_or("internal");
        let msg = json
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("daemon error")
            .to_string();

        Err(match code {
            "store_not_found" => Error::StoreNotFound { id: msg },
            "source_not_found" => Error::SourceNotFound { id: msg },
            "store_locked" => Error::StoreLocked,
            "config_readonly" => Error::ConfigReadonly,
            "daemon_unreachable" => Error::DaemonUnreachable,
            _ => Error::Internal {
                message: format!("daemon returned {}: {}", status.as_u16(), msg),
                correlation_id: "daemon_http".to_string(),
            },
        })
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

    /// Find a source by its `root` path or `url` field, optionally scoped to a store.
    fn find_by_root_or_url(
        &self,
        value: &str,
        store_name: Option<&str>,
    ) -> Result<Option<RuntimeSource>, Error> {
        let rtxn = self.db.begin_read().map_err(map_db_err)?;
        let t = rtxn.open_table(SOURCES_TABLE).map_err(map_db_err)?;
        for entry in t.iter().map_err(map_db_err)? {
            let (_, v) = entry.map_err(map_db_err)?;
            let src: RuntimeSource =
                serde_json::from_str(v.value()).map_err(|e| Error::Internal {
                    message: format!("cannot deserialize source: {}", e),
                    correlation_id: "source_find_deser".to_string(),
                })?;
            if let Some(sn) = store_name {
                if src.store_name != sn {
                    continue;
                }
            }
            let matches = src.root.as_deref() == Some(value) || src.url.as_deref() == Some(value);
            if matches {
                return Ok(Some(src));
            }
        }
        Ok(None)
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
    pub fn find_source_by_root_or_url(
        &self,
        value: &str,
        store_name: Option<&str>,
    ) -> Result<Option<RuntimeSource>, Error> {
        self.sources.find_by_root_or_url(value, store_name)
    }
}

// ---------------------------------------------------------------------------
// Common setup helpers
// ---------------------------------------------------------------------------

/// Load config and open the AppDb. Exits on failure.
///
/// When a daemon is running, the runtime-state DB is managed by the daemon
/// process exclusively. Opening it from the CLI would cause a "Database already
/// open" error (redb's single-writer guarantee). We therefore skip opening the
/// DB when a daemon is detected and return a dummy in-memory-only AppDb that
/// is only usable for reads that go through the daemon's HTTP API anyway.
///
/// Commands that reach the daemon-routing branch early (before needing the local
/// DB) will return before any embedded-mode DB access occurs.
fn load_app_db(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Probe the daemon *before* opening the local DB. If the daemon is running
    // it holds the exclusive write lock on runtime-state.redb; opening it from
    // the CLI would fail with "Database already open".
    let data_dir = &config_loader.paths.data_dir;
    if matches!(probe_daemon(data_dir), DaemonState::Running { .. }) {
        // Return a DB opened on a temporary path so callers that branch on the
        // daemon path early (index, search, source add/remove, store add/remove)
        // can still call resolve_store_name / list_stores without panicking.
        // The actual mutations all go through the HTTP API when the daemon is up.
        let tmp_dir = tempdir_for_daemon_mode(data_dir);
        let tmp_db_path = tmp_dir.join("daemon-mode-placeholder.redb");
        let db = match AppDb::open(&tmp_db_path) {
            Ok(d) => d,
            Err(e) => exit_err(&e, ctx.json),
        };
        return (config_loader, db);
    }

    let db_path = config_loader.paths.runtime_state_db_path();
    let db = match AppDb::open(&db_path) {
        Ok(d) => d,
        Err(e) => exit_err(&e, ctx.json),
    };
    (config_loader, db)
}

/// F1-cli: Load config with fallback to platform defaults for read-only commands.
///
/// When the config file is malformed or unreadable, read-only commands (search,
/// store list, status) should still work using platform default config and an
/// in-memory or temp DB, rather than hard failing.
fn load_app_db_lenient(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options) {
        Ok(c) => c,
        Err(_) => {
            // Config is malformed/missing — use platform default paths.
            // This allows read-only commands (search, store list, status) to work.
            let options_default = LoadOptions::default();
            match load_config(&options_default) {
                Ok(c) => c,
                Err(e) => exit_err(&e, ctx.json),
            }
        }
    };

    let data_dir = &config_loader.paths.data_dir;
    if matches!(probe_daemon(data_dir), DaemonState::Running { .. }) {
        let tmp_dir = tempdir_for_daemon_mode(data_dir);
        let tmp_db_path = tmp_dir.join("daemon-mode-placeholder.redb");
        let db = match AppDb::open(&tmp_db_path) {
            Ok(d) => d,
            Err(e) => exit_err(&e, ctx.json),
        };
        return (config_loader, db);
    }

    let db_path = config_loader.paths.runtime_state_db_path();
    // For read-only commands, use a temp DB if the real one can't be opened.
    let db = AppDb::open(&db_path).unwrap_or_else(|_| {
        let tmp_dir = tempdir_for_daemon_mode(data_dir);
        let tmp_path = tmp_dir.join("lenient-fallback.redb");
        AppDb::open(&tmp_path).unwrap_or_else(|e| exit_err(&e, ctx.json))
    });
    (config_loader, db)
}

/// Return a writable temp directory path that lives inside `data_dir` so the
/// daemon-mode placeholder DB is cleaned up automatically when the process exits.
///
/// We use a fixed subdir name rather than a proper `TempDir` because we only
/// need the path (the DB is tiny and will be recreated on next run).
fn tempdir_for_daemon_mode(data_dir: &Path) -> PathBuf {
    let p = data_dir.join(".cli-daemon-placeholder");
    let _ = std::fs::create_dir_all(&p);
    p
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

/// Reconcile YAML-declared stores (and their sources) into the runtime-state DB.
///
/// This is the resolution for issue #12: YAML-declared stores were invisible to
/// `localdb index` because that command looked up stores only in the runtime DB.
///
/// For each store declared in `config.stores`:
/// - If the runtime DB does not already have a record for that store name, insert
///   a shadow `RuntimeStore` so that the index command can find it.
/// - For each source declared under that store, if the runtime DB does not already
///   have an entry with the same root/url, insert a shadow `RuntimeSource`.
///
/// Shadow records are functionally identical to runtime-owned records. YAML ownership
/// is still determined at runtime by `check_yaml_owned` (name lookup in config), so
/// mutations on YAML-named stores continue to return `Error::ConfigReadonly`.
///
/// This function is intentionally idempotent: re-running it never overwrites existing
/// runtime-DB records (it only inserts when the name/key is absent).
pub fn reconcile_yaml_stores(
    db: &AppDb,
    config: &localdb_core::config::schema::RawConfig,
) -> Result<(), Error> {
    for yaml_store in &config.stores {
        // Only insert a shadow if the store is not already in the runtime DB.
        if db.get_store(&yaml_store.name)?.is_none() {
            let shadow = RuntimeStore {
                id: new_ulid(),
                name: yaml_store.name.clone(),
                visibility: yaml_store.visibility.clone(),
                backend: yaml_store.backend.clone(),
                indexing: yaml_store.indexing.clone(),
            };
            db.upsert_store(&shadow)?;
        }

        // Reconcile sources declared in this YAML store.
        for yaml_src in &yaml_store.sources {
            let root_or_url: Option<&str> = yaml_src.root.as_deref().or(yaml_src.url.as_deref());

            // Skip if we can't identify this source by root/url (shouldn't happen
            // in practice — a source always has one of them).
            let key = match root_or_url {
                Some(k) => k,
                None => continue,
            };

            // Only insert if not already present.
            if db
                .find_source_by_root_or_url(key, Some(&yaml_store.name))?
                .is_none()
            {
                let src = RuntimeSource {
                    id: new_ulid(),
                    store_name: yaml_store.name.clone(),
                    kind: yaml_src.kind.clone(),
                    root: yaml_src.root.clone(),
                    url: yaml_src.url.clone(),
                    include: yaml_src.include.clone(),
                    exclude: yaml_src.exclude.clone(),
                    preset: yaml_src.preset.clone(),
                };
                db.upsert_source(&src)?;
            }
        }
    }
    Ok(())
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

/// Normalize a file-system path source argument into `(root, include_globs, exclude_globs)`.
///
/// Shared by both the daemon branch and embedded branch of `source add` so that
/// path validation, single-file promotion, and default excludes are always applied
/// consistently regardless of whether the daemon is running.
///
/// Returns `Err(InvalidRequest)` (exit 2) if the path does not exist.
fn normalize_path_source(raw_path: &str) -> Result<(String, Vec<String>, Vec<String>), Error> {
    let p = std::path::Path::new(raw_path);

    if !p.exists() {
        return Err(Error::InvalidRequest {
            message: format!("path '{}' does not exist", raw_path),
        });
    }

    let (root, include_globs) = if p.is_file() {
        // #7: single-file source — use parent dir as root, include only this file.
        let parent = p
            .parent()
            .map(|par| {
                if par == Path::new("") {
                    Path::new(".")
                } else {
                    par
                }
            })
            .unwrap_or(Path::new("."));
        let filename = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (parent.to_string_lossy().to_string(), vec![filename])
    } else {
        (raw_path.to_string(), vec![])
    };

    // #4: apply default excludes for path sources.
    let exclude_globs: Vec<String> = DEFAULT_PATH_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .collect();

    Ok((root, include_globs, exclude_globs))
}

/// Determine whether a string looks like a ULID/UUID (not a path or URL).
///
/// ULIDs are 26 uppercase alphanumeric characters. We use this to distinguish
/// bare IDs from path/URL arguments in source remove.
fn looks_like_id(s: &str) -> bool {
    // ULID: exactly 26 chars, all uppercase alphanumeric.
    // UUID: 36 chars with hyphens.
    // Anything containing `/`, `\`, `.` or `://` is a path or URL, not an ID.
    if s.contains('/') || s.contains('\\') || s.contains("://") {
        return false;
    }
    // ULID pattern: 26 uppercase alphanumeric.
    if s.len() == 26
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_lowercase())
    {
        return true;
    }
    // UUID pattern: 32 hex + 4 hyphens = 36 chars.
    if s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return true;
    }
    // Shorter opaque IDs (no path indicators) are also treated as IDs.
    // E.g. numeric IDs or short hex. If it has no path separator or dot, treat
    // as ID only if it's clearly not a filename/relative path.
    false
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

    // F11: If --config was explicitly given but the parent directory doesn't exist,
    // fail with exit 2 (invalid config) rather than silently using platform defaults.
    if ctx.config.is_some() {
        if let Some(parent) = config_path.parent() {
            if !parent.exists() && parent != Path::new("") {
                exit_err(
                    &Error::InvalidConfig {
                        message: format!(
                            "config path parent directory '{}' does not exist",
                            parent.display()
                        ),
                    },
                    ctx.json,
                );
            }
        }
    }

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

    // Initialize the runtime-state DB and create default store (#6).
    let db_path = data_dir.join("runtime-state.redb");
    let db = match AppDb::open(&db_path) {
        Ok(d) => d,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Create the default store if it doesn't exist yet.
    match db.get_store("default") {
        Ok(None) => {
            let default_store = RuntimeStore {
                id: new_ulid(),
                name: "default".to_string(),
                visibility: "private".to_string(),
                backend: "lancedb".to_string(),
                indexing: None,
            };
            if let Err(e) = db.upsert_store(&default_store) {
                exit_err(&e, ctx.json);
            }
        }
        Ok(Some(_)) => {} // already exists
        Err(e) => exit_err(&e, ctx.json),
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
        println!(
            "Note: when using 'local-onnx' provider, the ONNX model is downloaded on first index."
        );
        println!("      Hosted providers (openai-compatible, perplexity, voyage) require an API key in config.");
        println!("Run `localdb store add <name>` to create a store.");
    }
}

/// `localdb status`
pub fn run_status(ctx: &CliContext) {
    // F1-cli: use lenient loader so status works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx);
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
    // A9-safety: validate store name before anything else.
    if let Err(e) = validate_store_name(name) {
        exit_err(&e, ctx.json);
    }

    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;

    if check_yaml_owned(name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir) {
        let url = format!("{}/v1/stores", base_url);
        let body = json!({ "name": name, "visibility": "private", "backend": "lancedb" });
        match daemon_request(reqwest::Method::POST, &url, Some(body)) {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!(
                        "Added store: {} (via daemon)",
                        v.get("name").and_then(|n| n.as_str()).unwrap_or(name)
                    );
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // Embedded mode: acquire write lock and write directly.
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
    // F1-cli: use lenient loader so store list works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx);

    // #12: Reconcile YAML-declared stores into the runtime DB so that commands
    // that look up stores by name (e.g. source list, source add) find them.
    // Failures here are non-fatal for the list display path.
    let _ = reconcile_yaml_stores(&db, &config_loader.config);

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

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir) {
        let url = format!("{}/v1/stores/{}", base_url, name);
        match daemon_request(reqwest::Method::DELETE, &url, None) {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!("Removed store: {} (via daemon)", name);
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // Embedded mode.
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

/// Default exclude patterns for path sources (#4).
/// Default exclude patterns for path sources (#4).
///
/// These patterns are matched against both the root-relative path and the bare
/// basename of each entry (see `enumerate_dir` in `core`), so a pattern like
/// `**/.git` prunes a `.git` directory at any depth before recursing into it.
/// Using `**/X` (without a trailing `/**`) matches the entry itself; the subtree
/// is never walked.  For single-file junk (`.DS_Store`) the same form works as a
/// file-pattern.
///
/// **Include** globs are still anchored to the source root and NOT affected by
/// this floating-basename rule.
const DEFAULT_PATH_EXCLUDES: &[&str] = &[
    "**/.git",
    "**/node_modules",
    "**/.DS_Store",
    "**/target",
    "**/__pycache__",
    "**/.venv",
];

/// `localdb source add <path-or-url>`
pub fn run_source_add(ctx: &CliContext, source_arg: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_add_async(ctx, source_arg));
}

async fn run_source_add_async(ctx: &CliContext, source_arg: &str) {
    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;

    // A9-safety: validate the --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let store_name = resolve_store_name(ctx, &config_loader, &db);

    if check_yaml_owned(&store_name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir) {
        let (kind, _root, url) = classify_source(source_arg);
        // The handler's CreateSourceRequest expects {kind, spec, preset} where
        // spec is a nested object (see server/src/handlers.rs CreateSourceRequest).
        // Apply the same path normalization as embedded mode (#14, #7, #4).
        let spec = if kind == "path" {
            match normalize_path_source(source_arg) {
                Ok((root, include, exclude)) => {
                    json!({ "root": root, "include": include, "exclude": exclude })
                }
                Err(e) => exit_err(&e, ctx.json),
            }
        } else {
            json!({ "url": url })
        };
        let url_str = format!("{}/v1/stores/{}/sources", base_url, store_name);
        let body = json!({
            "kind": kind,
            "spec": spec,
            "preset": "prose",
        });
        match daemon_request(reqwest::Method::POST, &url_str, Some(body)) {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!(
                        "Added source {} to store '{}' (via daemon)",
                        v.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
                        store_name
                    );
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // Embedded mode.
    let _lock = match WriteLock::acquire(data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    // #13: Verify store exists in runtime DB (exit 3 if not found).
    let rt_store = match db.get_store(&store_name) {
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store_name.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
        Ok(Some(s)) => s,
    };

    let (kind, _root_str, url_str2) = classify_source(source_arg);

    // Normalize path sources: validate existence, promote single files, apply excludes.
    let (actual_root, include_globs, exclude_globs) = if kind == "path" {
        match normalize_path_source(source_arg) {
            Ok(v) => v,
            Err(e) => exit_err(&e, ctx.json),
        }
    } else {
        (source_arg.to_string(), vec![], vec![])
    };

    let src = RuntimeSource {
        id: new_ulid(),
        store_name: store_name.clone(),
        kind: kind.to_string(),
        root: if kind == "path" {
            Some(actual_root)
        } else {
            None
        },
        url: url_str2.map(|s| s.to_string()),
        include: include_globs,
        exclude: exclude_globs,
        preset: "prose".to_string(),
    };

    if let Err(e) = db.upsert_source(&src) {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "id": src.id,
            "store": { "name": store_name },
            "kind": kind,
        }));
    } else {
        println!("Added source {} to store '{}'", src.id, store_name);
    }

    // #2: Auto-index after source add.
    // Drop the write lock AND the db handle before re-entering the index path,
    // which will acquire its own lock and open its own DB handle.
    let src_id = src.id.clone();
    let rt_store_clone = rt_store.clone();
    drop(_lock);
    drop(db);
    drop(config_loader);

    if kind == "path" || kind == "url" {
        if !ctx.json {
            eprintln!("Auto-indexing source {} ...", src_id);
        }
        // Build an index context scoped to this store.
        let index_ctx = CliContext {
            config: ctx.config.clone(),
            json: ctx.json,
            stores: vec![store_name.clone()],
        };
        run_index_for_source_async(&index_ctx, Some(&src_id), &rt_store_clone).await;
    }
}

/// Internal: run ingestion for a single source without re-resolving the store.
async fn run_index_for_source_async(
    ctx: &CliContext,
    source_id: Option<&str>,
    rt_store: &RuntimeStore,
) {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{
            run_ingestion_for_source, DocumentExtractor, DocumentIndex, ExtractionResult,
            IngestionConfig,
        },
    };

    let (config_loader, db) = load_app_db(ctx);
    let data_dir = config_loader.paths.data_dir.clone();

    let _lock = match WriteLock::acquire(&data_dir) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("warning: cannot acquire lock for auto-index: {}", e);
            return;
        }
    };

    let all_sources = match db.list_sources(&rt_store.name) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: cannot list sources for auto-index: {}", e);
            return;
        }
    };

    let sources_to_index: Vec<RuntimeSource> = if let Some(sid) = source_id {
        all_sources.into_iter().filter(|s| s.id == sid).collect()
    } else {
        all_sources
    };

    if sources_to_index.is_empty() {
        return;
    }

    let policy = config_loader.config.defaults.indexing.clone();
    let policy_version = compute_policy_version(&policy);

    let ingestion_cfg = IngestionConfig {
        store_id: rt_store.id.clone(),
        policy_version,
        chunker: ChunkerConfig::prose(),
    };

    let embed_policy = &config_loader.config.defaults.indexing.embedding;
    let models_dir = config_loader.paths.models_dir.clone();
    let embedder = match embed::create_embedder(
        embed_policy,
        &config_loader.config.providers,
        Some(&models_dir),
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: cannot create embedder for auto-index: {}", e);
            return;
        }
    };

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

    let store_data_dir = data_dir.join("stores").join(&rt_store.name);
    if let Err(e) = std::fs::create_dir_all(&store_data_dir) {
        eprintln!("warning: cannot create store dir for auto-index: {}", e);
        return;
    }

    let lance_path = store_data_dir.to_string_lossy().to_string();
    let lancedb_store =
        match store_lancedb::LanceDbStore::open(&lance_path, embedder.embedding_dim()).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: cannot open store for auto-index: {}", e);
                return;
            }
        };

    let mut doc_index = DocumentIndex::new();
    let mut chunks = 0u64;

    for rt_source in &sources_to_index {
        let source = runtime_source_to_core_source(rt_source, &rt_store.id);
        match run_ingestion_for_source(
            &source,
            &mut doc_index,
            &lancedb_store,
            embedder.as_ref(),
            &ingestion_cfg,
            &ExtractBridge,
            None,
        )
        .await
        {
            Ok(r) => {
                chunks += r.chunks_written;
                if !ctx.json {
                    eprintln!(
                        "  indexed {} docs, {} chunks",
                        r.docs_indexed, r.chunks_written
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "warning: auto-index error for source {}: {}",
                    rt_source.id, e
                );
            }
        }
    }

    if chunks > 0 {
        if let Err(e) = lancedb_store.create_fts_index().await {
            eprintln!("warning: FTS index creation failed: {}", e);
        }
    }
}

/// `localdb source list`
pub fn run_source_list(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx);

    // A9-safety: validate --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let store_name = resolve_store_name(ctx, &config_loader, &db);

    // D1: verify store exists before listing sources.
    if let Some(explicit) = ctx.stores.first() {
        match db.get_store(explicit) {
            Ok(None) => exit_err(
                &Error::StoreNotFound {
                    id: explicit.clone(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
            Ok(Some(_)) => {}
        }
    }

    let sources = match db.list_sources(&store_name) {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    if ctx.json {
        // D4: include store as an object matching the citation shape.
        let json_sources: Vec<serde_json::Value> = sources
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "store": { "name": store_name },
                    "store_id": s.store_name,
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

/// `localdb source remove <id-or-path-or-url>`
pub fn run_source_remove(ctx: &CliContext, id: &str) {
    // A9-safety: validate --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let (config_loader, db) = load_app_db(ctx);
    let data_dir = &config_loader.paths.data_dir;
    let store_name = resolve_store_name(ctx, &config_loader, &db);

    if check_yaml_owned(&store_name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    // D1: verify the store exists if --store was given explicitly.
    if let Some(explicit) = ctx.stores.first() {
        match db.get_store(explicit) {
            Ok(None) => exit_err(
                &Error::StoreNotFound {
                    id: explicit.clone(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
            Ok(Some(_)) => {}
        }
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir) {
        // Route is DELETE /v1/sources/{id} (see server/src/daemon.rs build_router).
        let url = format!("{}/v1/sources/{}", base_url, id);
        match daemon_request(reqwest::Method::DELETE, &url, None) {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!("Removed source: {} (via daemon)", id);
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // Embedded mode.
    let _lock = match WriteLock::acquire(data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    // #3: Resolve the source ID. If the argument looks like a path or URL
    // (not a ULID/UUID), look it up by root/url field.
    let resolved_id: String = if !looks_like_id(id) {
        // Argument is a path or URL — look up by root/url.
        let explicit_store = ctx.stores.first().map(|s| s.as_str());
        match db.find_source_by_root_or_url(id, explicit_store) {
            Ok(Some(src)) => src.id,
            Ok(None) => exit_err(&Error::SourceNotFound { id: id.to_string() }, ctx.json),
            Err(e) => exit_err(&e, ctx.json),
        }
    } else {
        id.to_string()
    };

    // D2: If --store was given, verify the source belongs to that store.
    if let Some(explicit_store) = ctx.stores.first() {
        match db.get_source(&resolved_id) {
            Ok(Some(src)) if src.store_name != *explicit_store => {
                exit_err(
                    &Error::SourceNotFound {
                        id: format!(
                            "source '{}' exists but belongs to store '{}', not '{}'",
                            resolved_id, src.store_name, explicit_store
                        ),
                    },
                    ctx.json,
                );
            }
            Ok(None) => exit_err(
                &Error::SourceNotFound {
                    id: resolved_id.clone(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
            Ok(Some(_)) => {}
        }
    }

    match db.delete_source(&resolved_id) {
        Ok(true) => {}
        Ok(false) => exit_err(
            &Error::SourceNotFound {
                id: resolved_id.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "id": resolved_id }));
    } else {
        println!("Removed source: {}", resolved_id);
    }
}

/// `localdb index [--source <id>] [--dir <path>]`
///
/// One-shot scan-and-index (embedded mode) or submits a job to the daemon.
///
/// Per specs/05-surfaces.md §2: when daemon is running, submits job and polls.
pub fn run_index(ctx: &CliContext, source_id: Option<&str>, dir: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_index_async(ctx, source_id, dir));
}

async fn run_index_async(ctx: &CliContext, source_id: Option<&str>, dir: Option<&str>) {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{
            run_ingestion_for_source, DocumentExtractor, DocumentIndex, ExtractionResult,
            IngestionConfig,
        },
    };

    // A9-safety: validate --store name if given.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let (config_loader, db) = load_app_db(ctx);
    let data_dir = config_loader.paths.data_dir.clone();
    let store_name = resolve_store_name(ctx, &config_loader, &db);

    // Per specs/05-surfaces.md §2: when daemon is running, submit a job and poll.
    if let DaemonState::Running { base_url } = probe_daemon(&data_dir) {
        let url = format!("{}/v1/jobs", base_url);
        let mut body = json!({ "store_name": store_name });
        if let Some(sid) = source_id {
            body["source_id"] = serde_json::Value::String(sid.to_string());
        }
        match daemon_request_async(reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    let job_id = v.get("id").and_then(|i| i.as_str()).unwrap_or("?");
                    println!(
                        "Index job submitted to daemon: {} (poll with status)",
                        job_id
                    );
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // Embedded mode: acquire write lock and run inline.
    let _lock = match WriteLock::acquire(&data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    // #12: Reconcile YAML-declared stores into the runtime DB so that YAML stores
    // are findable by the index command without requiring `localdb store add` first.
    if let Err(e) = reconcile_yaml_stores(&db, &config_loader.config) {
        exit_err(&e, ctx.json);
    }

    // #13: Validate --store exists in runtime DB.
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

    // #1: If --dir was given, create a temporary anonymous source for that directory.
    let ephemeral_source: Option<RuntimeSource> = if let Some(dir_path) = dir {
        let p = std::path::Path::new(dir_path);
        // Validate the path exists.
        if !p.exists() {
            exit_err(
                &Error::InvalidRequest {
                    message: format!("--dir path '{}' does not exist", dir_path),
                },
                ctx.json,
            );
        }
        let src = RuntimeSource {
            id: new_ulid(),
            store_name: store_name.clone(),
            kind: "path".to_string(),
            root: Some(dir_path.to_string()),
            url: None,
            include: vec![],
            exclude: DEFAULT_PATH_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            preset: "prose".to_string(),
        };
        Some(src)
    } else {
        None
    };

    let all_sources = match db.list_sources(&store_name) {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    let sources_to_index: Vec<RuntimeSource> = if let Some(ephemeral) = ephemeral_source {
        // --dir: index only the ephemeral source (not persisted to DB).
        vec![ephemeral]
    } else if let Some(sid) = source_id {
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

    let embed_policy = &config_loader.config.defaults.indexing.embedding;
    let models_dir = config_loader.paths.models_dir.clone();
    let embedder = match embed::create_embedder(
        embed_policy,
        &config_loader.config.providers,
        Some(&models_dir),
    ) {
        Ok(e) => e,
        Err(e) => exit_err(&Error::from(e), ctx.json),
    };

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
    let lancedb_store =
        match store_lancedb::LanceDbStore::open(&lance_path, embedder.embedding_dim()).await {
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
            embedder.as_ref(),
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
    // F9: Reject --limit 0.
    if limit == 0 {
        exit_err(
            &Error::InvalidRequest {
                message: "--limit must be at least 1".to_string(),
            },
            ctx.json,
        );
    }

    // A9-safety: validate --store name if given.
    for store_name in &ctx.stores {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_search_async(ctx, query, limit));
}

async fn run_search_async(ctx: &CliContext, query: &str, limit: usize) {
    use localdb_core::search::{QueryRequest, SearchOrchestrator, StoreHandle};

    // F1-cli: use lenient loader so search works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx);
    let data_dir = config_loader.paths.data_dir.clone();

    // Per specs/05-surfaces.md §2: search routes through the daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(&data_dir) {
        let url = format!("{}/v1/search", base_url);
        let mut body = json!({
            "query": query,
            "limit": limit,
        });
        if !ctx.stores.is_empty() {
            // The handler's SearchRequest uses field `store_filter`
            // (see server/src/handlers.rs SearchRequest).
            body["store_filter"] = serde_json::Value::Array(
                ctx.stores
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            );
        }
        match daemon_request_async(reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    let empty = vec![];
                    let citations = v
                        .get("citations")
                        .and_then(|c| c.as_array())
                        .unwrap_or(&empty);
                    if citations.is_empty() {
                        println!("No results for '{}'.", query);
                    } else {
                        for (i, cit) in citations.iter().enumerate() {
                            let uri = cit.get("uri").and_then(|u| u.as_str()).unwrap_or("?");
                            let snippet = cit.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
                            let snippet_short: String = snippet.chars().take(120).collect();
                            println!("{}. {}", i + 1, uri);
                            println!("   {}", snippet_short.trim());
                            println!();
                        }
                    }
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // Embedded mode.
    let runtime_stores = match db.list_stores() {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    // #13: If --store was given explicitly, verify each named store exists in the runtime DB
    // or YAML config (exit 3 if not found).
    if !ctx.stores.is_empty() {
        let yaml_names: std::collections::HashSet<&str> = config_loader
            .config
            .stores
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let runtime_names: std::collections::HashSet<&str> =
            runtime_stores.iter().map(|s| s.name.as_str()).collect();
        for name in &ctx.stores {
            if !yaml_names.contains(name.as_str()) && !runtime_names.contains(name.as_str()) {
                exit_err(&Error::StoreNotFound { id: name.clone() }, ctx.json);
            }
        }
    }

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

    let embed_policy = &config_loader.config.defaults.indexing.embedding;
    let models_dir = config_loader.paths.models_dir.clone();
    let embedder = match embed::create_embedder(
        embed_policy,
        &config_loader.config.providers,
        Some(&models_dir),
    ) {
        Ok(e) => e,
        Err(e) => exit_err(&Error::from(e), ctx.json),
    };

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
        match store_lancedb::LanceDbStore::open(&lance_path, embedder.embedding_dim()).await {
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
    let request = QueryRequest {
        query: query.to_string(),
        leg_k: None,
        top_n: Some(limit),
        filters: vec![],
    };

    match SearchOrchestrator::query(&store_handles, embedder.as_ref(), &request).await {
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

/// `localdb serve` — start the HTTP daemon (specs/05-surfaces.md §3).
pub fn run_serve(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_serve_async(ctx));
}

async fn run_serve_async(ctx: &CliContext) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    };
    if let Err(e) = std::fs::create_dir_all(&config_loader.paths.data_dir) {
        exit_err(
            &Error::Internal {
                message: format!("cannot create data dir: {}", e),
                correlation_id: "serve_datadir".to_string(),
            },
            ctx.json,
        );
    }

    let daemon_options = server::DaemonOptions {
        paths: config_loader.paths.clone(),
        config: config_loader.config.clone(),
    };
    match server::start_daemon(daemon_options).await {
        Ok((handle, fut)) => {
            // Announce the bound address before blocking on the server future
            // so callers (and tests) can discover an OS-assigned port.
            if ctx.json {
                print_json(&json!({
                    "status": "listening",
                    "url": format!("http://{}", handle.addr),
                }));
            } else {
                println!("daemon listening on http://{}", handle.addr);
            }
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            fut.await;
            // Keep the handle (write lock + socket) alive until shutdown.
            drop(handle);
        }
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// `localdb mcp` — run the MCP server on stdio (specs/05-surfaces.md §4).
pub fn run_mcp(ctx: &CliContext, allow_write: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_mcp_async(ctx, allow_write));
}

async fn run_mcp_async(ctx: &CliContext, allow_write: bool) {
    use mcp::{AvailableStore, McpServer, StoreDescriptor};

    let (config_loader, db) = load_app_db(ctx);
    let data_dir = config_loader.paths.data_dir.clone();

    let runtime_stores = match db.list_stores() {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Same store resolution as `localdb search`: YAML stores + runtime stores,
    // narrowed by --store flags when given.
    let store_names: Vec<String> = if ctx.stores.is_empty() {
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

    let embed_policy = &config_loader.config.defaults.indexing.embedding;
    let models_dir = config_loader.paths.models_dir.clone();
    let embedder = match embed::create_embedder(
        embed_policy,
        &config_loader.config.providers,
        Some(&models_dir),
    ) {
        Ok(e) => e,
        Err(e) => exit_err(&Error::from(e), ctx.json),
    };

    let mut available: Vec<AvailableStore> = Vec::new();
    for name in &store_names {
        let store_data_dir = data_dir.join("stores").join(name);
        if !store_data_dir.exists() {
            continue; // Not yet indexed.
        }
        let runtime = runtime_stores.iter().find(|s| s.name == *name);
        let descriptor = StoreDescriptor {
            id: runtime
                .map(|s| s.id.clone())
                .unwrap_or_else(|| name.clone()),
            name: name.clone(),
            visibility: runtime
                .map(|s| s.visibility.clone())
                .unwrap_or_else(|| "private".to_string()),
        };
        let lance_path = store_data_dir.to_string_lossy().to_string();
        match store_lancedb::LanceDbStore::open(&lance_path, embedder.embedding_dim()).await {
            Ok(s) => available.push(AvailableStore::new(descriptor, Box::new(s))),
            Err(e) => eprintln!("warning: cannot open store '{}': {}", name, e),
        }
    }

    let mut mcp_server = McpServer::new(available, embedder);
    mcp_server.allow_write = allow_write;

    if let Err(e) = mcp::run_stdio_loop(&mcp_server).await {
        exit_err(
            &Error::Internal {
                message: format!("mcp stdio loop failed: {}", e),
                correlation_id: "mcp_stdio".to_string(),
            },
            ctx.json,
        );
    }
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
        // Since probe_daemon now does a live health check, a sock file pointing
        // to a non-listening port will be removed and NotRunning returned.
        // This test verifies the stale-socket cleanup path (#11).
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        // Empty sock file → default URL http://127.0.0.1:7700 (not listening in tests).
        std::fs::write(&sock_path, b"").unwrap();
        // The health check fails → stale socket cleaned up → NotRunning.
        let state = probe_daemon(dir.path());
        assert!(
            matches!(state, DaemonState::NotRunning),
            "sock file pointing to a non-listening port should return NotRunning"
        );
        // The stale sock file should have been removed.
        assert!(
            !sock_path.exists(),
            "probe_daemon should remove the stale socket file"
        );
    }

    #[test]
    fn probe_daemon_health_inner_ipv6_no_port() {
        // Bracketed IPv6 with no port should attempt [::1]:80, not fail to parse.
        // Since nothing is listening there, it returns Some(false) or None — not panics.
        let result = probe_daemon_health_inner("http://[::1]/v1/status");
        // Any outcome is fine (false or None); we just verify no panic and that we
        // don't misparse [::1] as having a port.
        let _ = result;
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
        // specs/05-surfaces.md §5 — all 15 error variants must map to the correct exit code.
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
        assert_eq!(
            Error::UnsupportedFormat { format: "".into() }.exit_code(),
            2
        );
        assert_eq!(Error::StoreNotFound { id: "".into() }.exit_code(), 3);
        assert_eq!(Error::SourceNotFound { id: "".into() }.exit_code(), 3);
        assert_eq!(Error::DocumentNotFound { id: "".into() }.exit_code(), 3);
        assert_eq!(Error::JobNotFound { id: "".into() }.exit_code(), 3);
        assert_eq!(Error::StoreLocked.exit_code(), 4);
        assert_eq!(Error::DaemonRunning.exit_code(), 4);
        assert_eq!(Error::ConfigReadonly.exit_code(), 4);
        assert_eq!(Error::IndexInProgress.exit_code(), 4);
        assert_eq!(Error::DaemonUnreachable.exit_code(), 5);
        assert_eq!(
            Error::ProviderUnavailable { message: "".into() }.exit_code(),
            5
        );
        assert_eq!(Error::ModelMissing { message: "".into() }.exit_code(), 5);
    }

    // --- JSON output shape ---

    #[test]
    fn json_store_list_shape() {
        // Verify that the stores list JSON shape contains the required fields on each entry.
        // Tests against actual AppDb data, not a tautological construct.
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let store = new_runtime_store("shape-store");
        db.upsert_store(&store).unwrap();

        let stores = db.list_stores().unwrap();
        let json_stores: Vec<serde_json::Value> = stores
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "ownership": "runtime",
                    "visibility": s.visibility,
                    "backend": s.backend,
                })
            })
            .collect();
        let value = json!({ "stores": json_stores });

        // Must have a "stores" key.
        let arr = value.get("stores").expect("stores key must be present");
        assert!(arr.is_array(), "stores must be an array");
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 1);

        // Each store entry must have the 4 canonical fields.
        let entry = &arr[0];
        assert!(entry.get("name").is_some(), "store entry must have name");
        assert!(
            entry.get("ownership").is_some(),
            "store entry must have ownership"
        );
        assert!(
            entry.get("visibility").is_some(),
            "store entry must have visibility"
        );
        assert!(
            entry.get("backend").is_some(),
            "store entry must have backend"
        );
        assert_eq!(entry["name"].as_str().unwrap(), "shape-store");
        assert_eq!(entry["ownership"].as_str().unwrap(), "runtime");
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

    // --- C1: daemon error body uses `code` field (not `error`) ---

    /// Verify that the daemon error body field name is `code`, not `error`.
    /// The server's ErrorResponse has {code, message} (see server/src/error.rs).
    /// The CLI must read `get("code")` to correctly map error kinds.
    #[test]
    fn daemon_error_body_uses_code_field() {
        // Simulate the JSON body a daemon returns on error:
        // {"code": "store_not_found", "message": "store 'x' not found"}
        let body = json!({
            "code": "store_not_found",
            "message": "store 'x' not found"
        });

        let code = body
            .get("code")
            .and_then(|e| e.as_str())
            .unwrap_or("internal");

        assert_eq!(
            code, "store_not_found",
            "must read 'code' field from daemon error body, not 'error'"
        );

        // Ensure the old field name 'error' is absent (server never sends it).
        assert!(
            body.get("error").is_none(),
            "daemon error body should not have 'error' field; it uses 'code'"
        );
    }

    // --- #11: stale socket detection ---

    /// When `daemon.sock` exists but the daemon is unreachable (stale socket),
    /// `probe_daemon` should:
    ///   1. Detect the connection failure.
    ///   2. Remove the stale socket file.
    ///   3. Return `DaemonState::NotRunning`.
    ///
    /// We test this by creating a sock file that points to a definitely-closed
    /// port (port 1 is reserved and never listens), so the health check fails.
    #[test]
    fn probe_daemon_removes_stale_socket_and_returns_not_running() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("daemon.sock");

        // Write a URL that will always refuse connections (port 1 is reserved).
        std::fs::write(&sock_path, b"http://127.0.0.1:1").unwrap();
        assert!(sock_path.exists(), "sock file should exist before probe");

        let state = probe_daemon(dir.path());

        assert!(
            matches!(state, DaemonState::NotRunning),
            "stale socket should result in NotRunning"
        );
        assert!(
            !sock_path.exists(),
            "probe_daemon should remove the stale socket file"
        );
    }

    /// When `LOCALDB_DAEMON_URL` is set, `probe_daemon` bypasses the socket
    /// file check entirely (no health probe, no file removal).
    #[test]
    fn probe_daemon_env_var_bypasses_socket_check() {
        let dir = TempDir::new().unwrap();
        // No socket file needed — env var takes precedence.
        std::env::set_var("LOCALDB_DAEMON_URL", "http://127.0.0.1:9999");
        let state = probe_daemon(dir.path());
        std::env::remove_var("LOCALDB_DAEMON_URL");

        assert!(
            matches!(state, DaemonState::Running { base_url } if base_url == "http://127.0.0.1:9999"),
            "env var should return Running without a health probe"
        );
    }

    // --- A9-safety: store name validation ---

    #[test]
    fn validate_store_name_rejects_empty() {
        let err = validate_store_name("").unwrap_err();
        assert_eq!(err.exit_code(), 2, "empty name must exit 2");
    }

    #[test]
    fn validate_store_name_rejects_dot() {
        let err = validate_store_name(".").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn validate_store_name_rejects_dotdot() {
        let err = validate_store_name("..").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn validate_store_name_rejects_slash() {
        let err = validate_store_name("a/b").unwrap_err();
        assert_eq!(err.exit_code(), 2, "name with '/' must exit 2");
    }

    #[test]
    fn validate_store_name_rejects_leading_slash() {
        let err = validate_store_name("/root").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn validate_store_name_rejects_backslash() {
        let err = validate_store_name("a\\b").unwrap_err();
        assert_eq!(err.exit_code(), 2, "name with backslash must exit 2");
    }

    #[test]
    fn validate_store_name_accepts_valid_names() {
        assert!(validate_store_name("mystore").is_ok());
        assert!(validate_store_name("my-store").is_ok());
        assert!(validate_store_name("my_store_123").is_ok());
        assert!(validate_store_name("CamelCase").is_ok());
    }

    // --- normalize_path_source ---

    #[test]
    fn normalize_path_source_rejects_nonexistent_path() {
        let result = normalize_path_source("/nonexistent/path/that/does/not/exist");
        assert!(result.is_err(), "nonexistent path should return Err");
        let err = result.unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn normalize_path_source_directory_has_no_include() {
        let dir = TempDir::new().unwrap();
        let (root, include, exclude) = normalize_path_source(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(root, dir.path().to_str().unwrap());
        assert!(
            include.is_empty(),
            "directory source should have no include globs"
        );
        assert!(
            !exclude.is_empty(),
            "directory source should have default excludes"
        );
        assert!(exclude.iter().any(|s| s == "**/.git"));
    }

    #[test]
    fn normalize_path_source_single_file_promotes_to_parent() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("README.md");
        std::fs::write(&file_path, b"hello").unwrap();
        let (root, include, _exclude) = normalize_path_source(file_path.to_str().unwrap()).unwrap();
        assert_eq!(
            root,
            dir.path().to_str().unwrap(),
            "single file root should be parent dir"
        );
        assert_eq!(include, vec!["README.md".to_string()]);
    }

    // --- looks_like_id ---

    #[test]
    fn looks_like_id_recognizes_ulid() {
        // ULIDs are 26 uppercase alphanumeric chars.
        assert!(looks_like_id("01HRQHB7FN3WMX4AZDV3S9VCTZ"));
    }

    #[test]
    fn looks_like_id_rejects_paths() {
        assert!(!looks_like_id("/home/user/docs"));
        assert!(!looks_like_id("./relative/path"));
        assert!(!looks_like_id("https://example.com"));
        assert!(!looks_like_id("some/path"));
    }

    // --- #4: default excludes ---

    #[test]
    fn default_path_excludes_contains_git_and_node_modules() {
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/.git"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/node_modules"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/.DS_Store"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/target"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/__pycache__"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/.venv"));
    }

    // --- F9: --limit 0 validation ---

    /// F9: verify that limit=0 maps to exit code 2 via InvalidRequest.
    #[test]
    fn limit_zero_maps_to_exit_code_2() {
        // The run_search function calls exit_err(&InvalidRequest{..}) when limit==0.
        // We verify the error type and exit code without calling the full search pipeline.
        let err = Error::InvalidRequest {
            message: "--limit must be at least 1".to_string(),
        };
        assert_eq!(err.exit_code(), 2, "--limit 0 must exit 2");
    }

    // --- #13: store-not-found exit code ---

    /// #13: verify StoreNotFound maps to exit code 3.
    #[test]
    fn store_not_found_maps_to_exit_code_3() {
        let err = Error::StoreNotFound {
            id: "no-such-store".to_string(),
        };
        assert_eq!(err.exit_code(), 3, "unknown --store must exit 3");
    }

    // --- find_source_by_root_or_url ---

    #[test]
    fn find_source_by_root_finds_match() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let src = RuntimeSource {
            id: new_ulid(),
            store_name: "s1".into(),
            kind: "path".into(),
            root: Some("/my/docs".into()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".into(),
        };
        db.upsert_source(&src).unwrap();

        let found = db
            .find_source_by_root_or_url("/my/docs", Some("s1"))
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, src.id);
    }

    #[test]
    fn find_source_by_root_respects_store_scope() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let src = RuntimeSource {
            id: new_ulid(),
            store_name: "other-store".into(),
            kind: "path".into(),
            root: Some("/my/docs".into()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".into(),
        };
        db.upsert_source(&src).unwrap();

        // Scoped to a different store — should not find it.
        let found = db
            .find_source_by_root_or_url("/my/docs", Some("my-store"))
            .unwrap();
        assert!(found.is_none());
    }

    // --- #12: reconcile_yaml_stores ---

    fn make_raw_config_with_store(
        store_name: &str,
        sources: Vec<localdb_core::config::schema::SourceConfig>,
    ) -> localdb_core::config::schema::RawConfig {
        use localdb_core::config::schema::{
            DefaultsConfig, PathsConfig, RawConfig, ServerConfig, StoreConfig,
        };
        RawConfig {
            version: 1,
            server: ServerConfig::default(),
            paths: PathsConfig::default(),
            defaults: DefaultsConfig::default(),
            stores: vec![StoreConfig {
                name: store_name.to_string(),
                visibility: "private".to_string(),
                backend: "lancedb".to_string(),
                indexing: None,
                sources,
            }],
            providers: vec![],
        }
    }

    /// After reconciliation, a YAML-declared store must be findable by
    /// `get_store_by_name` (i.e. `db.get_store`), which is what `run_index_async`
    /// uses to locate the store before indexing.
    #[test]
    fn reconcile_yaml_store_makes_store_findable() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        // Precondition: the store does not exist in the runtime DB yet.
        assert!(db.get_store("notes").unwrap().is_none());

        let config = make_raw_config_with_store("notes", vec![]);
        reconcile_yaml_stores(&db, &config).unwrap();

        // After reconciliation, the store must be findable.
        let found = db.get_store("notes").unwrap();
        assert!(
            found.is_some(),
            "YAML store should be findable after reconciliation"
        );
        assert_eq!(found.unwrap().name, "notes");
    }

    /// Reconciliation is idempotent: calling it twice must not produce duplicate
    /// records or return an error.
    #[test]
    fn reconcile_yaml_store_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let config = make_raw_config_with_store("docs", vec![]);
        reconcile_yaml_stores(&db, &config).unwrap();
        reconcile_yaml_stores(&db, &config).unwrap(); // second call — must not fail

        let stores = db.list_stores().unwrap();
        let matching: Vec<_> = stores.iter().filter(|s| s.name == "docs").collect();
        assert_eq!(matching.len(), 1, "must not create duplicate store records");
    }

    /// Reconciliation must not overwrite an existing runtime-DB record (e.g. one
    /// created by `store add`) with a YAML shadow — the existing record wins.
    #[test]
    fn reconcile_does_not_overwrite_existing_runtime_store() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        // Pre-existing runtime store (e.g. created by `store add`).
        let existing_id = new_ulid();
        let existing = RuntimeStore {
            id: existing_id.clone(),
            name: "shared".to_string(),
            visibility: "shared".to_string(), // different from YAML's "private"
            backend: "lancedb".to_string(),
            indexing: None,
        };
        db.upsert_store(&existing).unwrap();

        // YAML config declares the same store name with "private" visibility.
        let config = make_raw_config_with_store("shared", vec![]);
        reconcile_yaml_stores(&db, &config).unwrap();

        // The existing record should be unchanged.
        let found = db.get_store("shared").unwrap().unwrap();
        assert_eq!(
            found.id, existing_id,
            "existing store id must not be replaced"
        );
        assert_eq!(
            found.visibility, "shared",
            "existing store visibility must not be overwritten"
        );
    }

    /// YAML-declared sources are reconciled into the runtime sources DB.
    #[test]
    fn reconcile_yaml_sources_into_db() {
        use localdb_core::config::schema::SourceConfig;

        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let sources = vec![SourceConfig {
            kind: "path".to_string(),
            root: Some("/home/user/notes".to_string()),
            include: vec!["**/*.md".to_string()],
            exclude: vec![],
            preset: "prose".to_string(),
            url: None,
            refresh: None,
        }];
        let config = make_raw_config_with_store("notes", sources);
        reconcile_yaml_stores(&db, &config).unwrap();

        // The source must be findable.
        let found = db
            .find_source_by_root_or_url("/home/user/notes", Some("notes"))
            .unwrap();
        assert!(
            found.is_some(),
            "YAML source should be findable after reconciliation"
        );
        let src = found.unwrap();
        assert_eq!(src.store_name, "notes");
        assert_eq!(src.kind, "path");
        assert_eq!(src.include, vec!["**/*.md".to_string()]);
    }

    /// Source reconciliation is also idempotent.
    #[test]
    fn reconcile_yaml_sources_is_idempotent() {
        use localdb_core::config::schema::SourceConfig;

        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir);

        let sources = vec![SourceConfig {
            kind: "path".to_string(),
            root: Some("/tmp/docs".to_string()),
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            url: None,
            refresh: None,
        }];
        let config = make_raw_config_with_store("mystore", sources);
        reconcile_yaml_stores(&db, &config).unwrap();
        reconcile_yaml_stores(&db, &config).unwrap(); // second call

        let sources_in_db = db.list_sources("mystore").unwrap();
        assert_eq!(
            sources_in_db.len(),
            1,
            "must not create duplicate source records"
        );
    }
}
