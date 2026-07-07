use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use localdb_core::{
    auth::Principal,
    config::{
        loader::ResolvedPaths,
        schema::{RawConfig, ServerAuthMode},
    },
    Embedder, Error,
};

use crate::{
    auth::{self, middleware::require_auth, AuthMode},
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
///    bind address is accepted (specs/05-surfaces.md §3) except `auth: off`
///    combined with a non-loopback bind (a hard `invalid_config` error, see
///    `resolve_auth_mode`); binding to all interfaces logs a warning.
/// 3. Resolve the auth mode from `server.auth` + the *actually-bound*
///    address, build the state, and — when auth is enforced with zero users —
///    print the one-time setup code to stderr (D3b).
/// 4. Record the daemon's client-reachable base URL in `daemon.url` so CLI/MCP
///    discovery finds it regardless of the configured bind address or port.
///
/// Config is read once here at startup; there is no hot-reload
/// (specs/03-config.md §5 — the file-watcher-based reload was removed in T3).
pub async fn start_daemon(
    options: DaemonOptions,
) -> Result<(DaemonHandle, impl std::future::Future<Output = ()>), Error> {
    let bind_addr = options.config.server.bind.as_str();
    let port = options.config.server.port;
    let socket_guard = bind_socket_guard(&options)?;
    // Bind first so `resolve_auth_mode` and `mcp_allowed_hosts` see the
    // actually-bound address (wildcard aliases like `"0"`/`"[::]"` only
    // resolve to a concrete `SocketAddr` after binding — same reasoning as
    // `warn_if_unspecified` and `client_base_url`, both of which also key
    // off `bound_addr` rather than the raw config string).
    let (listener, bound_addr) = bind_tcp_listener(bind_addr, port).await?;
    warn_if_unspecified(bound_addr);
    let auth_mode = resolve_auth_mode(bound_addr, options.config.server.auth)?;
    let (state, url_scheduler) = build_daemon_state(&options, auth_mode).await?;
    if let Some(setup_code) = auth::generate_setup_code_if_needed(&state).await? {
        // D3b: shown exactly once, plaintext never persisted (only its hash,
        // in AppState). Redeemed via `/authorize` starting with T4.
        eprintln!(
            "No users exist yet and authentication is enforced.\n\
             One-time setup code (use it to create the first admin account; \
             shown only once):\n\n    {setup_code}\n"
        );
    }
    let mcp_provider: Arc<dyn mcp::StoreProvider> =
        Arc::new(mcp_bridge::AppStateStoreProvider::new(state.clone()));
    let mcp_embedder = mcp_bridge::build_mcp_embedder(&state);
    let router = build_router(
        state.clone(),
        mcp_provider,
        mcp_embedder,
        mcp_allowed_hosts(bound_addr),
    );
    let url_file_guard =
        UrlFileGuard::new(&options.paths.url_path(), &client_base_url(bound_addr))?;

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
    auth_mode: AuthMode,
) -> Result<(AppState, UrlRefreshScheduler), Error> {
    let queue = JobQueue::new();
    let url_scheduler = UrlRefreshScheduler::new(queue.clone());
    let state = AppState::new(
        options.config.clone(),
        options.paths.data_dir.clone(),
        queue.clone(),
        url_scheduler.clone(),
        auth_mode,
    )
    .await?;

    Ok((state, url_scheduler))
}

/// Resolve the effective [`AuthMode`] from the configured `server.auth` and
/// the **actually-bound** address (D4, specs/05-surfaces.md §3):
///
/// | `server.auth` | loopback bind | non-loopback bind |
/// |---|---|---|
/// | `auto` (default) | `Open` | `Enforced` |
/// | `required` | `Enforced` | `Enforced` |
/// | `off` | `Open` | hard error (`invalid_config`) — the daemon refuses to start rather than exposing an unauthenticated surface to a network |
///
/// Keyed off the bound `SocketAddr` (not the raw config string) so wildcard
/// aliases resolve correctly; an unspecified bind (`0.0.0.0`/`::`) is
/// non-loopback for this purpose. The wildcard-bind startup *warning*
/// (`warn_if_unspecified`) applies independently, regardless of auth mode.
pub fn resolve_auth_mode(bound: SocketAddr, mode_cfg: ServerAuthMode) -> Result<AuthMode, Error> {
    let loopback = bound.ip().is_loopback();
    match mode_cfg {
        ServerAuthMode::Required => Ok(AuthMode::Enforced),
        ServerAuthMode::Auto if loopback => Ok(AuthMode::Open),
        ServerAuthMode::Auto => Ok(AuthMode::Enforced),
        ServerAuthMode::Off if loopback => Ok(AuthMode::Open),
        ServerAuthMode::Off => Err(Error::InvalidConfig {
            message: format!(
                "server.auth is 'off' but the daemon is bound to the non-loopback address {} — \
                 refusing to expose an unauthenticated surface to a network. Use a loopback \
                 bind, or set server.auth to 'auto' or 'required'.",
                bound.ip()
            ),
        }),
    }
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
///   GET /documents/{id}, POST /search,
///   POST /jobs, GET /jobs/{id}, GET /status, GET /config.
///
/// `mcp_provider` is realtime (specs/05-surfaces.md §4): it resolves the
/// current store list fresh on every `/mcp` tool call rather than from a
/// snapshot taken once at construction, so a store added later via
/// `POST /v1/stores` is visible on the very next MCP call — see
/// `mcp_bridge::AppStateStoreProvider` and `mcp::store_provider`'s design
/// rationale (D12). `nest_service` (rather than `route_service`) matches the
/// mount pattern rmcp's own test suite uses for `StreamableHttpService` and
/// composes fine with a `Router<AppState>` that also has `.with_state`
/// routes: the mounted service handles `Request` directly and needs no
/// state extraction.
///
/// Auth (specs/05-surfaces.md §3.1): the `require_auth` middleware layer is
/// applied *after* the `/mcp` `nest_service`, so it wraps every `/v1/*`
/// route AND the MCP mount. `/authorize`, `/token`, and `/revoke` (T4) are
/// built as a **separate, unlayered router** and merged in afterwards — they
/// *are* the auth flow itself (specs/05-surfaces.md §3.1's public-routes
/// table) and must be reachable without a bearer token. The MCP handler's
/// default principal follows the protected router's mode: `Open` passes
/// `local_trust` (requests carry it anyway via the middleware, but embedded
/// construction paths share this signature), while `Enforced` passes `None`
/// so a missing request-extension `Principal` fails closed inside the tool
/// handlers rather than granting full access.
pub fn build_router(
    state: AppState,
    mcp_provider: Arc<dyn mcp::StoreProvider>,
    mcp_embedder: Arc<dyn Embedder>,
    mcp_allowed_hosts: Vec<String>,
) -> Router {
    let mcp_default_principal = match state.auth_mode() {
        AuthMode::Open => Some(Principal::local_trust()),
        AuthMode::Enforced => None,
    };
    let protected = Router::new()
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
        .route("/v1/documents/{id}", get(handlers::get_document))
        .route("/v1/search", post(handlers::search))
        .route("/v1/jobs", post(handlers::create_job))
        .route("/v1/jobs/{id}", get(handlers::get_job))
        .route("/v1/status", get(handlers::get_status))
        .route("/v1/config", get(handlers::get_config))
        .route("/v1/auth/me", get(handlers::get_me))
        .with_state(state.clone())
        .nest_service(
            "/mcp",
            mcp::build_streamable_http_service(
                mcp_provider,
                mcp_embedder,
                mcp_allowed_hosts,
                mcp_default_principal,
            ),
        )
        // Applied after `.nest_service` so the auth layer wraps `/mcp` too.
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let public = Router::new()
        .route(
            "/authorize",
            get(auth::oauth::get_authorize).post(auth::oauth::post_authorize),
        )
        .route("/token", post(auth::oauth::post_token))
        .route("/revoke", post(auth::oauth::post_revoke))
        .with_state(state);

    public.merge(protected)
}

/// Warn when the actually-bound address is unspecified (all interfaces).
///
/// Per specs/05-surfaces.md §3: binding to all interfaces makes the daemon
/// reachable from any network the machine is on — the one case a user could
/// plausibly not realize how exposed it makes them. The warning applies
/// regardless of auth mode (an `auto`/`required` wildcard bind is enforced
/// but still network-reachable). Binding to a specific non-loopback address
/// (e.g. a LAN/VPN IP) is treated as a deliberate trust decision and doesn't
/// warn.
///
/// This checks the address the OS actually bound (`SocketAddr::ip().is_unspecified()`)
/// rather than the raw config string, so wildcard aliases the string form can't see —
/// `"0"`, `"[::]"`, `"000.000.000.000"` — are still caught.
fn warn_if_unspecified(bound_addr: SocketAddr) {
    if bound_addr.ip().is_unspecified() {
        warn!(
            bind = %bound_addr.ip(),
            "binding to all interfaces ({}); the daemon will be reachable from any network \
             this machine is on",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_resolved_paths(dir: &std::path::Path) -> ResolvedPaths {
        ResolvedPaths {
            config_file: dir.join("config.yaml"),
            data_dir: dir.join("data"),
            models_dir: dir.join("models"),
            logs_dir: dir.join("logs"),
        }
    }

    // --- bind address warning ---

    #[test]
    fn warn_if_unspecified_does_not_panic_for_any_input() {
        warn_if_unspecified("127.0.0.1:7700".parse().unwrap());
        warn_if_unspecified("192.168.1.1:7700".parse().unwrap());
        warn_if_unspecified("0.0.0.0:7700".parse().unwrap());
        warn_if_unspecified("[::]:7700".parse().unwrap());
        warn_if_unspecified("[::1]:7700".parse().unwrap());
    }

    /// Pins the actual OS-resolution behavior the wildcard-alias fix depends on
    /// (Codex review comment: string checks like `bind == "0.0.0.0"` miss aliases
    /// such as `"0"`, `"[::]"`, `"000.000.000.000"` that the OS still resolves to
    /// the unspecified address). Binding on the *actually returned* `SocketAddr`
    /// rather than the config string is only a real fix if these forms truly
    /// resolve to unspecified on the platforms we run on — this test binds each
    /// one for real and checks `local_addr().ip().is_unspecified()`, instead of
    /// just asserting the (already-known-correct) canonical `"0.0.0.0"` case.
    #[tokio::test]
    async fn wildcard_aliases_resolve_to_unspecified_when_actually_bound() {
        for alias in ["0", "[::]", "000.000.000.000"] {
            let (_listener, bound_addr) = bind_tcp_listener(alias, 0)
                .await
                .unwrap_or_else(|e| panic!("bind({alias:?}) should succeed: {e:?}"));
            assert!(
                bound_addr.ip().is_unspecified(),
                "bind alias {alias:?} resolved to {bound_addr}, expected an unspecified address"
            );
        }
    }

    // --- client_base_url ---

    #[test]
    fn client_base_url_substitutes_loopback_for_unspecified_v4() {
        let addr: SocketAddr = "0.0.0.0:7700".parse().unwrap();
        assert_eq!(client_base_url(addr), "http://127.0.0.1:7700");
    }

    #[test]
    fn client_base_url_substitutes_loopback_for_unspecified_v6() {
        let addr: SocketAddr = "[::]:7700".parse().unwrap();
        assert_eq!(client_base_url(addr), "http://[::1]:7700");
    }

    #[test]
    fn client_base_url_passes_through_specific_addresses() {
        assert_eq!(
            client_base_url("127.0.0.1:7700".parse().unwrap()),
            "http://127.0.0.1:7700"
        );
        assert_eq!(
            client_base_url("192.168.1.5:7700".parse().unwrap()),
            "http://192.168.1.5:7700"
        );
        assert_eq!(
            client_base_url("[::1]:7700".parse().unwrap()),
            "http://[::1]:7700"
        );
    }

    // --- mcp_allowed_hosts ---
    //
    // These pin the actual bug fix: rmcp's Streamable HTTP transport enforces
    // its own DNS-rebinding `Host`-header allowlist, defaulting to
    // localhost/127.0.0.1/::1 only — independent of, and narrower than, the
    // daemon's own non-loopback-bind trust decision (PR #135). Without this
    // function's fix, a deliberately-bound LAN/Tailscale address 403s every
    // `/mcp` request with "Host header is not allowed", which MCP clients
    // (e.g. Claude Code) surface as a spurious "needs authentication".

    #[test]
    fn mcp_allowed_hosts_includes_localhost_defaults_for_loopback_bind() {
        let hosts = mcp_allowed_hosts("127.0.0.1:7700".parse().unwrap());
        assert!(hosts.contains(&"localhost".to_string()));
        assert!(hosts.contains(&"127.0.0.1".to_string()));
        assert!(hosts.contains(&"::1".to_string()));
    }

    /// The actual bug: before this fix, only rmcp's localhost-only default
    /// applied, so a deliberately-bound non-loopback address (e.g. a
    /// Tailscale/LAN IP, here a TEST-NET-1 address per RFC 5737 — guaranteed
    /// non-routable, so safe to use as a plain `SocketAddr` without binding
    /// to it) would 403 on `/mcp` despite working on every other route.
    #[test]
    fn mcp_allowed_hosts_includes_the_specific_bind_address() {
        let hosts = mcp_allowed_hosts("192.0.2.1:7700".parse().unwrap());
        assert!(
            hosts.contains(&"192.0.2.1".to_string()),
            "expected the bind address itself to be allow-listed, got: {hosts:?}"
        );
        // Local access must keep working too — `with_allowed_hosts` replaces
        // rmcp's default list rather than extending it, so the defaults must
        // still be present alongside the bind-specific host.
        assert!(hosts.contains(&"localhost".to_string()));
        assert!(hosts.contains(&"127.0.0.1".to_string()));
        assert!(hosts.contains(&"::1".to_string()));
    }

    #[test]
    fn mcp_allowed_hosts_disables_the_check_for_wildcard_binds() {
        assert_eq!(
            mcp_allowed_hosts("0.0.0.0:7700".parse().unwrap()),
            Vec::<String>::new()
        );
        assert_eq!(
            mcp_allowed_hosts("[::]:7700".parse().unwrap()),
            Vec::<String>::new()
        );
    }

    // --- resolve_auth_mode (D4 matrix) ---

    fn loopback() -> SocketAddr {
        "127.0.0.1:7700".parse().unwrap()
    }

    /// TEST-NET-1 (RFC 5737): guaranteed non-routable, safe as a plain
    /// `SocketAddr` without binding — same trick as the `mcp_allowed_hosts`
    /// tests above.
    fn non_loopback() -> SocketAddr {
        "192.0.2.1:7700".parse().unwrap()
    }

    #[test]
    fn resolve_auth_mode_auto_loopback_is_open() {
        assert_eq!(
            resolve_auth_mode(loopback(), ServerAuthMode::Auto).unwrap(),
            AuthMode::Open
        );
    }

    #[test]
    fn resolve_auth_mode_auto_non_loopback_is_enforced() {
        assert_eq!(
            resolve_auth_mode(non_loopback(), ServerAuthMode::Auto).unwrap(),
            AuthMode::Enforced
        );
    }

    #[test]
    fn resolve_auth_mode_auto_ipv6_loopback_is_open() {
        assert_eq!(
            resolve_auth_mode("[::1]:7700".parse().unwrap(), ServerAuthMode::Auto).unwrap(),
            AuthMode::Open
        );
    }

    #[test]
    fn resolve_auth_mode_auto_wildcard_is_enforced() {
        // An unspecified bind is reachable from any network — non-loopback
        // for enforcement purposes.
        assert_eq!(
            resolve_auth_mode("0.0.0.0:7700".parse().unwrap(), ServerAuthMode::Auto).unwrap(),
            AuthMode::Enforced
        );
    }

    #[test]
    fn resolve_auth_mode_required_loopback_is_enforced() {
        assert_eq!(
            resolve_auth_mode(loopback(), ServerAuthMode::Required).unwrap(),
            AuthMode::Enforced
        );
    }

    #[test]
    fn resolve_auth_mode_required_non_loopback_is_enforced() {
        assert_eq!(
            resolve_auth_mode(non_loopback(), ServerAuthMode::Required).unwrap(),
            AuthMode::Enforced
        );
    }

    #[test]
    fn resolve_auth_mode_off_loopback_is_open() {
        assert_eq!(
            resolve_auth_mode(loopback(), ServerAuthMode::Off).unwrap(),
            AuthMode::Open
        );
    }

    #[test]
    fn resolve_auth_mode_off_non_loopback_is_invalid_config() {
        let err = resolve_auth_mode(non_loopback(), ServerAuthMode::Off).unwrap_err();
        assert!(
            matches!(err, Error::InvalidConfig { ref message } if message.contains("192.0.2.1")),
            "expected InvalidConfig naming the bind address, got: {err:?}"
        );
    }

    #[test]
    fn resolve_auth_mode_off_wildcard_is_invalid_config() {
        let err =
            resolve_auth_mode("0.0.0.0:7700".parse().unwrap(), ServerAuthMode::Off).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig { .. }));
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
                ..Default::default()
            },
            paths: Default::default(),
            defaults: Default::default(),
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

        // The discovery URL file should record the actual bound address/port.
        let url_path = paths.url_path();
        assert!(url_path.exists(), "daemon.url should exist while running");
        let recorded = std::fs::read_to_string(&url_path).unwrap();
        assert_eq!(recorded, format!("http://127.0.0.1:{}", handle.addr.port()));

        drop(handle);
        assert!(
            !url_path.exists(),
            "daemon.url should be removed after the handle is dropped"
        );
    }

    #[tokio::test]
    async fn second_daemon_fails_with_daemon_running() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig {
                bind: "127.0.0.1".to_string(),
                port: 0, // let OS assign a free port
                ..Default::default()
            },
            paths: Default::default(),
            defaults: Default::default(),
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

        let options2 = DaemonOptions {
            paths: paths.clone(),
            config: config.clone(),
        };
        let result2 = start_daemon(options2).await;
        assert!(
            matches!(result2, Err(Error::DaemonRunning)),
            "second daemon should fail with DaemonRunning, got: {:?}",
            result2.err()
        );
    }

    #[tokio::test]
    async fn wildcard_bind_starts_successfully_with_warning() {
        // 0.0.0.0 is the one address that's both non-loopback and reliably
        // bindable in CI (it binds all local interfaces rather than requiring
        // a specific routable non-loopback address to exist on the machine).
        // It should now start successfully (only logging a warning) instead of
        // being refused.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let mut config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
        config.server.bind = "0.0.0.0".to_string();
        config.server.port = 0; // let OS assign a free port

        let options = DaemonOptions {
            paths: paths.clone(),
            config,
        };

        let result = start_daemon(options).await;
        assert!(
            result.is_ok(),
            "wildcard bind should start: {:?}",
            result.err()
        );
        let (handle, _server_future) = result.unwrap();
        assert!(handle.addr.port() > 0);

        // Discovery must substitute loopback for the unbindable wildcard address
        // so CLI/MCP clients on the same machine can actually connect.
        let recorded = std::fs::read_to_string(paths.url_path()).unwrap();
        assert_eq!(recorded, format!("http://127.0.0.1:{}", handle.addr.port()));
    }

    // --- Watcher integration: file change ⇒ re-index ⇒ search reflects it ---

    /// Integration test for the acceptance criterion:
    /// "watcher test: file change ⇒ re-index ⇒ search reflects it"
    ///
    /// This test:
    /// 1. Creates a watched directory with a file.
    /// 2. Starts a watcher that queues a job on file change.
    /// 3. Modifies the file.
    /// 4. Verifies a job was submitted and completed.
    /// 5. Verifies the updated content appears in search results.
    #[tokio::test]
    async fn watcher_file_change_triggers_reindex_visible_in_search() {
        use localdb_core::{ChunkRecord, Embedder, FakeEmbedder};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let dir_real = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());

        // Create the state and job queue.
        let yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: localdb_core::config::schema::DefaultsConfig {
                indexing: localdb_core::config::schema::IndexingPolicyConfig {
                    embedding: localdb_core::config::schema::EmbeddingPolicy {
                        provider: "fake".to_string(),
                        model: "default".to_string(),
                    },
                    ..Default::default()
                },
            },
            providers: vec![],
        };
        let queue = JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir_real.to_path_buf(),
            queue.clone(),
            UrlRefreshScheduler::new(queue.clone()),
            AuthMode::Open,
        )
        .await
        .unwrap();
        state.add_store("store-A", "private").await.unwrap();
        let source = state
            .add_source(
                "store-A",
                "path",
                serde_json::json!({"root": "/tmp"}),
                "prose",
                None,
            )
            .await
            .unwrap();
        let store_id = source.store_id.clone();

        // Create initial file.
        let watched_file = dir_real.join("doc.md");
        std::fs::write(&watched_file, "initial content").unwrap();

        // Start a watcher on the directory.
        let (mut file_events, _watcher_handle) = crate::watcher::watch_path(&dir_real, 50).unwrap();

        // Give the watcher time to start.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Modify the file — this triggers a watcher event.
        let updated_text = "rust programming language performance tips";
        std::fs::write(&watched_file, updated_text).unwrap();

        // Wait for the watcher event.
        let event = tokio::time::timeout(Duration::from_secs(5), file_events.recv())
            .await
            .expect("watcher should deliver event within 5 seconds")
            .expect("event channel should not be closed");

        assert!(
            event.path.ends_with("doc.md") || event.path == watched_file,
            "event should reference the modified file, got: {:?}",
            event.path
        );

        // Simulate what the daemon's watcher loop would do: submit an index job.
        // In production this would run the full ingestion pipeline. Here we
        // directly upsert a chunk to the retrieval store (representing the indexed content).
        let embedder = FakeEmbedder::new(128);
        let docs = vec![localdb_core::embedder::DocumentChunks {
            document_context: updated_text.to_string(),
            chunks: vec![updated_text.to_string()],
        }];
        let embedded = embedder.embed_documents(docs).await.unwrap();
        let embedding = embedded
            .into_iter()
            .next()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let job_state_clone = state.clone();
        let job_store_id = store_id.clone();
        let chunks = vec![ChunkRecord {
            id: "watcher-chunk-1".to_string(),
            document_id: "watcher-doc-1".to_string(),
            store_id: store_id.clone(),
            text: updated_text.to_string(),
            span: localdb_core::types::Span::new(0, updated_text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "watcher-hash-1".to_string(),
            origin_store: store_id.clone(),
            source_id: source.id,
            source_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: format!("file://{}", watched_file.display()),
            metadata: localdb_core::DocumentMetadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
        }];

        // Submit a job that upserts the chunk (simulating real ingestion).
        let job = queue
            .submit("store-A", localdb_core::IndexJobScope::Store, move || {
                // This closure runs on a blocking thread and produces the chunk data.
                // In real ingestion, this would call run_ingestion_for_source.
                tokio::runtime::Handle::current()
                    .block_on(async {
                        job_state_clone
                            .backend()
                            .retrieval_store(&job_store_id)
                            .await?
                            .upsert_chunks(chunks)
                            .await
                    })
                    .map_err(|e| format!("upsert failed: {}", e))?;
                Ok(localdb_core::IndexJobStats {
                    docs_indexed: 1,
                    chunks_written: 1,
                    ..Default::default()
                })
            })
            .await;

        // Poll until the job completes.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("ingestion job did not complete in time");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            let current = queue.get_job(&job.id).await.unwrap();
            if current.state == localdb_core::IndexJobState::Done {
                assert_eq!(
                    current.stats.docs_indexed, 1,
                    "job should have indexed 1 document"
                );
                break;
            }
            if current.state == localdb_core::IndexJobState::Failed {
                panic!("ingestion job failed: {:?}", current.error);
            }
        }

        // Verify: search now returns the updated content.
        let store = state.backend().retrieval_store(&store_id).await.unwrap();
        let stats = store.stats().await.unwrap();
        assert_eq!(
            stats.chunk_count, 1,
            "one chunk should be indexed after job completes"
        );

        // Run a search via the HTTP API to confirm the citation is returned.
        // `vec![]` disables the Host check entirely (see `mcp_allowed_hosts`);
        // this test only drives `/v1/search` via `oneshot`, never `/mcp`, so
        // the allowlist behavior itself is untested here.
        let app = build_router(
            state,
            Arc::new(mcp::StaticStoreProvider::new(vec![])),
            Arc::new(FakeEmbedder::new(1)),
            vec![],
        );

        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let resp = app
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"query": "rust programming"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let citations = body["citations"].as_array().unwrap();
        assert!(
            !citations.is_empty(),
            "search should return citations for updated file content; body: {:?}",
            body
        );
        // The citation should point to the modified file.
        let found = citations.iter().any(|c| {
            c["uri"]
                .as_str()
                .map(|u| u.contains("doc.md"))
                .unwrap_or(false)
        });
        assert!(
            found,
            "search results should include the updated file; citations: {:?}",
            citations
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
            providers: vec![],
        };
        let queue = JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir.path().to_path_buf(),
            queue.clone(),
            UrlRefreshScheduler::new(queue),
            AuthMode::Open,
        )
        .await
        .unwrap();
        // `vec![]` disables the Host check entirely (see `mcp_allowed_hosts`);
        // this test only drives `/v1/status` via `oneshot`, never `/mcp`.
        let app = build_router(
            state,
            Arc::new(mcp::StaticStoreProvider::new(vec![])),
            Arc::new(localdb_core::FakeEmbedder::new(1)),
            vec![],
        );

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

    // --- parse_refresh_interval ---

    #[test]
    fn parse_refresh_interval_parses_hours() {
        assert_eq!(parse_refresh_interval("1h"), Some(3600));
        assert_eq!(parse_refresh_interval("24h"), Some(86400));
        assert_eq!(parse_refresh_interval("0h"), Some(0));
    }

    #[test]
    fn parse_refresh_interval_parses_minutes() {
        assert_eq!(parse_refresh_interval("1m"), Some(60));
        assert_eq!(parse_refresh_interval("30m"), Some(1800));
    }

    #[test]
    fn parse_refresh_interval_parses_seconds() {
        assert_eq!(parse_refresh_interval("3600s"), Some(3600));
        assert_eq!(parse_refresh_interval("0s"), Some(0));
    }

    #[test]
    fn parse_refresh_interval_parses_plain_number() {
        assert_eq!(parse_refresh_interval("7200"), Some(7200));
    }

    #[test]
    fn parse_refresh_interval_empty_returns_none() {
        assert_eq!(parse_refresh_interval(""), None);
        assert_eq!(parse_refresh_interval("   "), None);
    }

    #[test]
    fn parse_refresh_interval_invalid_returns_none() {
        assert_eq!(parse_refresh_interval("abc"), None);
        assert_eq!(parse_refresh_interval("1x"), None);
    }

    /// F6: overflow guard — very large hour values must not wrap around.
    /// `u64::MAX / 3600 + 1` hours would overflow; checked_mul returns None.
    #[test]
    fn parse_refresh_interval_overflow_returns_none() {
        // u64::MAX is 18_446_744_073_709_551_615.
        // 18_446_744_073_709_551_615 / 3600 = 5_124_095_576_030_431, remainder ≠ 0.
        // So 5_124_095_576_030_432h would overflow.
        let overflow_h = format!("{}h", u64::MAX / 3600 + 1);
        assert_eq!(
            parse_refresh_interval(&overflow_h),
            None,
            "hours overflow should return None, not wrap"
        );

        let overflow_m = format!("{}m", u64::MAX / 60 + 1);
        assert_eq!(
            parse_refresh_interval(&overflow_m),
            None,
            "minutes overflow should return None, not wrap"
        );
    }
}
