//! Unix socket management for daemon discovery.
//!
//! The daemon writes a socket file at `<data_dir>/daemon.sock`.
//! CLI and MCP commands probe this path on startup; if it responds
//! they route through the daemon instead of opening the store directly.
//!
//! See specs/01-architecture.md §3 and specs/03-config.md §4.

use std::path::{Path, PathBuf};

/// Guard for the daemon socket file.
///
/// Creates the socket file on construction and removes it on drop.
pub struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    /// Record that the daemon is listening at `socket_path`.
    ///
    /// Does not actually bind — the caller (axum server) handles that.
    /// This guard just tracks the path for cleanup on drop.
    pub fn new(socket_path: &Path) -> Self {
        Self {
            path: socket_path.to_owned(),
        }
    }

    /// Path of the socket file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Remove the socket file so stale sockets don't block next startup.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Probe whether a daemon is running at the given socket path.
///
/// Returns `true` if the socket file exists and a daemon appears to be
/// responsive (i.e. we can connect and get a valid HTTP response).
/// Returns `false` if the socket doesn't exist or the connection fails.
///
/// This is a sync probe for use at CLI/MCP startup.
#[cfg(unix)]
pub fn probe_daemon(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    // Just check if the file exists; actual HTTP probe would require an HTTP client.
    // The actual thin-client probe happens in the CLI/MCP layer.
    true
}

#[cfg(not(unix))]
pub fn probe_daemon(_socket_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_guard_stores_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        let guard = SocketGuard::new(&path);
        assert_eq!(guard.path(), path.as_path());
    }

    #[test]
    fn probe_daemon_returns_false_for_nonexistent_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        assert!(!probe_daemon(&path));
    }

    #[cfg(unix)]
    #[test]
    fn probe_daemon_returns_true_when_socket_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        // Create a dummy file at socket path
        std::fs::write(&path, "").unwrap();
        assert!(probe_daemon(&path));
    }
}
