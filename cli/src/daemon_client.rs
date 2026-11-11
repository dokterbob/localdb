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
    /// Bearer secret from `LOCALDB_API_KEY`, read once at startup. Overrides
    /// any `credentials.json` entry for daemon-attached requests
    /// (specs/03-config.md §6).
    pub api_key: Option<String>,
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
        // `daemon.sock` itself is a live Unix socket, not a text file — the
        // daemon records its actual base URL separately in `daemon.url` so
        // discovery works for non-default binds/ports, not just 127.0.0.1:7700.
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

/// Expiry margin for proactively refreshing a still-valid access token
/// before it is actually used: a request that would otherwise land just
/// past expiry (network latency, clock skew) gets a fresh token instead of
/// a guaranteed 401. Applied by `refresh_cached_bearer`'s `needs_refresh`
/// check, shared by every call site through `fresh_bearer_for_url`.
const REFRESH_MARGIN_SECS: i64 = 30;

pub(crate) fn resolved_config_file(ctx: &CliContext) -> Option<std::path::PathBuf> {
    let options = localdb_core::config::loader::LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    localdb_core::config::loader::resolve_config_path(&options, ctx.config_env.as_deref()).ok()
}

/// Build a `reqwest::Client` with a fixed request timeout. The one place
/// every daemon-bound HTTP exchange in `cli` builds its client, so a change
/// to client defaults (timeout policy, redirect policy, ...) only has to
/// happen here instead of at each call site.
pub(crate) fn build_http_client(timeout: std::time::Duration) -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| Error::Internal {
            message: format!("cannot build HTTP client: {}", e),
            correlation_id: "daemon_client_build".to_string(),
        })
}

/// Issue one HTTP request, with an explicit bearer override (rather than
/// re-resolving it from `ctx`/`credentials.json`) so a post-refresh retry
/// can use the freshly rotated access token without a second file lookup.
async fn send_once(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<&serde_json::Value>,
    bearer: Option<&str>,
) -> Result<(reqwest::StatusCode, serde_json::Value), Error> {
    let mut req = client.request(method, url);
    if let Some(secret) = bearer {
        req = req.bearer_auth(secret);
    }
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req.send().await.map_err(|_| Error::DaemonUnreachable)?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    Ok((status, json))
}

/// Redeem `refresh_token` against `base_url`'s `/token` endpoint, returning
/// the new access token and the rotated `CredentialEntry` to persist, or
/// `None` if the request failed outright or the daemon rejected it (expired
/// or revoked refresh token). Pure HTTP exchange — callers own the
/// credentials-file lookup and write so this can be shared by both the
/// retry-on-401 path (`try_refresh_and_persist`) and the proactive
/// pre-connect path (`fresh_bearer_for_url`).
async fn redeem_refresh_token(
    base_url: &str,
    refresh_token: &str,
) -> Option<(String, crate::credentials::CredentialEntry)> {
    let client = build_http_client(std::time::Duration::from_secs(30)).ok()?;
    let token_url = format!("{base_url}/token");
    let resp = client
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let access_token = json.get("access_token")?.as_str()?.to_string();
    let new_refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    let new_entry = crate::credentials::CredentialEntry {
        secret: None,
        access_token: Some(access_token.clone()),
        refresh_token: new_refresh_token.or_else(|| Some(refresh_token.to_string())),
        access_expires_at: Some(localdb_core::auth::rfc3339_from_now(expires_in)),
    };
    Some((access_token, new_entry))
}

/// Attempt a refresh-grant exchange for the stored refresh token (if any)
/// against the matching credentials-file key's `/token` endpoint,
/// persisting the rotated pair on success. The key is resolved from `url`
/// by the same longest-path-segment-prefix match `fresh_bearer_for_url`
/// uses, so a path-prefixed daemon refreshes under the key its credential
/// was written under rather than a re-derived origin. Returns the new
/// access token to retry with, or `None` if there was nothing to refresh,
/// no stored key matched `url`, or the refresh itself failed — either way
/// the caller falls through to surfacing the original rejection (a 401 for
/// `daemon_request_async`, the presented bearer for
/// `cmds::surface::CliBearerSource::refresh`).
///
/// Skipped entirely when `ctx.api_key` (`LOCALDB_API_KEY`) is set: a
/// statically configured bearer isn't part of the login token-pair rotation
/// model, so there is nothing to refresh.
pub(crate) async fn try_refresh_and_persist(
    ctx: &CliContext,
    url: &str,
    rejected: Option<&str>,
) -> Option<String> {
    let config_file = resolved_config_file(ctx)?;
    let path = crate::credentials::credentials_path(&config_file);
    let (key, _) = crate::credentials::lookup_entry_for_url(&path, url)?;
    refresh_cached_bearer(ctx, &key, rejected).await
}

/// Test-only rendezvous between two concurrent callers of
/// `refresh_cached_bearer`, used to prove the credential lock genuinely
/// serializes a race rather than the test merely running sequentially.
/// A no-op unless `LOCALDB_TEST_REFRESH_READY_DIR` is set (it never is
/// outside `concurrent_processes_share_one_refresh_and_preserve_other_origins`'s
/// own child processes): writes this process's marker file, named after
/// `LOCALDB_TEST_REFRESH_WORKER_ID`, then blocks until the sibling worker's
/// marker also appears, bounded so a stuck sibling fails the test loudly
/// instead of hanging it.
#[cfg(test)]
fn rendezvous_before_credential_lock() {
    let Ok(dir) = std::env::var("LOCALDB_TEST_REFRESH_READY_DIR") else {
        return;
    };
    let my_id = std::env::var("LOCALDB_TEST_REFRESH_WORKER_ID").unwrap();
    let sibling_id = if my_id == "0" { "1" } else { "0" };
    std::fs::write(
        std::path::Path::new(&dir).join(format!("prelock-{my_id}")),
        b"",
    )
    .unwrap();
    let sibling_marker = std::path::Path::new(&dir).join(format!("prelock-{sibling_id}"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !sibling_marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "sibling worker did not reach the pre-lock rendezvous in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// One serialized path for reactive HTTP refresh and proactive refresh
/// alike. `rejected` is the bearer sent by the failed request, not a new
/// cache lookup. `base_url` is the exact credentials-file key, not a
/// request URL — callers that only have a request URL resolve the key
/// first (`try_refresh_and_persist`'s `lookup_entry_for_url` call, and
/// `fresh_bearer_for_url`'s own).
pub(crate) async fn refresh_cached_bearer(
    ctx: &CliContext,
    base_url: &str,
    rejected: Option<&str>,
) -> Option<String> {
    if let Some(key) = ctx.api_key.as_deref().filter(|key| !key.is_empty()) {
        return if rejected.is_none() {
            Some(key.to_string())
        } else {
            None
        };
    }
    let config_file = resolved_config_file(ctx)?;
    let path = crate::credentials::credentials_path(&config_file);
    let needs_refresh = |entry: &crate::credentials::CredentialEntry| {
        let current = entry.access_token.as_deref().or(entry.secret.as_deref());
        let due = entry.access_token.is_some()
            && entry
                .access_expires_at
                .as_deref()
                .is_some_and(|at| localdb_core::auth::expires_within(at, REFRESH_MARGIN_SECS));
        due || rejected.is_some_and(|old| current == Some(old))
    };
    // Ordinary reads need no writable directory or lock: rename makes them atomic.
    let entry = crate::credentials::lookup_entry(&path, base_url)?;
    if !needs_refresh(&entry) {
        return entry.access_token.or(entry.secret);
    }
    let current = entry.access_token.clone().or_else(|| entry.secret.clone());
    #[cfg(test)]
    rendezvous_before_credential_lock();
    let Some(lock) = crate::credentials::CredentialLock::acquire(&path)
        .await
        .ok()
    else {
        // A still-usable token must not be dropped just because a proactive
        // refresh can't take the lock (read-only credentials dir, or
        // something else occupying the lock path) — only a bearer the
        // daemon already rejected must never be reused.
        return if rejected.is_none() { current } else { None };
    };
    let entry = lock.lookup(base_url)?;
    let current = entry.access_token.clone().or_else(|| entry.secret.clone());
    // A process that waited for the lock must recheck the replacement entry.
    if needs_refresh(&entry) {
        if let Some(refresh) = entry.refresh_token {
            if let Some((access, updated)) = redeem_refresh_token(base_url, &refresh).await {
                lock.write(base_url, updated).ok()?;
                return Some(access);
            }
        }
        return if rejected.is_none() { current } else { None };
    }
    current
}

/// Resolve a fresh bearer for a request to `url`, refreshing an expired or
/// soon-to-expire cached access token before handing it back instead of
/// after a guaranteed 401:
///
/// 1. A non-empty `ctx.api_key` (`LOCALDB_API_KEY`/`--api-key`) wins
///    unchanged, with no refresh — a statically configured bearer isn't
///    part of the login token-pair rotation model.
/// 2. Otherwise, the `credentials.json` entry whose key is the longest
///    path-segment prefix of `url` is looked up — covering both a
///    bare-origin daemon and one served behind a path (e.g.
///    `https://example.com/team`). No matching entry means no bearer at
///    all (fine against an open-mode daemon).
/// 3. The matched entry is then run through `refresh_cached_bearer`'s
///    proactive-refresh check, so a token that is expired or due to expire
///    within `REFRESH_MARGIN_SECS` is redeemed before use.
///
/// Used by every daemon-bound CLI request: `daemon_request_async` (which
/// keeps its own 401 retry as a backstop for a token that expires mid-flight
/// or is revoked out of band), the job-attach SSE path, and the MCP
/// daemon-proxy's `current()` bearer.
pub(crate) async fn fresh_bearer_for_url(ctx: &CliContext, url: &str) -> Option<String> {
    if let Some(key) = ctx.api_key.as_deref().filter(|key| !key.is_empty()) {
        return Some(key.to_string());
    }
    let config_file = resolved_config_file(ctx)?;
    let path = crate::credentials::credentials_path(&config_file);
    let (key, _) = crate::credentials::lookup_entry_for_url(&path, url)?;
    refresh_cached_bearer(ctx, &key, None).await
}

pub(crate) async fn daemon_request_async(
    ctx: &CliContext,
    method: reqwest::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, Error> {
    let client = build_http_client(std::time::Duration::from_secs(30))?;
    let bearer = fresh_bearer_for_url(ctx, url).await;
    let (status, json) = send_once(
        &client,
        method.clone(),
        url,
        body.as_ref(),
        bearer.as_deref(),
    )
    .await?;

    if status == reqwest::StatusCode::UNAUTHORIZED {
        if let Some(new_access) = try_refresh_and_persist(ctx, url, bearer.as_deref()).await {
            let (status2, json2) =
                send_once(&client, method, url, body.as_ref(), Some(&new_access)).await?;
            return if status2.is_success() {
                Ok(json2)
            } else {
                Err(daemon_response_error(status2, &json2))
            };
        }
        return Err(Error::Unauthorized {
            message: "credentials rejected or expired; run `localdb login` to re-authenticate"
                .to_string(),
        });
    }

    if status.is_success() {
        Ok(json)
    } else {
        Err(daemon_response_error(status, &json))
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
pub(crate) fn daemon_response_error(
    status: reqwest::StatusCode,
    body: &serde_json::Value,
) -> Error {
    decode_daemon_error(
        body.get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("internal"),
        body.get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("daemon error")
            .to_string(),
        status,
    )
}

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
    ctx: &CliContext,
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
        let resp = daemon_request_async(ctx, reqwest::Method::GET, &url, None).await?;
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
