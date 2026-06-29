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

        Err(match code {
            "store_not_found" => Error::StoreNotFound { id: msg },
            "source_not_found" => Error::SourceNotFound { id: msg },
            "document_not_found" => Error::DocumentNotFound { id: msg },
            "job_not_found" => Error::JobNotFound { id: msg },
            "runtime_state_locked" => Error::RuntimeStateLocked,
            "daemon_running" => Error::DaemonRunning,
            "daemon_unreachable" => Error::DaemonUnreachable,
            "invalid_config" => Error::InvalidConfig { message: msg },
            "invalid_request" => Error::InvalidRequest { message: msg },
            "index_in_progress" => Error::IndexInProgress,
            "provider_unavailable" => Error::ProviderUnavailable { message: msg },
            "model_missing" => Error::ModelMissing { message: msg },
            _ => Error::Internal {
                message: format!("daemon returned {}: {}", status.as_u16(), msg),
                correlation_id: "daemon_http".to_string(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
}
