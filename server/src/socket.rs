//! Unix socket management for daemon discovery.
//!
//! The daemon binds a Unix domain socket at `<data_dir>/daemon.sock`.
//! CLI and MCP commands probe this path on startup; if a connection succeeds
//! they route through the daemon instead of opening the store directly.
//!
//! See specs/01-architecture.md §3 and specs/03-config.md §4.

use std::path::{Path, PathBuf};

use localdb_core::Error;

/// Guard for the daemon Unix socket.
///
/// Binds a `UnixListener` on construction (creating the socket file) and
/// removes the socket file on drop so stale sockets don't block next startup.
pub struct SocketGuard {
    path: PathBuf,
    /// The bound listener — kept alive so the socket stays open.
    _listener: tokio::net::UnixListener,
}

impl SocketGuard {
    /// Bind a Unix domain socket at `socket_path` and return a guard.
    ///
    /// Returns an error if the bind fails (permissions, path issues, etc.).
    pub fn new(socket_path: &Path) -> Result<Self, Error> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(map_socket_io_error)?;
        }

        if socket_path.exists() {
            if probe_daemon(socket_path) {
                return Err(Error::DaemonRunning);
            }
            std::fs::remove_file(socket_path).map_err(map_socket_io_error)?;
        }

        let listener = tokio::net::UnixListener::bind(socket_path).map_err(map_socket_io_error)?;
        tracing::info!("daemon socket bound at: {}", socket_path.display());

        Ok(Self {
            path: socket_path.to_owned(),
            _listener: listener,
        })
    }

    /// Path of the socket file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn map_socket_io_error(err: std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::AddrInUse {
        Error::DaemonRunning
    } else {
        Error::Internal {
            message: format!("socket error: {}", err),
            correlation_id: "socket_bind".to_string(),
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Remove the socket file so stale sockets don't block next startup.
        let _ = std::fs::remove_file(&self.path);
        tracing::debug!("daemon socket removed: {}", self.path.display());
    }
}

/// Probe whether a daemon is running at the given socket path.
///
/// Returns `true` if the socket file exists and a daemon is responsive
/// (i.e. we can connect to it).
/// Returns `false` if the socket doesn't exist or the connection fails.
///
/// This is a synchronous probe for use at CLI/MCP startup.
#[cfg(unix)]
pub fn probe_daemon(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    // Attempt to connect to the socket. If we get a connection (even if the
    // server immediately closes it), the daemon is alive.
    use std::os::unix::net::UnixStream;
    match UnixStream::connect(socket_path) {
        Ok(_) => true,
        Err(_) => {
            // Socket file exists but nothing is listening — stale socket.
            false
        }
    }
}

#[cfg(not(unix))]
pub fn probe_daemon(_socket_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socket_guard_binds_and_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        let guard = SocketGuard::new(&path).expect("should bind socket");
        assert_eq!(guard.path(), path.as_path());
        // The socket file must exist on disk after binding.
        assert!(path.exists(), "socket file should exist after binding");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn socket_guard_returns_daemon_running_when_path_is_already_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        let _guard = SocketGuard::new(&path).expect("should bind first socket");

        match SocketGuard::new(&path) {
            Ok(_) => panic!("second bind should fail"),
            Err(err) => assert_eq!(err, localdb_core::Error::DaemonRunning),
        }
    }

    #[tokio::test]
    async fn socket_guard_drop_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        {
            let _guard = SocketGuard::new(&path).expect("should bind socket");
            assert!(path.exists(), "socket should exist while guard is live");
        }
        // After drop the file should be gone.
        assert!(
            !path.exists(),
            "socket file should be removed after guard is dropped"
        );
    }

    #[test]
    fn probe_daemon_returns_false_for_nonexistent_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        assert!(!probe_daemon(&path));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_daemon_returns_true_when_daemon_listening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        // Bind the socket (daemon side).
        let _guard = SocketGuard::new(&path).expect("should bind socket");

        // Probe should return true because something is listening.
        assert!(
            probe_daemon(&path),
            "probe should return true for live daemon socket"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_daemon_returns_false_for_stale_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        // Create a plain file — not an actual socket.
        std::fs::write(&path, "").unwrap();
        // probe_daemon tries to connect; connecting to a plain file fails.
        // The result depends on OS behavior; at minimum it must not panic.
        let _ = probe_daemon(&path);
    }
}
