use std::path::{Path, PathBuf};

use localdb_core::Error;

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

/// The `credentials.json` key for a request URL: its origin
/// (`scheme://host[:port]`), matching the base URLs `probe_daemon` hands
/// out (which always carry an explicit port). The port is preserved exactly
/// as written rather than normalized to a scheme default, so the key
/// round-trips byte-for-byte with what the daemon recorded in `daemon.url`.
fn base_url_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let scheme = parsed.scheme();
    Some(match parsed.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

/// Resolve the bearer secret for a daemon request per specs/03-config.md §6:
/// `LOCALDB_API_KEY` (read once into `ctx.api_key`) wins; otherwise the
/// `credentials.json` next to the resolved config file, keyed by the
/// request's base URL. `None` sends the request without an Authorization
/// header (fine against an open-mode daemon).
fn bearer_for_request(ctx: &CliContext, url: &str) -> Option<String> {
    let base_url = base_url_of(url)?;
    let config_file = resolved_config_file(ctx);
    crate::credentials::resolve_bearer(ctx.api_key.as_deref(), config_file.as_deref(), &base_url)
}

pub(crate) fn resolved_config_file(ctx: &CliContext) -> Option<std::path::PathBuf> {
    let options = localdb_core::config::loader::LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    localdb_core::config::loader::resolve_config_path(&options, ctx.config_env.as_deref()).ok()
}

fn build_http_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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
/// pre-connect path (`ensure_fresh_bearer`).
async fn redeem_refresh_token(
    base_url: &str,
    refresh_token: &str,
) -> Option<(String, crate::credentials::CredentialEntry)> {
    let client = build_http_client().ok()?;
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
/// against `base_url`'s `/token` endpoint, persisting the rotated pair on
/// success. Returns the new access token to retry with, or `None` if there
/// was nothing to refresh or the refresh itself failed — either way the
/// caller falls through to surfacing the original 401.
///
/// Skipped entirely when `ctx.api_key` (`LOCALDB_API_KEY`) is set: a
/// statically configured bearer isn't part of the login token-pair rotation
/// model, so there is nothing to refresh.
async fn try_refresh_and_persist(ctx: &CliContext, url: &str) -> Option<String> {
    if ctx.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
        return None;
    }
    let base_url = base_url_of(url)?;
    let config_file = resolved_config_file(ctx)?;
    let credentials_file = crate::credentials::credentials_path(&config_file);
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url)?;
    let refresh_token = entry.refresh_token?;

    let (access_token, new_entry) = redeem_refresh_token(&base_url, &refresh_token).await?;
    crate::credentials::write_entry(&credentials_file, &base_url, new_entry).ok()?;

    Some(access_token)
}

/// Resolve a currently-valid bearer for `base_url`, refreshing proactively
/// when the cached access token is expired and a refresh token is on hand.
///
/// This exists for callers that open a long-lived connection up front and
/// can't cheaply retry mid-stream on a 401 the way `daemon_request_async`
/// does (the MCP daemon-proxy handshake in `cmds::surface::run_mcp_async`).
/// Resolution order mirrors `bearer_for_request`/`resolve_bearer`:
/// 1. `ctx.api_key` (`LOCALDB_API_KEY`) wins outright, returned verbatim
///    without touching `credentials.json` — API keys aren't part of the
///    login token-pair rotation model, so there's nothing to refresh.
/// 2. Otherwise, the `credentials.json` entry for `base_url`: if it carries
///    an `access_token` whose `access_expires_at` has passed and a
///    `refresh_token` is present, redeem the refresh token and persist the
///    rotated pair, returning the fresh access token.
/// 3. Otherwise (including a failed refresh attempt, or an entry with no
///    expiry info) the cached secret (`access_token` or legacy `secret`) is
///    returned as-is — best effort, matching today's non-refreshing
///    behavior; a stale-but-still-cached token still gets a chance against
///    the daemon rather than sending no bearer at all.
pub(crate) async fn ensure_fresh_bearer(ctx: &CliContext, base_url: &str) -> Option<String> {
    if let Some(key) = ctx.api_key.as_deref() {
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    let config_file = resolved_config_file(ctx)?;
    let credentials_file = crate::credentials::credentials_path(&config_file);
    let entry = crate::credentials::lookup_entry(&credentials_file, base_url)?;
    let current = entry.access_token.clone().or_else(|| entry.secret.clone());

    let is_expired_access_token = entry.access_token.is_some()
        && entry
            .access_expires_at
            .as_deref()
            .is_some_and(localdb_core::auth::is_expired);

    if is_expired_access_token {
        if let Some(refresh_token) = entry.refresh_token.clone() {
            if let Some((access_token, new_entry)) =
                redeem_refresh_token(base_url, &refresh_token).await
            {
                let _ = crate::credentials::write_entry(&credentials_file, base_url, new_entry);
                return Some(access_token);
            }
        }
    }

    current
}

pub(crate) async fn daemon_request_async(
    ctx: &CliContext,
    method: reqwest::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, Error> {
    let client = build_http_client()?;
    let bearer = bearer_for_request(ctx, url);
    let (status, json) = send_once(
        &client,
        method.clone(),
        url,
        body.as_ref(),
        bearer.as_deref(),
    )
    .await?;

    if status == reqwest::StatusCode::UNAUTHORIZED {
        if let Some(new_access) = try_refresh_and_persist(ctx, url).await {
            let (status2, json2) =
                send_once(&client, method, url, body.as_ref(), Some(&new_access)).await?;
            return if status2.is_success() {
                Ok(json2)
            } else {
                let code = json2
                    .get("code")
                    .and_then(|e| e.as_str())
                    .unwrap_or("internal");
                let msg = json2
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("daemon error")
                    .to_string();
                Err(decode_daemon_error(code, msg, status2))
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
/// Extracted as a pure function so the code -> variant mapping (including
/// the legacy-code fallback below) can be unit-tested without an HTTP round
/// trip.
fn decode_daemon_error(code: &str, msg: String, status: reqwest::StatusCode) -> Error {
    match code {
        "store_not_found" => Error::StoreNotFound { id: msg },
        "source_not_found" => Error::SourceNotFound { id: msg },
        "resource_not_found" => Error::ResourceNotFound { id: msg },
        // Legacy code string from a stale daemon predating the
        // resource_not_found rename (specs/05-surfaces.md §5); a v5+
        // CLI may still talk to an older daemon binary, so keep
        // decoding it to the same variant.
        "document_not_found" => Error::ResourceNotFound { id: msg },
        "job_not_found" => Error::JobNotFound { id: msg },
        "runtime_state_locked" => Error::RuntimeStateLocked,
        "daemon_running" => Error::DaemonRunning,
        "daemon_unreachable" => Error::DaemonUnreachable,
        "invalid_config" => Error::InvalidConfig { message: msg },
        "invalid_request" => Error::InvalidRequest { message: msg },
        "index_in_progress" => Error::IndexInProgress,
        "provider_unavailable" => Error::ProviderUnavailable { message: msg },
        "model_missing" => Error::ModelMissing { message: msg },
        "unauthorized" => Error::Unauthorized { message: msg },
        "forbidden" => Error::Forbidden { message: msg },
        _ => Error::Internal {
            message: format!("daemon returned {}: {}", status.as_u16(), msg),
            correlation_id: "daemon_http".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn base_url_of_extracts_origin_with_port() {
        assert_eq!(
            base_url_of("http://127.0.0.1:7700/v1/stores").as_deref(),
            Some("http://127.0.0.1:7700")
        );
    }

    #[test]
    fn base_url_of_preserves_bracketed_ipv6() {
        assert_eq!(
            base_url_of("http://[::1]:7700/v1/status").as_deref(),
            Some("http://[::1]:7700")
        );
    }

    #[test]
    fn base_url_of_without_port() {
        assert_eq!(
            base_url_of("https://daemon.example.com/v1/search").as_deref(),
            Some("https://daemon.example.com")
        );
    }

    /// Regression test for a reviewer claim that an IPv6 daemon origin key
    /// might be written one way (e.g. by `login`, from the raw base URL
    /// `probe_daemon`/`daemon.url` hand out) and looked up another way (via
    /// `base_url_of` on the constructed request URL), causing a bracket
    /// mismatch and a spurious 401. Both sides in fact go through the same
    /// canonical `scheme://[host]:port` shape — `daemon.url` is written from
    /// `std::net::SocketAddr`'s `Display` impl, which already brackets IPv6
    /// (`[::1]:7700`), and `base_url_of` reserializes via `url::Url`, which
    /// also brackets IPv6 host_str. This test writes a credential keyed by
    /// the raw bracketed base URL (as `login` would) and confirms
    /// `bearer_for_request` on a URL built from that same base URL finds it.
    #[test]
    fn bearer_for_request_matches_credential_written_for_bracketed_ipv6_base_url() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();

        let base_url = "http://[::1]:7700";
        let credentials_file = crate::credentials::credentials_path(&config_file);
        crate::credentials::write_entry(
            &credentials_file,
            base_url,
            crate::credentials::CredentialEntry {
                secret: None,
                access_token: Some("ldb_ipv6_access".to_string()),
                refresh_token: None,
                access_expires_at: None,
            },
        )
        .unwrap();

        let ctx = ctx_with_config(&config_file);
        let request_url = format!("{base_url}/v1/status");
        assert_eq!(
            bearer_for_request(&ctx, &request_url).as_deref(),
            Some("ldb_ipv6_access"),
            "the credential written for the bracketed IPv6 base URL must be \
             found again when looked up via the same base URL derivation"
        );
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
    fn probe_running_with_socket_file_removes_stale_socket() {
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
    fn probe_daemon_env_var_bypasses_socket_check() {
        let dir = TempDir::new().unwrap();
        let state = probe_daemon(dir.path(), Some("http://127.0.0.1:9999"));
        assert!(
            matches!(state, DaemonState::Running { base_url } if base_url == "http://127.0.0.1:9999")
        );
    }

    #[test]
    fn probe_running_reads_base_url_from_discovery_file() {
        // Simulate a daemon bound to a non-default port: `daemon.sock` marks a
        // daemon as present, and `daemon.url` (server::socket::UrlFileGuard)
        // records the real client-reachable base URL to probe.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("daemon.sock"), b"").unwrap();
        std::fs::write(dir.path().join("daemon.url"), &base_url).unwrap();

        let state = probe_daemon(dir.path(), None);
        assert!(
            matches!(state, DaemonState::Running { base_url: found } if found == base_url),
            "expected Running with base_url from daemon.url"
        );
    }

    #[test]
    fn probe_stale_removes_both_socket_and_url_file() {
        // Port 0 is never a listening address, so this deterministically fails
        // the reachability check without depending on port availability in CI.
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let url_path = dir.path().join("daemon.url");
        std::fs::write(&sock_path, b"").unwrap();
        std::fs::write(&url_path, b"http://127.0.0.1:0").unwrap();

        assert!(matches!(
            probe_daemon(dir.path(), None),
            DaemonState::NotRunning
        ));
        assert!(!sock_path.exists(), "stale socket should be removed");
        assert!(
            !url_path.exists(),
            "stale discovery URL file should be removed"
        );
    }

    // -----------------------------------------------------------------
    // T4: 401-retry-with-refresh
    // -----------------------------------------------------------------

    /// A minimal stateful mock daemon (hand-rolled raw TCP, mirroring
    /// `localdb/tests/auth_cli.rs`'s style): any non-`/token` route answers
    /// 401 unless `Authorization: Bearer new_access` is presented; `POST
    /// /token` with `grant_type=refresh_token&refresh_token=old_refresh`
    /// answers a fresh `new_access`/`new_refresh` pair, anything else
    /// `invalid_grant`.
    fn start_refresh_mock_daemon() -> u16 {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());

                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();

                let mut auth: Option<String> = None;
                let mut content_length: usize = 0;
                loop {
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if lower.starts_with("authorization:") {
                        auth = Some(line["authorization:".len()..].trim().to_string());
                    }
                    if let Some(rest) = lower.strip_prefix("content-length:") {
                        content_length = rest.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    let _ = reader.read_exact(&mut body);
                }
                let body = String::from_utf8_lossy(&body).to_string();

                let response = if path.starts_with("/token") {
                    if body.contains("grant_type=refresh_token")
                        && body.contains("refresh_token=old_refresh")
                    {
                        let json = r#"{"access_token":"new_access","refresh_token":"new_refresh","expires_in":3600,"token_type":"Bearer"}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            json.len(),
                            json
                        )
                    } else {
                        let json = r#"{"error":"invalid_grant"}"#;
                        format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            json.len(),
                            json
                        )
                    }
                } else if auth.as_deref() == Some("Bearer new_access") {
                    let json = r#"{"status":"ok"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        json.len(),
                        json
                    )
                } else {
                    let json =
                        r#"{"code":"unauthorized","message":"missing or expired bearer token"}"#;
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nWWW-Authenticate: Bearer\r\nContent-Length: {}\r\n\r\n{}",
                        json.len(),
                        json
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        port
    }

    fn ctx_with_config(config_file: &std::path::Path) -> CliContext {
        CliContext {
            config: Some(config_file.to_path_buf()),
            json: false,
            stores: vec![],
            yes: false,
            daemon_url: None,
            config_env: None,
            api_key: None,
        }
    }

    #[tokio::test]
    async fn expired_access_token_is_retried_once_with_refreshed_token() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        let port = start_refresh_mock_daemon();
        let base_url = format!("http://127.0.0.1:{port}");

        let credentials_file = crate::credentials::credentials_path(&config_file);
        crate::credentials::write_entry(
            &credentials_file,
            &base_url,
            crate::credentials::CredentialEntry {
                secret: None,
                access_token: Some("old_access".to_string()),
                refresh_token: Some("old_refresh".to_string()),
                access_expires_at: None,
            },
        )
        .unwrap();

        let ctx = ctx_with_config(&config_file);
        let result = daemon_request_async(
            &ctx,
            reqwest::Method::GET,
            &format!("{base_url}/v1/status"),
            None,
        )
        .await;

        assert!(
            result.is_ok(),
            "expired-token retry should succeed: {:?}",
            result.err()
        );

        let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
        assert_eq!(entry.access_token.as_deref(), Some("new_access"));
        assert_eq!(entry.refresh_token.as_deref(), Some("new_refresh"));
    }

    #[tokio::test]
    async fn no_refresh_token_available_surfaces_unauthorized_with_login_guidance() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        let port = start_refresh_mock_daemon();
        let base_url = format!("http://127.0.0.1:{port}");

        // No credentials.json entry at all: nothing to refresh.
        let ctx = ctx_with_config(&config_file);
        let result = daemon_request_async(
            &ctx,
            reqwest::Method::GET,
            &format!("{base_url}/v1/status"),
            None,
        )
        .await;

        let err = result.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
        assert!(
            err.to_string().contains("login") || format!("{err:?}").contains("login"),
            "guidance should point at `localdb login`: {err}"
        );
    }

    #[tokio::test]
    async fn stale_refresh_token_that_the_daemon_rejects_surfaces_unauthorized() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        let port = start_refresh_mock_daemon();
        let base_url = format!("http://127.0.0.1:{port}");

        let credentials_file = crate::credentials::credentials_path(&config_file);
        crate::credentials::write_entry(
            &credentials_file,
            &base_url,
            crate::credentials::CredentialEntry {
                secret: None,
                access_token: Some("old_access".to_string()),
                refresh_token: Some("no-longer-valid".to_string()),
                access_expires_at: None,
            },
        )
        .unwrap();

        let ctx = ctx_with_config(&config_file);
        let result = daemon_request_async(
            &ctx,
            reqwest::Method::GET,
            &format!("{base_url}/v1/status"),
            None,
        )
        .await;

        assert!(matches!(result.unwrap_err(), Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn env_api_key_skips_refresh_attempt_entirely() {
        // LOCALDB_API_KEY is a static bearer, not part of the token-pair
        // rotation model — a 401 with it set must not attempt a refresh
        // grant at all (there is nothing to refresh it with).
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        let port = start_refresh_mock_daemon();
        let base_url = format!("http://127.0.0.1:{port}");

        let credentials_file = crate::credentials::credentials_path(&config_file);
        // Even with a valid refresh token cached, the env override must win
        // and no refresh attempt should be made.
        crate::credentials::write_entry(
            &credentials_file,
            &base_url,
            crate::credentials::CredentialEntry {
                secret: None,
                access_token: Some("old_access".to_string()),
                refresh_token: Some("old_refresh".to_string()),
                access_expires_at: None,
            },
        )
        .unwrap();

        let mut ctx = ctx_with_config(&config_file);
        ctx.api_key = Some("ldb_env_override_that_is_wrong".to_string());

        let result = daemon_request_async(
            &ctx,
            reqwest::Method::GET,
            &format!("{base_url}/v1/status"),
            None,
        )
        .await;

        assert!(matches!(result.unwrap_err(), Error::Unauthorized { .. }));
        // The cached refresh token must be untouched — no refresh attempt happened.
        let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
        assert_eq!(entry.access_token.as_deref(), Some("old_access"));
    }

    // -----------------------------------------------------------------
    // ensure_fresh_bearer: proactive pre-connect refresh for the MCP
    // daemon-proxy handshake (`cmds::surface::run_mcp_async`), which has no
    // cheap way to retry mid-stream on a 401 the way `daemon_request_async`
    // does.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn ensure_fresh_bearer_refreshes_an_expired_access_token() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        let port = start_refresh_mock_daemon();
        let base_url = format!("http://127.0.0.1:{port}");

        let credentials_file = crate::credentials::credentials_path(&config_file);
        crate::credentials::write_entry(
            &credentials_file,
            &base_url,
            crate::credentials::CredentialEntry {
                secret: None,
                access_token: Some("old_access".to_string()),
                refresh_token: Some("old_refresh".to_string()),
                access_expires_at: Some(localdb_core::auth::rfc3339_from_now(-10)),
            },
        )
        .unwrap();

        let ctx = ctx_with_config(&config_file);
        let bearer = ensure_fresh_bearer(&ctx, &base_url).await;

        assert_eq!(
            bearer.as_deref(),
            Some("new_access"),
            "an expired access token with a live refresh token must be redeemed proactively"
        );
        let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
        assert_eq!(entry.access_token.as_deref(), Some("new_access"));
        assert_eq!(
            entry.refresh_token.as_deref(),
            Some("new_refresh"),
            "the rotated refresh token must be persisted, not just the access token"
        );
    }

    #[tokio::test]
    async fn ensure_fresh_bearer_returns_unexpired_token_without_refreshing() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        // No mock daemon needed: a fresh token must never trigger a network
        // call, so any unreachable base URL will do — if the code tried to
        // refresh, the test would still pass by accident (redeem failure
        // falls back to the current token), so we additionally assert the
        // credentials file is untouched to catch a spurious refresh attempt.
        let base_url = "http://127.0.0.1:1".to_string();

        let credentials_file = crate::credentials::credentials_path(&config_file);
        crate::credentials::write_entry(
            &credentials_file,
            &base_url,
            crate::credentials::CredentialEntry {
                secret: None,
                access_token: Some("still_good".to_string()),
                refresh_token: Some("unused_refresh".to_string()),
                access_expires_at: Some(localdb_core::auth::rfc3339_from_now(3600)),
            },
        )
        .unwrap();

        let ctx = ctx_with_config(&config_file);
        let bearer = ensure_fresh_bearer(&ctx, &base_url).await;

        assert_eq!(bearer.as_deref(), Some("still_good"));
        let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
        assert_eq!(
            entry.refresh_token.as_deref(),
            Some("unused_refresh"),
            "the refresh token must be untouched — no refresh attempt should happen"
        );
    }

    #[tokio::test]
    async fn ensure_fresh_bearer_env_override_wins_without_touching_credentials() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        let base_url = "http://127.0.0.1:1".to_string();

        // No credentials.json entry at all — if the env override is not
        // returned verbatim first, this would fall through to `None`
        // instead, or (if the code were buggy) attempt a file read.
        let ctx = {
            let mut ctx = ctx_with_config(&config_file);
            ctx.api_key = Some("ldb_env_key".to_string());
            ctx
        };

        let bearer = ensure_fresh_bearer(&ctx, &base_url).await;
        assert_eq!(bearer.as_deref(), Some("ldb_env_key"));

        let credentials_file = crate::credentials::credentials_path(&config_file);
        assert!(
            !credentials_file.exists(),
            "the env override must be returned without ever creating/touching credentials.json"
        );
    }

    #[tokio::test]
    async fn ensure_fresh_bearer_falls_back_to_stale_token_when_refresh_fails() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.yaml");
        std::fs::write(&config_file, "version: 1\n").unwrap();
        let port = start_refresh_mock_daemon();
        let base_url = format!("http://127.0.0.1:{port}");

        let credentials_file = crate::credentials::credentials_path(&config_file);
        crate::credentials::write_entry(
            &credentials_file,
            &base_url,
            crate::credentials::CredentialEntry {
                secret: None,
                access_token: Some("old_access".to_string()),
                // The mock daemon only accepts `old_refresh`; this one is
                // rejected, exercising the best-effort fallback.
                refresh_token: Some("no-longer-valid".to_string()),
                access_expires_at: Some(localdb_core::auth::rfc3339_from_now(-10)),
            },
        )
        .unwrap();

        let ctx = ctx_with_config(&config_file);
        let bearer = ensure_fresh_bearer(&ctx, &base_url).await;

        assert_eq!(
            bearer.as_deref(),
            Some("old_access"),
            "a failed refresh should still hand back the stale cached token \
             (best effort) rather than nothing at all"
        );
    }

    #[test]
    fn decode_daemon_error_maps_resource_not_found() {
        let err = decode_daemon_error(
            "resource_not_found",
            "doc-1".to_string(),
            reqwest::StatusCode::NOT_FOUND,
        );
        assert_eq!(
            err,
            Error::ResourceNotFound {
                id: "doc-1".to_string()
            }
        );
    }

    #[test]
    fn decode_daemon_error_accepts_legacy_document_not_found_code() {
        // A stale daemon (pre-rename) may still emit the legacy
        // "document_not_found" code string; the CLI must decode it to the
        // same `ResourceNotFound` variant a current daemon would produce.
        let err = decode_daemon_error(
            "document_not_found",
            "doc-1".to_string(),
            reqwest::StatusCode::NOT_FOUND,
        );
        assert_eq!(
            err,
            Error::ResourceNotFound {
                id: "doc-1".to_string()
            }
        );
    }
}
