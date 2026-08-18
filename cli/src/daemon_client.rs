use std::collections::HashSet;
use std::path::{Path, PathBuf};

use localdb_core::Error;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

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

pub(crate) fn probe_daemon_health_inner(base_url: &str) -> Option<bool> {
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
///   2. Content of `daemon.url` (the discovery file the daemon writes at startup
///      with its actual client-reachable base URL — see `server::socket::UrlFileGuard`)
///   3. Default `http://127.0.0.1:7700`, for daemons started before `daemon.url`
///      existed or if the file is missing/unreadable
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
    let url_path = data_dir.join("daemon.url");
    if socket_path.exists() {
        // `daemon.sock` is the daemon's liveness sentinel, not a text file to
        // read a URL out of — the daemon records its actual base URL separately
        // in `daemon.url` so discovery works for non-default binds/ports, not
        // just 127.0.0.1:7700.
        let base_url = std::fs::read_to_string(&url_path)
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
            // Stale socket: remove it (and any discovery URL file) and report not running.
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(&url_path);
            DaemonState::NotRunning
        }
    } else {
        DaemonState::NotRunning
    }
}

// ---------------------------------------------------------------------------
// Daemon HTTP client — specs/05-surfaces.md §2, specs/01-architecture.md §3
// ---------------------------------------------------------------------------
//
// When a daemon is running, mutating commands route to its REST API instead of
// writing directly to the embedded store. This thin client issues the
// appropriate HTTP requests and maps responses to exit codes.

pub(crate) async fn daemon_request_async(
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

        Err(decode_daemon_error(code, msg, status))
    }
}

/// Map a daemon HTTP error body's stable `code` string (see
/// `server/src/error.rs` and specs/05-surfaces.md §5) to a `core::Error`.
///
/// Delegates the code -> variant mapping to [`Error::from_code`] — the same
/// mapping `cli::job_attach::finish_job` uses to reconstruct a failed daemon
/// job's typed error from its `error_code`/`error` fields, so the two
/// boundaries (HTTP error bodies, job terminal state) never drift apart. Only
/// the fallback for a code `from_code` doesn't recognize (an unknown/newer
/// code, or `internal`/`unsupported_format`/`extraction_failed`, none of
/// which round-trip through a single message string) is specific to this
/// call site: it folds the HTTP status into the message, which `from_code`
/// has no access to.
fn decode_daemon_error(code: &str, msg: String, status: reqwest::StatusCode) -> Error {
    Error::from_code(code, msg.clone()).unwrap_or_else(|| Error::Internal {
        message: format!("daemon returned {}: {}", status.as_u16(), msg),
        correlation_id: "daemon_http".to_string(),
    })
}

/// RFC 3986 "unreserved" characters (`ALPHA / DIGIT / "-" / "." / "_" /
/// "~"`) are left unencoded; everything else is percent-encoded.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode a single user- or daemon-controlled value for safe
/// inclusion in a daemon request URL — whether as a path segment (a store
/// name, a source id) or a query value (a pagination cursor).
///
/// Without this, a store name containing a URL-structural character (`#`,
/// `?`, `/`) interpolated raw via `format!` silently retargets the request:
/// `"a#b"` in `format!("{base_url}/v1/stores/{name}/sources")` parses as
/// path `/v1/stores/a` with fragment `b/sources` — the fragment is never
/// sent to the server at all, so the request hits `GET /v1/stores/a`
/// instead. The unreserved-only encoding here is safe in both the
/// path-segment and query-value position: percent-encoding round-trips
/// through `axum`'s `Path`/`Query` extractors regardless of which delimiter
/// the raw value happened to contain, so a value can never be split across a
/// URL structural boundary it didn't ask to cross. Over-encoding a character
/// that didn't strictly need it is harmless; under-encoding one that did is
/// this bug.
pub(crate) fn encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

/// Upper bound on pages walked by [`walk_daemon_pages`] — defense in depth
/// beyond the cursor-repeat guard below: even a daemon that never repeats a
/// cursor value cannot make the CLI paginate forever.
const MAX_DAEMON_PAGES: usize = 10_000;

/// Walk a paginated daemon list endpoint (`GET {base_url}{path}`, optionally
/// suffixed with `?cursor=<encoded>` or, when `path` already carries a query
/// string of its own, `&cursor=<encoded>`) to exhaustion, invoking `on_page` with
/// each page's raw `items` array. `on_page` returns `true` to stop walking
/// early (e.g. once a sought item has been found) or `false` to continue to
/// the next page. `path` must already be fully formed (any dynamic segment,
/// e.g. a store name or an existing `?filter=value` query string, pre-encoded
/// via [`encode_path_segment`]) — this function only ever appends the
/// cursor's query value itself, joined with `?` when `path` carries no query
/// string yet or `&` when it already does (e.g. `path` already ending in
/// `?source=<id>`), so the cursor is never merged into the same key as an
/// existing query parameter.
///
/// Shared by every daemon-routed command that paginates a list endpoint
/// (`resolve_daemon_store_scope`'s `GET /v1/stores` walk, `index`'s
/// `GET /v1/stores/{name}/sources` owner walk) so the two guards below can't
/// drift out of sync between call sites.
///
/// Guards against two failure modes a hostile or broken daemon response can
/// trigger:
/// - **Malformed page shape**: a response with a missing or non-array
///   `items` field is `Error::Internal`, not a silently-empty page. Without
///   this, a request that lands on the wrong endpoint (e.g. the
///   fragment-truncation bug `encode_path_segment` fixes) gets back a
///   differently-shaped body — a single resource object, say — and the old
///   `.unwrap_or_default()` swallowed that into an empty item list, which
///   `daemon_store_has_source` then read as a legitimate "not found in this
///   store" rather than an error.
/// - **Cursor cycles**: every `next_cursor` value returned is recorded in a
///   `HashSet`; a repeat of *any* previously-seen value — not just the
///   immediately-preceding one — is `Error::Internal` rather than an
///   infinite loop. A single "does this equal the previous cursor" check
///   only catches an immediate repeat; a daemon alternating between two (or
///   more) cursors never triggers it and loops forever. `MAX_DAEMON_PAGES`
///   additionally bounds the walk even against a daemon that never repeats a
///   cursor value at all.
pub(crate) async fn walk_daemon_pages(
    base_url: &str,
    path: &str,
    mut on_page: impl FnMut(&[serde_json::Value]) -> bool,
) -> Result<(), Error> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();

    for _ in 0..MAX_DAEMON_PAGES {
        let url = match &cursor {
            Some(c) => {
                let sep = if path.contains('?') { '&' } else { '?' };
                format!("{base_url}{path}{sep}cursor={}", encode_path_segment(c))
            }
            None => format!("{base_url}{path}"),
        };
        let resp = daemon_request_async(reqwest::Method::GET, &url, None).await?;
        let items = resp
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Internal {
                message: format!(
                    "unexpected response shape from GET {path} (missing or non-array 'items' \
                     field)"
                ),
                correlation_id: "daemon_pagination_shape".to_string(),
            })?;

        if on_page(items) {
            return Ok(());
        }

        let next_cursor = resp
            .get("next_cursor")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        match next_cursor {
            None => return Ok(()),
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return Err(Error::Internal {
                        message: format!(
                            "daemon returned a repeating pagination cursor '{next}' for GET \
                             {path} — a cursor value was seen twice, which a well-behaved daemon \
                             never produces"
                        ),
                        correlation_id: "daemon_pagination_cycle".to_string(),
                    });
                }
                cursor = Some(next);
            }
        }
    }

    Err(Error::Internal {
        message: format!(
            "daemon pagination for GET {path} did not terminate within {MAX_DAEMON_PAGES} pages"
        ),
        correlation_id: "daemon_pagination_page_cap".to_string(),
    })
}

#[cfg(test)]
mod tests;
