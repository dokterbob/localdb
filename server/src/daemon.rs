//! Daemon startup: acquire lock, bind socket, start HTTP server.
//!
//! Entry point for `localdb serve`. Validates the bind address (loopback-only
//! by default), acquires the write lock, binds the unix socket for discovery,
//! starts the axum HTTP server, sets up file watchers, and spawns URL refresh
//! schedulers.
//!
//! See specs/05-surfaces.md §3 and specs/01-architecture.md §3.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::{
    routing::{delete, get, post},
    Router,
};
use tokio::net::TcpListener;
use tracing::{error, info};

use localdb_core::{
    config::{loader::ResolvedPaths, schema::RawConfig},
    Error,
};

use crate::{handlers, job_queue::JobQueue, lock::WriteLock, socket::SocketGuard, state::AppState};

/// Options for starting the daemon.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Resolved paths (data dir, socket, lock, etc.).
    pub paths: ResolvedPaths,
    /// The loaded YAML config.
    pub config: RawConfig,
}

/// A running daemon instance.
///
/// Holds the write lock and socket guard for their lifetimes.
/// Stopping this struct (or dropping it) releases both.
pub struct DaemonHandle {
    /// The write lock (released on drop).
    pub _lock: WriteLock,
    /// The socket guard (cleans up socket file on drop).
    pub _socket: SocketGuard,
    /// The bind address.
    pub addr: SocketAddr,
}

impl std::fmt::Debug for DaemonHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DaemonHandle({})", self.addr)
    }
}

/// Start the daemon.
///
/// Steps:
/// 1. Validate bind address: refuse non-loopback without auth.
/// 2. Acquire the write lock; error `DaemonRunning` if already held.
/// 3. Create the unix socket file for discovery.
/// 4. Build the axum router with all routes.
/// 5. Bind TCP and return the handle; the caller awaits the server future.
pub async fn start_daemon(
    options: DaemonOptions,
) -> Result<(DaemonHandle, impl std::future::Future<Output = ()>), Error> {
    let bind_addr = options.config.server.bind.as_str();
    let port = options.config.server.port;

    // Guard: refuse non-loopback bind without auth.
    validate_bind_address(bind_addr)?;

    // Acquire write lock.
    let lock = WriteLock::try_acquire(&options.paths.write_lock_path())?;
    info!(
        "write lock acquired: {}",
        options.paths.write_lock_path().display()
    );

    // Create socket guard (we write the socket file after binding).
    let socket_guard = SocketGuard::new(&options.paths.socket_path());

    // Build shared application state.
    let queue = JobQueue::new();
    let state = AppState::new(
        options.config.clone(),
        options.paths.data_dir.clone(),
        queue,
    )?;

    // Build router.
    let router = build_router(state.clone());

    // Bind TCP listener.
    let addr_str = format!("{}:{}", bind_addr, port);
    let listener = TcpListener::bind(&addr_str)
        .await
        .map_err(|e| Error::Internal {
            message: format!("cannot bind to {}: {}", addr_str, e),
            correlation_id: "daemon_bind".to_string(),
        })?;

    let bound_addr = listener.local_addr().map_err(|e| Error::Internal {
        message: format!("cannot get local addr: {}", e),
        correlation_id: "daemon_local_addr".to_string(),
    })?;

    info!("daemon listening on {}", bound_addr);

    // Spawn config watcher (non-fatal if it fails to start).
    let config_file_path = options.paths.config_file.clone();
    let state_for_watcher = state.clone();
    tokio::spawn(async move {
        let result = run_config_watcher(config_file_path, state_for_watcher).await;
        if let Err(e) = result {
            error!("config watcher failed: {}", e);
        }
    });

    // Create the server future.
    let server_future = async move {
        if let Err(e) = axum::serve(listener, router).await {
            error!("server error: {}", e);
        }
    };

    let handle = DaemonHandle {
        _lock: lock,
        _socket: socket_guard,
        addr: bound_addr,
    };

    Ok((handle, server_future))
}

/// Build the axum router with all /v1 routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/v1/stores",
            get(handlers::list_stores).post(handlers::create_store),
        )
        .route(
            "/v1/stores/{name}",
            get(handlers::get_store).delete(handlers::delete_store),
        )
        .route(
            "/v1/stores/{name}/sources",
            get(handlers::list_sources).post(handlers::create_source),
        )
        .route("/v1/sources/{id}", delete(handlers::delete_source))
        .route("/v1/search", post(handlers::search))
        .route("/v1/jobs", post(handlers::create_job))
        .route("/v1/jobs/{id}", get(handlers::get_job))
        .route("/v1/status", get(handlers::get_status))
        .route("/v1/config", get(handlers::get_config))
        .with_state(state)
}

/// Validate the bind address.
///
/// Per specs/05-surfaces.md §3: "Binding to a non-loopback address without auth
/// configured is a refused startup, not a warning."
///
/// MVP: any non-loopback address is refused.
pub fn validate_bind_address(bind: &str) -> Result<(), Error> {
    // Accept loopback addresses: 127.x.x.x or ::1 or localhost
    let is_loopback =
        bind == "127.0.0.1" || bind == "::1" || bind == "localhost" || bind.starts_with("127.");

    if !is_loopback {
        return Err(Error::InvalidConfig {
            message: format!(
                "refusing to bind to non-loopback address '{}' without auth configured; \
                 use 127.0.0.1 for local-only mode (the default). \
                 Non-loopback binding requires auth (roadmap feature).",
                bind
            ),
        });
    }

    Ok(())
}

/// Watch the config file for changes and reload the YAML config snapshot.
///
/// Non-fatal: logs errors but does not stop the daemon.
async fn run_config_watcher(
    config_file: PathBuf,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let parent = config_file
        .parent()
        .ok_or("config file has no parent directory")?;

    let (mut rx, _handle) = crate::watcher::watch_path(parent, 300)?;

    info!("config watcher started for: {}", config_file.display());

    while let Some(event) = rx.recv().await {
        if event.path == config_file {
            info!("config file changed, reloading: {}", config_file.display());
            match reload_config_file(&config_file) {
                Ok(new_config) => {
                    state.reload_yaml_config(new_config).await;
                    info!("config reloaded successfully");
                }
                Err(e) => {
                    error!("config reload failed: {}", e);
                }
            }
        }
    }

    Ok(())
}

/// Read and parse the config file.
fn reload_config_file(path: &Path) -> Result<RawConfig, Box<dyn std::error::Error + Send + Sync>> {
    let contents = std::fs::read_to_string(path)?;
    let config: RawConfig = serde_yaml::from_str(&contents)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_resolved_paths(dir: &Path) -> ResolvedPaths {
        ResolvedPaths {
            config_file: dir.join("config.yaml"),
            data_dir: dir.join("data"),
            models_dir: dir.join("models"),
            logs_dir: dir.join("logs"),
        }
    }

    // --- validate_bind_address ---

    #[test]
    fn loopback_addresses_are_accepted() {
        assert!(validate_bind_address("127.0.0.1").is_ok());
        assert!(validate_bind_address("::1").is_ok());
        assert!(validate_bind_address("localhost").is_ok());
        assert!(validate_bind_address("127.0.0.2").is_ok());
    }

    #[test]
    fn non_loopback_addresses_are_rejected() {
        let result = validate_bind_address("0.0.0.0");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "invalid_config");

        let result = validate_bind_address("192.168.1.1");
        assert!(result.is_err());

        let result = validate_bind_address("0.0.0.0");
        assert!(result.is_err());
    }

    #[test]
    fn non_loopback_error_message_is_descriptive() {
        let err = validate_bind_address("0.0.0.0").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("0.0.0.0") || msg.contains("non-loopback"),
            "error message should describe the problem: {}",
            msg
        );
    }

    // --- Daemon startup ---

    #[tokio::test]
    async fn daemon_starts_and_binds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig {
                bind: "127.0.0.1".to_string(),
                port: 0, // let OS assign a free port
            },
            paths: Default::default(),
            defaults: Default::default(),
            stores: vec![],
            providers: vec![],
        };

        let options = DaemonOptions {
            paths: paths.clone(),
            config,
        };

        let result = start_daemon(options).await;
        assert!(result.is_ok(), "daemon should start: {:?}", result.err());
        let (handle, _server_future) = result.unwrap();
        assert!(handle.addr.port() > 0);
    }

    #[tokio::test]
    async fn second_daemon_fails_with_store_locked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig {
                bind: "127.0.0.1".to_string(),
                port: 0, // let OS assign a free port
            },
            paths: Default::default(),
            defaults: Default::default(),
            stores: vec![],
            providers: vec![],
        };

        let options1 = DaemonOptions {
            paths: paths.clone(),
            config: config.clone(),
        };

        // Start first daemon
        let result1 = start_daemon(options1).await;
        assert!(result1.is_ok(), "first daemon should start");
        let (_handle1, _fut1) = result1.unwrap();

        // Try to start second daemon — should fail with StoreLocked
        let options2 = DaemonOptions {
            paths: paths.clone(),
            config: config.clone(),
        };
        let result2 = start_daemon(options2).await;
        assert!(
            matches!(result2, Err(Error::StoreLocked)),
            "second daemon should fail with StoreLocked"
        );
    }

    #[tokio::test]
    async fn non_loopback_bind_refuses_startup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let mut config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig::default(),
            paths: Default::default(),
            defaults: Default::default(),
            stores: vec![],
            providers: vec![],
        };
        // Set non-loopback bind address
        config.server.bind = "0.0.0.0".to_string();

        let options = DaemonOptions { paths, config };

        let result = start_daemon(options).await;
        assert!(
            matches!(result, Err(Error::InvalidConfig { .. })),
            "non-loopback bind should fail with InvalidConfig"
        );
    }

    // --- HTTP integration via build_router ---

    #[tokio::test]
    async fn router_serves_status_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            stores: vec![],
            providers: vec![],
        };
        let queue = JobQueue::new();
        let state = AppState::new(yaml_config, dir.path().to_path_buf(), queue).unwrap();
        let app = build_router(state);

        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }
}
