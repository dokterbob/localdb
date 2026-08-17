use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::Request,
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, post},
    Router,
};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use localdb_core::{
    config::{loader::ResolvedPaths, schema::RawConfig},
    Embedder, Error,
};

use crate::{
    handlers,
    job_queue::JobQueue,
    mcp_bridge,
    scheduler::UrlRefreshScheduler,
    socket::{SocketGuard, UrlFileGuard},
    state::AppState,
};

/// Options for starting the daemon.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub paths: ResolvedPaths,
    /// The loaded YAML config.
    pub config: RawConfig,
}

/// A running daemon instance.
///
pub struct DaemonHandle {
    /// The socket guard (cleans up socket file on drop).
    pub _socket: SocketGuard,
    /// The discovery URL file guard (cleans up `daemon.url` on drop).
    pub _url_file: UrlFileGuard,
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
/// 1. Bind the Unix discovery socket (fails fast if another daemon is running).
/// 2. Bind the TCP listener at the configured `server.bind`/`server.port`. Any
///    bind address is accepted (specs/05-surfaces.md §3); binding to all
///    interfaces logs a warning since the daemon has no authentication.
/// 3. Record the daemon's client-reachable base URL in `daemon.url` so CLI/MCP
///    discovery finds it regardless of the configured bind address or port.
pub async fn start_daemon(
    options: DaemonOptions,
) -> Result<(DaemonHandle, impl std::future::Future<Output = ()>), Error> {
    let bind_addr = options.config.server.bind.as_str();
    let port = options.config.server.port;
    let socket_guard = bind_socket_guard(&options)?;
    let (state, url_scheduler) = build_daemon_state(&options).await?;
    let (mcp_stores, mcp_embedder) = mcp_bridge::build_available_stores(&state).await?;
    // Bind first so `mcp_allowed_hosts` sees the actually-bound address
    // (wildcard aliases like `"0"`/`"[::]"` only resolve to a concrete
    // `SocketAddr` after binding — same reasoning as `warn_if_unspecified`
    // and `client_base_url`, both of which also key off `bound_addr` rather
    // than the raw config string). `build_available_stores`'s embedder is a
    // `LazyEmbedder` and doesn't block on model loading, so reordering the
    // (cheap) router construction after the bind doesn't delay startup.
    let (listener, bound_addr) = bind_tcp_listener(bind_addr, port).await?;
    warn_if_unspecified(bound_addr);
    let router = build_router(
        state.clone(),
        mcp_stores,
        mcp_embedder,
        mcp_allowed_hosts(bound_addr),
    );
    let url_file_guard =
        UrlFileGuard::new(&options.paths.url_path(), &client_base_url(bound_addr))?;

    spawn_config_watcher(options.paths.config_file.clone(), state.clone());
    spawn_url_scheduler(&state, url_scheduler);

    let handle = DaemonHandle {
        _socket: socket_guard,
        _url_file: url_file_guard,
        addr: bound_addr,
    };

    Ok((handle, server_future(listener, router)))
}

fn bind_socket_guard(options: &DaemonOptions) -> Result<SocketGuard, Error> {
    SocketGuard::new(&options.paths.socket_path())
}

async fn build_daemon_state(
    options: &DaemonOptions,
) -> Result<(AppState, UrlRefreshScheduler), Error> {
    let queue = JobQueue::with_workers(options.config.server.job_workers);
    let url_scheduler = UrlRefreshScheduler::new(queue.clone());
    let state = AppState::new(
        options.config.clone(),
        options.paths.data_dir.clone(),
        options.paths.models_dir.clone(),
        queue.clone(),
        url_scheduler.clone(),
    )
    .await?;
    // `AppState::new` above requires an already-built `UrlRefreshScheduler`
    // (so sources can register with it), which is why this can't happen at
    // `UrlRefreshScheduler::new` time — see that field's doc comment.
    // Without this, `tick()`'s submitted jobs would fail every time with
    // "no state attached" instead of running real ingestion (issue #187).
    url_scheduler.attach_state(state.clone()).await;

    Ok((state, url_scheduler))
}

async fn bind_tcp_listener(bind_addr: &str, port: u16) -> Result<(TcpListener, SocketAddr), Error> {
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

    Ok((listener, bound_addr))
}

fn spawn_config_watcher(config_file_path: PathBuf, state: AppState) {
    tokio::spawn(async move {
        let result = run_config_watcher(config_file_path, state).await;
        if let Err(e) = result {
            error!("config watcher failed: {}", e);
        }
    });
}

fn spawn_url_scheduler(state: &AppState, url_scheduler: UrlRefreshScheduler) {
    let backend_for_url = state.backend_arc();
    let sched_for_url = url_scheduler.clone();
    tokio::spawn(async move {
        let stores = match backend_for_url.list_stores().await {
            Ok(s) => s,
            Err(e) => {
                error!("URL scheduler: cannot list stores: {e}");
                return;
            }
        };
        for store in stores {
            let sources = match backend_for_url.list_sources(&store.id).await {
                Ok(s) => s,
                Err(e) => {
                    error!(
                        "URL scheduler: cannot list sources for '{}': {e}",
                        store.name
                    );
                    continue;
                }
            };
            for source in sources {
                if source.kind == localdb_core::types::SourceKind::Url {
                    if let Some(url) = source.url {
                        let interval_secs =
                            source.refresh.as_deref().and_then(parse_refresh_interval);
                        sched_for_url
                            .register(source.id, store.name.clone(), url, interval_secs)
                            .await;
                    }
                }
            }
        }
    });
    tokio::spawn(url_scheduler.run(std::time::Duration::from_secs(60)));
}

async fn server_future(listener: TcpListener, router: Router) {
    if let Err(e) = axum::serve(listener, router).await {
        error!("server error: {}", e);
    }
}

/// Build the axum router with all /v1 routes plus the `/mcp` MCP-over-HTTP
/// route.
///
/// Routes per specs/05-surfaces.md §3:
///   GET/POST /stores, GET/PATCH/DELETE /stores/{id},
///   GET/POST /stores/{id}/sources, DELETE /sources/{id},
///   GET /stores/{id}/documents, GET /documents/{id}, POST /search,
///   GET/POST /jobs, GET/DELETE /jobs/{id}, GET /jobs/{id}/events, GET /status,
///   GET /config.
///
/// `mcp_stores`/`mcp_embedder` are the startup-time snapshot built by
/// `mcp_bridge::build_available_stores` (specs/05-surfaces.md §4) — see
/// that function's doc comment for why `/mcp` doesn't see stores added
/// later via `/v1/stores` without a restart. `nest_service` (rather than
/// `route_service`) matches the mount pattern rmcp's own test suite uses
/// for `StreamableHttpService` and composes fine with a `Router<AppState>`
/// that also has `.with_state` routes: the mounted service handles
/// `Request` directly and needs no state extraction.
pub fn build_router(
    state: AppState,
    mcp_stores: Vec<mcp::AvailableStore>,
    mcp_embedder: Arc<dyn Embedder>,
    mcp_allowed_hosts: Vec<String>,
) -> Router {
    Router::new()
        .route(
            "/v1/stores",
            get(handlers::list_stores).post(handlers::create_store),
        )
        .route(
            "/v1/stores/{name}",
            get(handlers::get_store)
                .patch(handlers::patch_store)
                .delete(handlers::delete_store),
        )
        .route(
            "/v1/stores/{name}/sources",
            get(handlers::list_sources).post(handlers::create_source),
        )
        .route("/v1/sources/{id}", delete(handlers::delete_source))
        .route("/v1/stores/{name}/documents", get(handlers::list_documents))
        .route("/v1/documents/{id}", get(handlers::get_document))
        .route("/v1/search", post(handlers::search))
        .route(
            "/v1/jobs",
            get(handlers::list_jobs).post(handlers::create_job),
        )
        .route(
            "/v1/jobs/{id}",
            get(handlers::get_job).delete(handlers::cancel_job),
        )
        .route("/v1/jobs/{id}/events", get(handlers::job_events))
        .route("/v1/status", get(handlers::get_status))
        .route("/v1/config", get(handlers::get_config))
        .with_state(state)
        .nest_service(
            "/mcp",
            mcp::build_streamable_http_service(mcp_stores, mcp_embedder, mcp_allowed_hosts),
        )
        // Applied *after* `nest_service` so this layer wraps the whole
        // composed router — including the nested rmcp `/mcp` mount, whose
        // own rejections (e.g. the Host-header DNS-rebinding check; see
        // `mcp_allowed_hosts`) would otherwise never reach a log at all
        // (issue #147). A layer added before `nest_service` would only wrap
        // the routes already present at that point.
        .layer(middleware::from_fn(log_rejected_responses))
}

/// Log any response with status >= 400 at `warn`, so a rejected request —
/// whether from `/v1` or the nested `/mcp` mount — leaves a trace instead of
/// silently returning a bare 4xx/5xx with nothing in `localdb serve`'s
/// output to diagnose it (issue #147). Deliberately hand-rolled rather than
/// pulling in `tower-http`'s trace layer: this is the one thing we need
/// (method, path, status, `Host`) and the project avoids adding a dependency
/// for it.
///
/// The `Host` header is logged (rather than peer address) because it's what
/// actually drives rejection in the one case this was hardest to diagnose
/// without it — rmcp's DNS-rebinding check on `/mcp` (see
/// `mcp_allowed_hosts`'s doc comment) — and it's already present on every
/// HTTP request at no extra cost, unlike the TCP peer address, which axum
/// does not expose to middleware without opting into `into_make_service_with_connect_info`.
async fn log_rejected_responses(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let response = next.run(request).await;

    let status = response.status();
    if status.as_u16() >= 400 {
        warn!(
            method = %method,
            path = %path,
            status = status.as_u16(),
            host = %host,
            "rejected request"
        );
    }

    response
}

/// Warn when the actually-bound address is unspecified (all interfaces).
///
/// Per specs/05-surfaces.md §3: the daemon has no authentication, so binding to
/// all interfaces makes it reachable from any network the machine is on. Binding
/// to a specific non-loopback address (e.g. a LAN/VPN IP) is treated as a
/// deliberate trust decision and doesn't warn.
///
/// This checks the address the OS actually bound (`SocketAddr::ip().is_unspecified()`)
/// rather than the raw config string, so wildcard aliases the string form can't see —
/// `"0"`, `"[::]"`, `"000.000.000.000"` — are still caught.
fn warn_if_unspecified(bound_addr: SocketAddr) {
    if bound_addr.ip().is_unspecified() {
        warn!(
            bind = %bound_addr.ip(),
            "binding to all interfaces ({}); the daemon has no authentication and will be \
             reachable from any network this machine is on",
            bound_addr.ip()
        );
    }
}

/// Host allowlist for rmcp's DNS-rebinding `Host`-header check on the `/mcp`
/// route, derived from the daemon's own already-accepted bind-address trust
/// decision (specs/05-surfaces.md §3, PR #135) rather than rmcp's
/// independent localhost-only default — otherwise a deliberately-chosen
/// non-loopback bind (e.g. a Tailscale/LAN IP) works for every other route
/// but rmcp still 403s `/mcp` with "Host header is not allowed", which MCP
/// clients like Claude Code surface as a spurious "needs authentication".
///
/// Checks the actually-bound `SocketAddr` (see `bind_tcp_listener`), not the
/// raw config string, for the same reason `warn_if_unspecified` and
/// `client_base_url` do: wildcard aliases (`"0"`, `"[::]"`) only resolve to
/// a concrete unspecified address once actually bound.
fn mcp_allowed_hosts(bound_addr: SocketAddr) -> Vec<String> {
    if bound_addr.ip().is_unspecified() {
        // Wildcard bind: `warn_if_unspecified` already warns this is
        // reachable from any network and accepts connections from anywhere.
        // There's no single external IP to allow-list ahead of time (it
        // could be any interface on the machine), and layering an
        // incomplete Host check on top of an already-fully-open bind adds
        // inconsistency, not security. Empty means "disabled" — see
        // `mcp::build_streamable_http_service`'s doc comment.
        return Vec::new();
    }
    // `with_allowed_hosts` *replaces* rmcp's default list rather than
    // extending it, so the localhost defaults must be included explicitly
    // alongside the bind address — otherwise local access (e.g. `localdb
    // mcp` proxying to a daemon bound to a LAN IP, or a human curling it
    // locally) would break.
    vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        bound_addr.ip().to_string(),
    ]
}

/// The daemon's client-reachable base URL for a bound address.
///
/// An unspecified (wildcard) bind such as `0.0.0.0` or `::` isn't itself a
/// connectable address — substitute the loopback address for the same family so
/// CLI/MCP discovery (which runs on the same machine) can always reach it.
/// Any other bound address is used as-is (IPv6 hosts are bracketed by
/// `SocketAddr`'s `Display` impl).
fn client_base_url(bound_addr: SocketAddr) -> String {
    let port = bound_addr.port();
    if bound_addr.ip().is_unspecified() {
        if bound_addr.is_ipv6() {
            format!("http://[::1]:{port}")
        } else {
            format!("http://127.0.0.1:{port}")
        }
    } else {
        format!("http://{bound_addr}")
    }
}

/// Watch the config file for changes and reload the YAML config snapshot.
///
/// Non-fatal: logs errors but does not stop the daemon.
async fn run_config_watcher(config_file: PathBuf, state: AppState) -> Result<(), Error> {
    let parent = config_file.parent().ok_or_else(|| Error::InvalidConfig {
        message: "config file has no parent directory".to_string(),
    })?;

    let (mut rx, _handle) =
        crate::watcher::watch_path(parent, 300).map_err(|e| Error::Internal {
            message: format!(
                "cannot start config watcher for '{}': {e}",
                config_file.display()
            ),
            correlation_id: "daemon_config_reload".into(),
        })?;

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

/// Parse a human-readable refresh interval string (e.g. "24h", "30m", "3600s") to seconds.
///
/// Returns `None` if the string is unparseable, empty, or would overflow `u64`.
/// Uses checked arithmetic to guard against integer overflow for very large values.
pub fn parse_refresh_interval(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(h) = s.strip_suffix('h') {
        h.parse::<u64>().ok().and_then(|n| n.checked_mul(3600))
    } else if let Some(m) = s.strip_suffix('m') {
        m.parse::<u64>().ok().and_then(|n| n.checked_mul(60))
    } else if let Some(sec) = s.strip_suffix('s') {
        sec.parse::<u64>().ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// Read, parse **and validate** the config file.
///
/// `load_config_from_str`, not a bare `serde_yaml::from_str`: hot-reload has
/// to apply the same validation the startup path does, or a value that is
/// syntactically fine but semantically rejected (`http.rate_limit.burst: 0`,
/// an `http.user_agent` that is not a legal header value) enters a running
/// daemon through the file watcher and fails later, opaquely, at the point of
/// use — which is precisely what validating at load time exists to prevent.
fn reload_config_file(path: &Path) -> Result<RawConfig, Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| Error::Internal {
        message: format!("cannot read config file '{}': {e}", path.display()),
        correlation_id: "daemon_config_reload".into(),
    })?;
    localdb_core::config::load_config_from_str(&contents).map_err(|e| Error::Internal {
        message: format!("cannot load config file '{}': {e}", path.display()),
        correlation_id: "daemon_config_reload".into(),
    })
}

#[cfg(test)]
mod tests;
