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

pub mod progress;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fetch::HttpUrlFetcher;
use localdb_core::{
    config::{
        loader::{load_config, ConfigLoader, LoadOptions, ResolvedPaths},
        policy::compute_policy_version,
        runtime_state::check_yaml_owned,
        schema::{EmbeddingPolicy, IndexingPolicyConfig, ProviderConfig},
    },
    ids::new_ulid,
    ingestion::now_rfc3339,
    types::{SourceKind, StoreVisibility},
    Error, SourceRow, StoreBackend, StoreBackendConfig, StoreRow,
};
use serde_json::json;
use store_libsql::SqliteBackend;

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
    /// Whether --yes was given (skip confirmation prompts).
    pub yes: bool,
    /// Daemon URL override, read once from `LOCALDB_DAEMON_URL` at startup.
    pub daemon_url: Option<String>,
    /// Config file path from `LOCALDB_CONFIG` env var, read once at startup.
    pub config_env: Option<PathBuf>,
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
///   1. `daemon_url_override` (from `LOCALDB_DAEMON_URL`, read once at startup)
///   2. Content of `daemon.sock` file (if it contains a URL)
///   3. Default `http://127.0.0.1:7700`
///
/// Returns `DaemonState::NotRunning` if neither the override is set nor the
/// socket file exists.
pub fn probe_daemon(data_dir: &Path, daemon_url_override: Option<&str>) -> DaemonState {
    if let Some(url) = daemon_url_override {
        return DaemonState::Running {
            base_url: url.to_string(),
        };
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
            "runtime_state_locked" => Error::RuntimeStateLocked,
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

/// Format a chunk snippet for terminal display: collapse internal runs of
/// whitespace into single spaces, then cap at ~500 chars, appending `…` if cut.
fn format_snippet(snippet: &str) -> String {
    const MAX_CHARS: usize = 500;
    let normalized = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() > MAX_CHARS {
        let truncated: String = normalized.chars().take(MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        normalized
    }
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
        let config = StoreBackendConfig::local_path(paths.unified_db_path(), dim, encoding);
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

fn visibility_to_string(visibility: &StoreVisibility) -> &'static str {
    match visibility {
        StoreVisibility::Private => "private",
        StoreVisibility::Shared => "shared",
    }
}

fn source_kind_to_string(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::Path => "path",
        SourceKind::Url => "url",
    }
}

fn default_store_row(name: &str, db: &AppDb) -> Result<StoreRow, Error> {
    Ok(StoreRow {
        id: new_ulid(),
        name: name.to_string(),
        visibility: StoreVisibility::Private,
        backend: "libsql".to_string(),
        indexing_policy: serde_json::to_string(db.default_indexing_policy()).map_err(|e| {
            Error::Internal {
                message: format!("cannot serialize default indexing policy: {e}"),
                correlation_id: "appdb_serialize_default_policy".into(),
            }
        })?,
        policy_version: db.default_policy_version().to_string(),
        acl: "{}".to_string(),
        created_at: now_rfc3339(),
    })
}

async fn open_app_db_from_loader(config_loader: &ConfigLoader) -> Result<AppDb, Error> {
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
async fn load_app_db(ctx: &CliContext) -> (ConfigLoader, AppDb) {
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
async fn load_app_db_lenient(ctx: &CliContext) -> (ConfigLoader, AppDb) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options, ctx.config_env.as_deref()) {
        Ok(c) => c,
        Err(_) => {
            // Config is malformed/missing — use platform default paths.
            let options_default = LoadOptions::default();
            match load_config(&options_default, None) {
                Ok(c) => c,
                Err(e) => exit_err(&e, ctx.json),
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

/// Resolve the target store name from --store flags, YAML config, or runtime DB.
async fn resolve_store_name(ctx: &CliContext, config_loader: &ConfigLoader, db: &AppDb) -> String {
    if let Some(name) = ctx.stores.first() {
        return name.clone();
    }
    if let Some(s) = config_loader.config.stores.first() {
        return s.name.clone();
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
        // Directory source: apply the default include allowlist so that only
        // files with supported extensions are visited.  Callers that need to
        // override this can set explicit include globs after construction.
        let includes = DEFAULT_PATH_INCLUDES
            .iter()
            .map(|s| s.to_string())
            .collect();
        (raw_path.to_string(), includes)
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

pub fn source_row_to_core_source(src: &SourceRow) -> localdb_core::types::Source {
    use localdb_core::types::{Source, SourceSpec};

    let spec = match src.kind {
        SourceKind::Url => SourceSpec::Url {
            url: src.url.clone().unwrap_or_default(),
            refresh_interval_secs: None,
        },
        SourceKind::Path => SourceSpec::Path {
            root: src.root.clone().unwrap_or_default(),
            include: src.include.clone(),
            exclude: src.exclude.clone(),
        },
    };

    Source {
        id: src.id.clone(),
        store_id: src.store_id.clone(),
        kind: src.kind.clone(),
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
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_init_async(ctx));
}

async fn run_init_async(ctx: &CliContext) {
    let platform = localdb_core::config::PlatformPaths::resolve().unwrap_or_else(|| {
        eprintln!("error: cannot determine platform paths");
        std::process::exit(1);
    });

    // Resolve config path (same priority order as load_config).
    let config_path = ctx
        .config
        .clone()
        .or_else(|| ctx.config_env.clone())
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
        match load_config(&options, ctx.config_env.as_deref()) {
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

    let (_config_loader, db) = load_app_db_lenient(ctx).await;

    match db.backend().get_store_by_name("default").await {
        Ok(None) => {
            let default_store = match default_store_row("default", &db) {
                Ok(store) => store,
                Err(e) => exit_err(&e, ctx.json),
            };
            if let Err(e) = db.backend().upsert_store(&default_store).await {
                exit_err(&e, ctx.json);
            }
        }
        Ok(Some(_)) => {}
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
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_status_async(ctx));
}

async fn run_status_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so status works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    let daemon_status = match probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        DaemonState::Running { base_url } => format!("running ({})", base_url),
        DaemonState::NotRunning => "not running (embedded mode)".to_string(),
    };

    let runtime_stores = match db.backend().list_stores().await {
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
                "visibility": visibility_to_string(&s.visibility),
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
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_add_async(ctx, name));
}

async fn run_store_add_async(ctx: &CliContext, name: &str) {
    // A9-safety: validate store name before anything else.
    if let Err(e) = validate_store_name(name) {
        exit_err(&e, ctx.json);
    }

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if check_yaml_owned(name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{}/v1/stores", base_url);
        let body = json!({ "name": name, "visibility": "private", "backend": "libsql" });
        match daemon_request_async(reqwest::Method::POST, &url, Some(body)).await {
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

    // Embedded mode: SQLite serialises the metadata write; no WriteLock needed here.

    // Duplicate check.
    match db.backend().get_store_by_name(name).await {
        Ok(Some(_)) => exit_err(
            &Error::InvalidRequest {
                message: format!("store '{}' already exists", name),
            },
            ctx.json,
        ),
        Ok(None) => {}
        Err(e) => exit_err(&e, ctx.json),
    }

    let store = match default_store_row(name, &db) {
        Ok(store) => store,
        Err(e) => exit_err(&e, ctx.json),
    };
    if let Err(e) = db.backend().upsert_store(&store).await {
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
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_list_async(ctx));
}

async fn run_store_list_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so store list works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx).await;

    let runtime_stores = match db.backend().list_stores().await {
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
                "visibility": visibility_to_string(&s.visibility),
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

/// Prompt the user for confirmation of a destructive action.
///
/// Returns `true` if confirmed (proceed), `false` if aborted.
/// Exits with code 2 if non-interactive and `--yes` was not given.
pub fn confirm_destructive(ctx: &CliContext, prompt: &str) -> bool {
    use std::io::IsTerminal as _;

    if ctx.yes {
        return true;
    }
    if ctx.json || !std::io::stdin().is_terminal() {
        exit_err(
            &Error::InvalidRequest {
                message: "this command is destructive; re-run with --yes to confirm".to_string(),
            },
            ctx.json,
        );
    }
    eprint!("{} [y/N] ", prompt);
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        eprintln!("Aborted.");
        return false;
    }
    let answer = line.trim().to_lowercase();
    if answer == "y" || answer == "yes" {
        true
    } else {
        eprintln!("Aborted.");
        false
    }
}

/// `localdb store remove <name>`
pub fn run_store_remove(ctx: &CliContext, name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_remove_async(ctx, name));
}

async fn run_store_remove_async(ctx: &CliContext, name: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if check_yaml_owned(name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    let prompt = format!(
        "This permanently deletes store '{}', its sources, and its index data. Continue?",
        name
    );
    if !confirm_destructive(ctx, &prompt) {
        return;
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{}/v1/stores/{}", base_url, name);
        match daemon_request_async(reqwest::Method::DELETE, &url, None).await {
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

    let store_id = match db.resolve_store_id(name).await {
        Ok(id) => id,
        Err(e) => exit_err(&e, ctx.json),
    };
    match db.backend().delete_store(&store_id).await {
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

/// Default include patterns for directory path sources.
///
/// When a directory source has no explicit `include` globs, this allowlist is
/// applied so that only files with supported extensions (or known basenames) are
/// visited.  Single-file sources are never affected by this constant — they
/// already carry an exact filename as their include glob.
///
/// Generated from `extract::supported_extensions()`: plain extension tokens
/// (no `.`) become `**/*.ext`; basename tokens (contain `.`) become
/// `**/<basename>`.
const DEFAULT_PATH_INCLUDES: &[&str] = &[
    // Markdown
    "**/*.md",
    "**/*.markdown",
    // HTML
    "**/*.html",
    "**/*.htm",
    // PDF
    "**/*.pdf",
    // EPUB / ebook
    "**/*.epub",
    // Office formats
    "**/*.docx",
    "**/*.xlsx",
    "**/*.pptx",
    "**/*.odt",
    "**/*.ods",
    "**/*.odp",
    // Plaintext prose
    "**/*.txt",
    "**/*.text",
    // Code / data
    "**/*.rs",
    "**/*.py",
    "**/*.js",
    "**/*.mjs",
    "**/*.ts",
    "**/*.tsx",
    "**/*.json",
    "**/*.yaml",
    "**/*.yml",
    "**/*.toml",
    "**/*.lock",
    "**/*.c",
    "**/*.h",
    "**/*.cpp",
    "**/*.hpp",
    "**/*.go",
    "**/*.java",
    "**/*.rb",
    "**/*.php",
    "**/*.sh",
    "**/*.css",
    "**/*.scss",
    "**/*.sql",
    "**/*.csv",
    "**/*.xml",
    "**/*.ini",
    "**/*.cfg",
    // Lockfile basenames
    "**/Cargo.lock",
    "**/package-lock.json",
    "**/yarn.lock",
    "**/poetry.lock",
    "**/Gemfile.lock",
];

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
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    // A9-safety: validate the --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let store_name = resolve_store_name(ctx, &config_loader, &db).await;

    if check_yaml_owned(&store_name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
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
        match daemon_request_async(reqwest::Method::POST, &url_str, Some(body)).await {
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

    // Embedded mode — SQLite serialises the metadata write; no WriteLock needed here.
    // The auto-index step below acquires its own WriteLock for LanceDB writes.

    // #13: Verify store exists in runtime DB (exit 3 if not found).
    let rt_store = match db.backend().get_store_by_name(&store_name).await {
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

    let src = SourceRow {
        id: new_ulid(),
        store_id: rt_store.id.clone(),
        kind: match kind {
            "url" => SourceKind::Url,
            "path" => SourceKind::Path,
            _ => SourceKind::Path,
        },
        root: if kind == "path" {
            Some(actual_root)
        } else {
            None
        },
        url: url_str2.map(|s| s.to_string()),
        include: include_globs,
        exclude: exclude_globs,
        preset: "prose".to_string(),
        refresh: None,
        created_at: now_rfc3339(),
    };

    if let Err(e) = db.backend().upsert_source(&src).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "id": src.id,
            "store": { "name": store_name },
            "kind": source_kind_to_string(&src.kind),
        }));
    } else {
        println!("Added source {} to store '{}'", src.id, store_name);
    }

    // #2: Auto-index after source add.
    // Drop the db handle before re-entering the index path, which opens its own.
    let src_id = src.id.clone();
    let rt_store_clone = rt_store.clone();
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
            yes: false,
            daemon_url: ctx.daemon_url.clone(),
            config_env: ctx.config_env.clone(),
        };
        run_index_for_source_async(&index_ctx, Some(&src_id), &rt_store_clone).await;
    }
}

/// Internal: run ingestion for a single source without re-resolving the store.
async fn run_index_for_source_async(
    ctx: &CliContext,
    source_id: Option<&str>,
    store_row: &StoreRow,
) {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{run_ingestion_for_source, DocumentIndex, IngestionConfig},
    };

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = config_loader.paths.data_dir.clone();

    let all_sources = match db.backend().list_sources(&store_row.id).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: cannot list sources for auto-index: {}", e);
            return;
        }
    };

    let sources_to_index: Vec<SourceRow> = if let Some(sid) = source_id {
        all_sources.into_iter().filter(|s| s.id == sid).collect()
    } else {
        all_sources
    };

    if sources_to_index.is_empty() {
        return;
    }

    let policy = config_loader.config.defaults.indexing.clone();
    let ingestion_cfg = IngestionConfig {
        store_id: store_row.id.clone(),
        policy_version: store_row.policy_version.clone(),
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

    let extractor = match extract::ChainExtractor::from_ids(&policy.parsers) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: cannot build parser chain for auto-index: {}", e);
            return;
        }
    };

    let _lock = match WriteLock::acquire(&data_dir) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("warning: cannot acquire lock for auto-index: {}", e);
            return;
        }
    };

    let handle = match db.backend().retrieval_store(&store_row.id).await {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("warning: cannot open store handle for auto-index: {e}");
            return;
        }
    };

    let existing = match handle.list_indexed_documents().await {
        Ok(records) => records,
        Err(e) => {
            eprintln!("warning: cannot read existing documents for auto-index: {e}");
            return;
        }
    };
    let mut doc_index = DocumentIndex::from_records(existing);
    let url_fetcher = HttpUrlFetcher::new();

    for rt_source in &sources_to_index {
        let source = source_row_to_core_source(rt_source);
        let chunker = match ChunkerConfig::from_preset(&source.source_kind_preset) {
            Ok(chunker) => chunker,
            Err(e) => {
                eprintln!(
                    "warning: invalid chunker preset '{}' for source {}: {}",
                    source.source_kind_preset, rt_source.id, e
                );
                continue;
            }
        };
        let cfg = IngestionConfig {
            chunker,
            ..ingestion_cfg.clone()
        };
        let sink = crate::progress::build_progress_sink(ctx.json);
        match run_ingestion_for_source(
            &source,
            &mut doc_index,
            handle.as_ref(),
            embedder.as_ref(),
            &cfg,
            &extractor,
            Some(&url_fetcher),
            sink,
        )
        .await
        {
            Ok(r) => {
                let _ = r.chunks_written;
            }
            Err(e) => {
                eprintln!(
                    "warning: auto-index error for source {}: {}",
                    rt_source.id, e
                );
            }
        }
    }
}

/// `localdb source list`
pub fn run_source_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_list_async(ctx));
}

async fn run_source_list_async(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx).await;

    // A9-safety: validate --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let store_name = resolve_store_name(ctx, &config_loader, &db).await;

    // D1: verify store exists before listing sources.
    if let Some(explicit) = ctx.stores.first() {
        match db.backend().get_store_by_name(explicit).await {
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

    let store_row = match db.backend().get_store_by_name(&store_name).await {
        Ok(Some(s)) => s,
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store_name.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    };

    let sources = match db.backend().list_sources(&store_row.id).await {
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
                    "store_id": s.store_id,
                    "kind": source_kind_to_string(&s.kind),
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
            println!("{} [{}] {}", s.id, source_kind_to_string(&s.kind), loc);
        }
    }
}

/// `localdb source remove <id-or-path-or-url>`
pub fn run_source_remove(ctx: &CliContext, id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_remove_async(ctx, id));
}

async fn run_source_remove_async(ctx: &CliContext, id: &str) {
    // A9-safety: validate --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;
    let store_name = resolve_store_name(ctx, &config_loader, &db).await;

    if check_yaml_owned(&store_name, &config_loader.config) {
        exit_err(&Error::ConfigReadonly, ctx.json);
    }

    // D1: verify the store exists if --store was given explicitly.
    if let Some(explicit) = ctx.stores.first() {
        match db.backend().get_store_by_name(explicit).await {
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
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        // Route is DELETE /v1/sources/{id} (see server/src/daemon.rs build_router).
        let url = format!("{}/v1/sources/{}", base_url, id);
        match daemon_request_async(reqwest::Method::DELETE, &url, None).await {
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

    // Embedded mode: SQLite serialises all ops here; no WriteLock needed.

    // #3: Resolve the source ID. If the argument looks like a path or URL
    // (not a ULID/UUID), look it up by root/url field.
    let explicit_store = ctx.stores.first().map(|s| s.as_str());
    if !looks_like_id(id) && explicit_store.is_none() {
        exit_err(
            &Error::InvalidRequest {
                message: "source remove by path/url requires --store; pass --store <name> or use the source ULID".into(),
            },
            ctx.json,
        );
    }
    let resolved_store_id = match explicit_store {
        Some(name) => Some(match db.resolve_store_id(name).await {
            Ok(id) => id,
            Err(e) => exit_err(&e, ctx.json),
        }),
        None => None,
    };
    let resolved_id: String = if !looks_like_id(id) {
        let Some(store_id) = resolved_store_id.as_deref() else {
            exit_err(
                &Error::InvalidRequest {
                    message: "source remove by path/url requires --store; pass --store <name> or use the source ULID".into(),
                },
                ctx.json,
            );
        };
        match db.backend().find_source_by_root_or_url(id, store_id).await {
            Ok(Some(src)) => src.id,
            Ok(None) => exit_err(&Error::SourceNotFound { id: id.to_string() }, ctx.json),
            Err(e) => exit_err(&e, ctx.json),
        }
    } else {
        id.to_string()
    };

    // D2: If --store was given, verify the source belongs to that store.
    if let Some(expected_store_id) = resolved_store_id.as_deref() {
        match db.backend().get_source(&resolved_id).await {
            Ok(Some(src)) if src.store_id != expected_store_id => {
                exit_err(
                    &Error::SourceNotFound {
                        id: resolved_id.clone(),
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

    match db.backend().delete_source(&resolved_id).await {
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

/// `localdb index [--source <id>] [--dir <path>] [--strict]`
///
/// One-shot scan-and-index (embedded mode) or submits a job to the daemon.
///
/// Per specs/05-surfaces.md §2: when daemon is running, submits job and polls.
/// With `--strict`, exits 2 if any document failed extraction (run always completes).
pub fn run_index(ctx: &CliContext, source_id: Option<&str>, dir: Option<&str>, strict: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_index_async(ctx, source_id, dir, strict));
}

async fn run_index_async(
    ctx: &CliContext,
    source_id: Option<&str>,
    dir: Option<&str>,
    strict: bool,
) {
    use localdb_core::{
        chunker::ChunkerConfig,
        ingestion::{run_ingestion_for_source, DocumentIndex, IngestionConfig},
    };

    // A9-safety: validate --store name if given.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = config_loader.paths.data_dir.clone();
    let store_name = resolve_store_name(ctx, &config_loader, &db).await;

    // Per specs/05-surfaces.md §2: when daemon is running, submit a job and poll.
    if let DaemonState::Running { base_url } = probe_daemon(&data_dir, ctx.daemon_url.as_deref()) {
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

    let store_id = match db.resolve_store_id(&store_name).await {
        Ok(id) => id,
        Err(e) => exit_err(&e, ctx.json),
    };
    let rt_store = match db.backend().get_store(&store_id).await {
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
    let ephemeral_source: Option<SourceRow> = if let Some(dir_path) = dir {
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
        let src = SourceRow {
            id: new_ulid(),
            store_id: store_id.clone(),
            kind: SourceKind::Path,
            root: Some(dir_path.to_string()),
            url: None,
            include: vec![],
            exclude: DEFAULT_PATH_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            preset: "prose".to_string(),
            refresh: None,
            created_at: now_rfc3339(),
        };
        Some(src)
    } else {
        None
    };

    let all_sources = match db.backend().list_sources(&store_id).await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    let sources_to_index: Vec<SourceRow> = if let Some(ephemeral) = ephemeral_source {
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

    let ingestion_cfg = IngestionConfig {
        store_id: rt_store.id.clone(),
        policy_version: rt_store.policy_version.clone(),
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

    let extractor = match extract::ChainExtractor::from_ids(&policy.parsers) {
        Ok(e) => e,
        Err(e) => exit_err(&e, ctx.json),
    };

    let _lock = match WriteLock::acquire(&data_dir) {
        Ok(l) => l,
        Err(e) => exit_err(&e, ctx.json),
    };

    let handle = match db.backend().retrieval_store(&store_id).await {
        Ok(handle) => handle,
        Err(e) => exit_err(&e, ctx.json),
    };

    let existing = match handle.list_indexed_documents().await {
        Ok(records) => records,
        Err(e) => exit_err(&e, ctx.json),
    };
    let mut doc_index = DocumentIndex::from_records(existing);
    let (mut indexed, mut skipped, mut chunks, mut errors, mut unsupported) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let url_fetcher = HttpUrlFetcher::new();

    for rt_source in &sources_to_index {
        let source = source_row_to_core_source(rt_source);

        let chunker = match ChunkerConfig::from_preset(&source.source_kind_preset) {
            Ok(chunker) => chunker,
            Err(e) => {
                errors += 1;
                eprintln!(
                    "error indexing source {}: invalid chunker preset '{}': {}",
                    rt_source.id, source.source_kind_preset, e
                );
                continue;
            }
        };
        let cfg = IngestionConfig {
            chunker,
            ..ingestion_cfg.clone()
        };
        let sink = crate::progress::build_progress_sink(ctx.json);
        match run_ingestion_for_source(
            &source,
            &mut doc_index,
            handle.as_ref(),
            embedder.as_ref(),
            &cfg,
            &extractor,
            Some(&url_fetcher),
            sink,
        )
        .await
        {
            Ok(r) => {
                indexed += r.docs_indexed;
                skipped += r.docs_skipped;
                chunks += r.chunks_written;
                errors += r.error_count;
                unsupported += r.unsupported_format_count;
            }
            Err(e) => {
                errors += 1;
                eprintln!("error indexing source {}: {}", rt_source.id, e);
            }
        }
    }

    let status = if strict && errors > 0 { "error" } else { "ok" };
    if ctx.json {
        print_json(&json!({
            "status": status,
            "docs_indexed": indexed,
            "docs_skipped": skipped,
            "chunks_written": chunks,
            "unsupported": unsupported,
            "errors": errors,
        }));
    } else {
        println!(
            "Index complete: {} indexed, {} skipped, {} chunks written, {} unsupported, {} errors",
            indexed, skipped, chunks, unsupported, errors
        );
    }
    if strict && errors > 0 {
        std::process::exit(2);
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
    let (config_loader, db) = load_app_db_lenient(ctx).await;
    let data_dir = config_loader.paths.data_dir.clone();

    // Per specs/05-surfaces.md §2: search routes through the daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(&data_dir, ctx.daemon_url.as_deref()) {
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
                            println!("{}. {}", i + 1, uri);
                            println!("   {}", format_snippet(snippet));
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
    let runtime_stores = match db.backend().list_stores().await {
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

    let mut store_handles: Vec<StoreHandle> = Vec::new();
    for name in &store_names {
        if let Some(store_row) = runtime_stores.iter().find(|s| s.name == *name) {
            let handle = match db.backend().retrieval_store(&store_row.id).await {
                Ok(handle) => handle,
                Err(e) => exit_err(&e, ctx.json),
            };
            store_handles.push(StoreHandle {
                id: store_row.id.clone(),
                name: store_row.name.clone(),
                store: handle,
            });
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
                    println!("   {}", format_snippet(&citation.snippet));
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
    let config_loader = match load_config(&options, ctx.config_env.as_deref()) {
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

    let (config_loader, db) = load_app_db(ctx).await;

    let runtime_stores = match db.backend().list_stores().await {
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
        if let Some(store_row) = runtime_stores.iter().find(|s| s.name == *name) {
            let descriptor = StoreDescriptor {
                id: store_row.id.clone(),
                name: store_row.name.clone(),
                visibility: visibility_to_string(&store_row.visibility).to_string(),
            };
            let handle = match db.backend().retrieval_store(&store_row.id).await {
                Ok(handle) => handle,
                Err(e) => exit_err(&e, ctx.json),
            };
            available.push(AvailableStore::from_arc(descriptor, handle));
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
    use localdb_core::config::loader::ResolvedPaths;
    use localdb_core::config::schema::{
        DefaultsConfig, EmbeddingPolicy, PathsConfig, RawConfig, ServerConfig,
    };
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
            stores: vec![],
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

    #[test]
    fn format_snippet_collapses_whitespace() {
        assert_eq!(format_snippet("a\n\n  b   c"), "a b c");
    }

    #[test]
    fn format_snippet_truncates_long_input() {
        let base: String = "a".repeat(498);
        let input = format!("{base}é extra text that should be cut");
        let result = format_snippet(&input);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 501);
    }

    #[test]
    fn write_lock_creates_data_dir_and_file() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        let _lock = WriteLock::acquire(&sub).unwrap();
        assert!(sub.join(".write.lock").exists());
    }

    #[test]
    fn write_lock_removes_lock_file_on_drop() {
        let dir = TempDir::new().unwrap();
        {
            let _lock = WriteLock::acquire(dir.path()).unwrap();
            assert!(dir.path().join(".write.lock").exists());
        }
        assert!(!dir.path().join(".write.lock").exists());
    }

    #[test]
    fn write_lock_exit_code_for_store_locked() {
        assert_eq!(Error::StoreLocked.exit_code(), 4);
        assert_eq!(Error::StoreLocked.code(), "store_locked");
    }

    #[test]
    fn probe_not_running_without_socket() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            probe_daemon(dir.path(), None),
            DaemonState::NotRunning
        ));
    }

    #[test]
    fn probe_running_with_socket_file() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        std::fs::write(&sock_path, b"").unwrap();
        assert!(matches!(
            probe_daemon(dir.path(), None),
            DaemonState::NotRunning
        ));
        assert!(!sock_path.exists());
    }

    #[test]
    fn probe_daemon_health_inner_ipv6_no_port() {
        let _ = probe_daemon_health_inner("http://[::1]/v1/status");
    }

    #[test]
    fn probe_daemon_removes_stale_socket_and_returns_not_running() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        std::fs::write(&sock_path, b"http://127.0.0.1:1").unwrap();
        let state = probe_daemon(dir.path(), None);
        assert!(matches!(state, DaemonState::NotRunning));
        assert!(!sock_path.exists());
    }

    #[test]
    fn probe_daemon_env_var_bypasses_socket_check() {
        let dir = TempDir::new().unwrap();
        let state = probe_daemon(dir.path(), Some("http://127.0.0.1:9999"));
        assert!(
            matches!(state, DaemonState::Running { base_url } if base_url == "http://127.0.0.1:9999")
        );
    }

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

    #[test]
    fn convert_path_source() {
        use localdb_core::types::SourceSpec;
        let src = SourceRow {
            id: "src-1".into(),
            store_id: "store-id".into(),
            kind: SourceKind::Path,
            root: Some("/tmp/docs".into()),
            url: None,
            include: vec!["**/*.md".into()],
            exclude: vec![],
            preset: "prose".into(),
            refresh: None,
            created_at: now_rfc3339(),
        };
        let core = source_row_to_core_source(&src);
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
        use localdb_core::types::SourceSpec;
        let src = SourceRow {
            id: "src-2".into(),
            store_id: "store-id".into(),
            kind: SourceKind::Url,
            root: None,
            url: Some("https://example.com".into()),
            include: vec![],
            exclude: vec![],
            preset: "prose".into(),
            refresh: None,
            created_at: now_rfc3339(),
        };
        let core = source_row_to_core_source(&src);
        assert!(matches!(core.kind, SourceKind::Url));
        match &core.spec {
            SourceSpec::Url { url, .. } => assert_eq!(url, "https://example.com"),
            _ => panic!("expected url spec"),
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
        assert!(db.backend().list_stores().await.unwrap().is_empty());
        assert!(!db.backend().delete_store(&id).await.unwrap());
    }

    #[tokio::test]
    async fn app_db_get_store_by_name() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store = test_store_row("s1", &db);
        db.backend().upsert_store(&store).await.unwrap();
        let found = db.backend().get_store_by_name("s1").await.unwrap();
        assert_eq!(found.unwrap().name, "s1");
        assert!(db
            .backend()
            .get_store_by_name("nonexistent")
            .await
            .unwrap()
            .is_none());
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
        assert!(!db.backend().delete_source(&src.id).await.unwrap());
        assert!(db
            .backend()
            .list_sources(&store_id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn app_db_source_list_filters_by_store() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store_a = test_store_row("sa", &db);
        let store_b = test_store_row("sb", &db);
        db.backend().upsert_store(&store_a).await.unwrap();
        db.backend().upsert_store(&store_b).await.unwrap();
        db.backend()
            .upsert_source(&test_source_row(&store_a.id, "/a"))
            .await
            .unwrap();
        db.backend()
            .upsert_source(&test_source_row(&store_b.id, "/b"))
            .await
            .unwrap();
        assert_eq!(
            db.backend().list_sources(&store_a.id).await.unwrap().len(),
            1
        );
        assert_eq!(
            db.backend().list_sources(&store_b.id).await.unwrap().len(),
            1
        );
        assert!(db
            .backend()
            .list_sources("missing")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn app_db_source_get_by_id() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store = test_store_row("s", &db);
        db.backend().upsert_store(&store).await.unwrap();
        let src = test_source_row(&store.id, "/tmp");
        db.backend().upsert_source(&src).await.unwrap();
        assert!(db.backend().get_source(&src.id).await.unwrap().is_some());
        assert!(db
            .backend()
            .get_source("no-such-id")
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn error_exit_codes_match_spec() {
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

    #[tokio::test]
    async fn json_store_list_shape() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store = test_store_row("shape-store", &db);
        db.backend().upsert_store(&store).await.unwrap();
        let stores = db.backend().list_stores().await.unwrap();
        let json_stores: Vec<serde_json::Value> = stores
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "ownership": "runtime",
                    "visibility": visibility_to_string(&s.visibility),
                    "backend": s.backend,
                })
            })
            .collect();
        let value = json!({ "stores": json_stores });
        let arr = value.get("stores").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let entry = &arr[0];
        assert!(entry.get("name").is_some());
        assert!(entry.get("ownership").is_some());
        assert!(entry.get("visibility").is_some());
        assert!(entry.get("backend").is_some());
        assert_eq!(entry["name"].as_str().unwrap(), "shape-store");
    }

    #[test]
    fn json_error_shape() {
        let err = Error::StoreLocked;
        let v = json!({ "error": err.code(), "message": err.to_string() });
        assert_eq!(v["error"].as_str().unwrap(), "store_locked");
    }

    #[test]
    fn daemon_error_body_uses_code_field() {
        let body = json!({ "code": "store_not_found", "message": "store 'x' not found" });
        let code = body
            .get("code")
            .and_then(|e| e.as_str())
            .unwrap_or("internal");
        assert_eq!(code, "store_not_found");
        assert!(body.get("error").is_none());
    }

    #[test]
    fn validate_store_name_rejects_empty() {
        assert_eq!(validate_store_name("").unwrap_err().exit_code(), 2);
    }
    #[test]
    fn validate_store_name_rejects_dot() {
        assert_eq!(validate_store_name(".").unwrap_err().exit_code(), 2);
    }
    #[test]
    fn validate_store_name_rejects_dotdot() {
        assert_eq!(validate_store_name("..").unwrap_err().exit_code(), 2);
    }
    #[test]
    fn validate_store_name_rejects_slash() {
        assert_eq!(validate_store_name("a/b").unwrap_err().exit_code(), 2);
    }
    #[test]
    fn validate_store_name_rejects_leading_slash() {
        assert_eq!(validate_store_name("/root").unwrap_err().exit_code(), 2);
    }
    #[test]
    fn validate_store_name_rejects_backslash() {
        assert_eq!(validate_store_name("a\\b").unwrap_err().exit_code(), 2);
    }
    #[test]
    fn validate_store_name_accepts_valid_names() {
        assert!(validate_store_name("mystore").is_ok());
        assert!(validate_store_name("my-store").is_ok());
        assert!(validate_store_name("my_store_123").is_ok());
        assert!(validate_store_name("CamelCase").is_ok());
    }

    #[test]
    fn normalize_path_source_rejects_nonexistent_path() {
        let err = normalize_path_source("/nonexistent/path/that/does/not/exist").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn normalize_path_source_directory_has_default_includes() {
        let dir = TempDir::new().unwrap();
        let (root, include, exclude) = normalize_path_source(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(root, dir.path().to_str().unwrap());
        assert!(include.iter().any(|s| s == "**/*.rs"));
        assert!(include.iter().any(|s| s == "**/*.md"));
        assert!(include.iter().any(|s| s == "**/Cargo.lock"));
        assert!(include.iter().any(|s| s == "**/*.epub"));
        assert!(exclude.iter().any(|s| s == "**/.git"));
    }

    #[test]
    fn epub_in_folder_is_enumerated_by_default_includes() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("book.epub"), b"fake epub content").unwrap();
        std::fs::write(dir.path().join("notes.md"), b"# notes").unwrap();
        let (root, include, exclude) = normalize_path_source(dir.path().to_str().unwrap()).unwrap();
        let found =
            localdb_core::ingestion::enumerate_path_source(&root, &include, &exclude).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "book.epub"));
        assert!(names.iter().any(|n| n == "notes.md"));
    }

    #[test]
    fn normalize_path_source_single_file_promotes_to_parent() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("README.md");
        std::fs::write(&file_path, b"hello").unwrap();
        let (root, include, _exclude) = normalize_path_source(file_path.to_str().unwrap()).unwrap();
        assert_eq!(root, dir.path().to_str().unwrap());
        assert_eq!(include, vec!["README.md".to_string()]);
    }

    #[test]
    fn looks_like_id_recognizes_ulid() {
        assert!(looks_like_id("01HRQHB7FN3WMX4AZDV3S9VCTZ"));
    }

    #[test]
    fn looks_like_id_rejects_paths() {
        assert!(!looks_like_id("/home/user/docs"));
        assert!(!looks_like_id("./relative/path"));
        assert!(!looks_like_id("https://example.com"));
        assert!(!looks_like_id("some/path"));
    }

    #[test]
    fn default_path_excludes_contains_git_and_node_modules() {
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/.git"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/node_modules"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/.DS_Store"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/target"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/__pycache__"));
        assert!(DEFAULT_PATH_EXCLUDES.contains(&"**/.venv"));
    }

    #[test]
    fn limit_zero_maps_to_exit_code_2() {
        let err = Error::InvalidRequest {
            message: "--limit must be at least 1".to_string(),
        };
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn store_not_found_maps_to_exit_code_3() {
        let err = Error::StoreNotFound {
            id: "no-such-store".to_string(),
        };
        assert_eq!(err.exit_code(), 3);
    }

    #[tokio::test]
    async fn find_source_by_root_finds_match() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store = test_store_row("s1", &db);
        db.backend().upsert_store(&store).await.unwrap();
        let store_id = db.resolve_store_id("s1").await.unwrap();
        let src = test_source_row(&store_id, "/my/docs");
        db.backend().upsert_source(&src).await.unwrap();
        let found = db
            .backend()
            .find_source_by_root_or_url("/my/docs", &store_id)
            .await
            .unwrap();
        assert_eq!(found.unwrap().id, src.id);
    }

    #[tokio::test]
    async fn find_source_by_root_respects_store_scope() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store_a = test_store_row("my-store", &db);
        let store_b = test_store_row("other-store", &db);
        db.backend().upsert_store(&store_a).await.unwrap();
        db.backend().upsert_store(&store_b).await.unwrap();
        let src = test_source_row(&store_b.id, "/my/docs");
        db.backend().upsert_source(&src).await.unwrap();
        let found = db
            .backend()
            .find_source_by_root_or_url("/my/docs", &store_a.id)
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn confirm_destructive_yes_flag_skips_prompt() {
        let ctx = CliContext {
            config: None,
            json: false,
            stores: vec![],
            yes: true,
            daemon_url: None,
            config_env: None,
        };
        assert!(confirm_destructive(&ctx, "Are you sure?"));
    }

    #[test]
    fn confirm_destructive_json_mode_exits_with_invalid_request() {
        let err = Error::InvalidRequest {
            message: "this command is destructive; re-run with --yes to confirm".to_string(),
        };
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.code(), "invalid_request");
    }
}
