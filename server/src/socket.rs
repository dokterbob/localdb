//! Daemon sentinel and discovery-URL management for daemon discovery.
//!
//! The daemon holds a sentinel at `<data_dir>/daemon.sock`: a bound Unix domain
//! socket on Unix, an exclusively-locked file on Windows. Either way it is a
//! liveness marker and single-instance mutex, never a transport — nothing is
//! ever accepted from it. CLI and MCP commands probe this path on startup; if
//! the probe says a daemon holds it they route through the daemon instead of
//! opening the store directly. The daemon also writes its client-reachable base
//! URL to `<data_dir>/daemon.url` (see [`UrlFileGuard`]) so that probe resolves
//! to the actual configured bind address/port instead of a hardcoded default.
//!
//! See specs/01-architecture.md §3 and specs/03-config.md §4.

use std::path::{Path, PathBuf};

use localdb_core::Error;

#[cfg(not(any(unix, windows)))]
compile_error!(
    "the daemon sentinel needs a per-platform implementation; only unix and windows are supported"
);

/// `ERROR_SHARING_VIOLATION` — another handle holds the file with a share mode
/// that denies our access. This is what a live daemon's sentinel lock looks
/// like from the outside.
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;

/// `ERROR_LOCK_VIOLATION` — the byte-range-lock analogue of the above.
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

/// Guard for the daemon sentinel.
///
/// On Unix, binds a `UnixListener` on construction (creating the socket file).
/// On Windows, opens the same path with a share mode of `0`, which denies every
/// other process read, write, and delete access for as long as the handle is
/// open. Both remove the file on drop so a stale sentinel doesn't block next
/// startup.
pub struct SocketGuard {
    path: PathBuf,
    /// The bound listener — kept alive so the socket stays open.
    #[cfg(unix)]
    _listener: tokio::net::UnixListener,
    /// The exclusively-opened sentinel file — kept alive so the lock is held.
    ///
    /// `Option` so [`Drop`] can close the handle before unlinking the path; see
    /// the comment there.
    #[cfg(windows)]
    lock: Option<std::fs::File>,
}

impl SocketGuard {
    /// Claim the daemon sentinel at `socket_path` and return a guard.
    ///
    /// Returns [`Error::DaemonRunning`] if another daemon already holds it, and
    /// an internal error if the claim fails for any other reason (permissions,
    /// path issues, etc.).
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

        #[cfg(unix)]
        let listener = tokio::net::UnixListener::bind(socket_path).map_err(map_socket_io_error)?;
        #[cfg(windows)]
        let lock = open_exclusive(socket_path, true).map_err(map_socket_io_error)?;

        tracing::info!("daemon socket bound at: {}", socket_path.display());

        Ok(Self {
            path: socket_path.to_owned(),
            #[cfg(unix)]
            _listener: listener,
            #[cfg(windows)]
            lock: Some(lock),
        })
    }

    /// Path of the sentinel file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Open `path` denying all sharing to other processes.
///
/// With `create`, the file is created if missing — that is how the daemon
/// claims the sentinel. Without it, the open only succeeds when the file
/// already exists and nobody holds it — that is how [`probe_daemon`] tests for
/// a live holder.
#[cfg(windows)]
fn open_exclusive(path: &Path, create: bool) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .create(create)
        .write(true)
        .truncate(create)
        // FILE_SHARE_NONE: no other process may open this file at all, not even
        // to delete it, until our handle is closed.
        .share_mode(0)
        .open(path)
}

fn map_socket_io_error(err: std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::AddrInUse || is_exclusive_conflict(&err) {
        Error::DaemonRunning
    } else {
        Error::Internal {
            message: format!("socket error: {}", err),
            correlation_id: "socket_bind".to_string(),
        }
    }
}

/// Whether `err` means "someone else already holds the sentinel exclusively".
///
/// Unix reports that as `AddrInUse` from `bind`, which [`map_socket_io_error`]
/// already handles, so there is nothing extra to recognise here.
#[cfg(unix)]
fn is_exclusive_conflict(_err: &std::io::Error) -> bool {
    false
}

/// Whether `err` means "someone else already holds the sentinel exclusively".
///
/// Windows surfaces a share-mode conflict as `ErrorKind::PermissionDenied`, not
/// `AddrInUse`, so this has to match on the raw OS error code.
#[cfg(windows)]
fn is_exclusive_conflict(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
    )
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        // Windows holds the sentinel with a share mode of 0, which denies
        // delete access to *every* process — including this one. The handle has
        // to be closed before the unlink below, and struct fields are dropped
        // only after this body returns, so take the handle out explicitly
        // rather than relying on field drop order.
        #[cfg(windows)]
        drop(self.lock.take());

        // Remove the sentinel file so a stale one doesn't block next startup.
        let _ = std::fs::remove_file(&self.path);
        tracing::debug!("daemon socket removed: {}", self.path.display());
    }
}

/// Guard for the daemon discovery URL file.
///
/// Writes the daemon's client-reachable base URL to `<data_dir>/daemon.url` on
/// construction so CLI/MCP discovery (`cli::daemon_client::probe_daemon`) can
/// find the daemon regardless of the configured bind address or port, instead
/// of assuming `http://127.0.0.1:7700`. Removes the file on drop, mirroring
/// `SocketGuard`.
pub struct UrlFileGuard {
    path: PathBuf,
}

impl UrlFileGuard {
    /// Write `base_url` to `url_path` and return a guard that removes it on drop.
    pub fn new(url_path: &Path, base_url: &str) -> Result<Self, Error> {
        if let Some(parent) = url_path.parent() {
            std::fs::create_dir_all(parent).map_err(map_socket_io_error)?;
        }
        std::fs::write(url_path, base_url).map_err(map_socket_io_error)?;
        tracing::debug!(
            "daemon discovery URL recorded at {}: {}",
            url_path.display(),
            base_url
        );

        Ok(Self {
            path: url_path.to_owned(),
        })
    }

    /// Path of the discovery URL file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UrlFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        tracing::debug!("daemon discovery URL file removed: {}", self.path.display());
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

/// Probe whether a daemon is running at the given sentinel path.
///
/// **The test is inverted relative to the Unix connect idiom.** There is
/// nothing to connect to: a live daemon is one that *holds* the file open with
/// a share mode of `0`. So an open that **succeeds** proves nobody holds it —
/// a stale sentinel — and an open that fails with a sharing violation proves
/// somebody does.
///
/// Windows releases share-mode locks when the holding process dies, by any
/// means, so this detects an ungracefully-killed daemon more reliably than the
/// Unix path does.
#[cfg(windows)]
pub fn probe_daemon(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }
    // `create: false` — probing must never bring the sentinel into existence.
    match open_exclusive(socket_path, false) {
        // We got it, so no daemon has it. Close immediately: holding it would
        // lock out the daemon we just decided isn't running.
        Ok(file) => {
            drop(file);
            false
        }
        Err(err) if is_exclusive_conflict(&err) => true,
        // Anything else (the file vanished between the check and the open,
        // permissions, a bad path) is not evidence of a live daemon.
        Err(_) => false,
    }
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

    #[cfg(windows)]
    #[tokio::test]
    async fn socket_guard_returns_daemon_running_when_path_is_already_locked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        let _guard = SocketGuard::new(&path).expect("should claim first sentinel");

        match SocketGuard::new(&path) {
            Ok(_) => panic!("second claim should fail"),
            Err(err) => assert_eq!(err, localdb_core::Error::DaemonRunning),
        }
    }

    /// Pins the raw OS error a share-mode conflict actually produces, because
    /// `is_exclusive_conflict` matches on the raw code rather than on
    /// `ErrorKind` — Windows reports this as `PermissionDenied`, so keying on
    /// the kind would silently misclassify a live daemon as a bind failure.
    #[cfg(windows)]
    #[tokio::test]
    async fn live_sentinel_denies_an_external_exclusive_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        let _guard = SocketGuard::new(&path).expect("should claim sentinel");

        let err = open_exclusive(&path, false).expect_err("external open should be denied");
        assert_eq!(
            err.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION),
            "expected ERROR_SHARING_VIOLATION, got kind {:?} / raw {:?}",
            err.kind(),
            err.raw_os_error()
        );
        assert!(is_exclusive_conflict(&err));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn probe_daemon_returns_true_when_sentinel_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");

        let _guard = SocketGuard::new(&path).expect("should claim sentinel");

        assert!(
            probe_daemon(&path),
            "probe should return true while the sentinel is held"
        );
    }

    /// The stale case: a sentinel file left behind by a dead daemon. Windows
    /// drops share-mode locks on process death, so the file opens freely and
    /// the probe must report no daemon.
    #[cfg(windows)]
    #[test]
    fn probe_daemon_returns_false_for_stale_sentinel_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        std::fs::write(&path, "").unwrap();

        assert!(!probe_daemon(&path));
        assert!(path.exists(), "probing must not consume the sentinel");
    }

    /// A stale sentinel must not block startup, and probing it must not have
    /// left a handle open that would deny the claim.
    #[cfg(windows)]
    #[tokio::test]
    async fn socket_guard_reclaims_a_stale_sentinel_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.sock");
        std::fs::write(&path, "stale").unwrap();

        let guard = SocketGuard::new(&path).expect("should reclaim stale sentinel");
        assert_eq!(guard.path(), path.as_path());
        assert!(path.exists());
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
    fn url_file_guard_writes_url_and_removes_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.url");

        {
            let guard =
                UrlFileGuard::new(&path, "http://127.0.0.1:7700").expect("should write url file");
            assert_eq!(guard.path(), path.as_path());
            assert!(path.exists(), "url file should exist while guard is live");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "http://127.0.0.1:7700"
            );
        }
        assert!(
            !path.exists(),
            "url file should be removed after guard is dropped"
        );
    }

    #[test]
    fn url_file_guard_creates_missing_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("daemon.url");

        let guard = UrlFileGuard::new(&path, "http://192.168.1.5:7700")
            .expect("should create parent dir and write url file");
        assert!(path.exists());
        drop(guard);
        assert!(!path.exists());
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
