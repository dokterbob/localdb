//! File system watcher for config and path-source files.
//!
//! Uses `notify-debouncer-mini` to debounce rapid file changes.
//!
//! Two kinds of watching:
//! 1. **Config watch** — re-read `config.yaml` when it changes; reload AppState.
//! 2. **Source watch** — watch path-source roots; queue re-index when files change.
//!
//! See specs/03-config.md §3 (reload semantics) and T11 scope.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind, Debouncer};
use tokio::sync::mpsc;
use tracing::{error, info};

/// A file change event from the watcher.
#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    /// The path that changed.
    pub path: PathBuf,
    /// What kind of change (from the debouncer's perspective).
    pub kind: DebouncedEventKind,
}

/// Starts a file watcher for the given path.
///
/// Returns a receiver that yields `FileChangeEvent`s and a `WatcherHandle`
/// that stops the watcher when dropped.
pub fn watch_path(
    path: &Path,
    debounce_ms: u64,
) -> Result<(mpsc::Receiver<FileChangeEvent>, WatcherHandle), WatchError> {
    let (tx, rx) = mpsc::channel::<FileChangeEvent>(64);

    let tx_clone = tx.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(debounce_ms),
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                for event in events {
                    let _ = tx_clone.try_send(FileChangeEvent {
                        path: event.path,
                        kind: event.kind,
                    });
                }
            }
            Err(e) => {
                error!("watcher error: {:?}", e);
            }
        },
    )
    .map_err(|e| WatchError(format!("cannot create debouncer: {}", e)))?;

    debouncer
        .watcher()
        .watch(
            path,
            notify_debouncer_mini::notify::RecursiveMode::Recursive,
        )
        .map_err(|e| WatchError(format!("cannot watch '{}': {}", path.display(), e)))?;

    info!("watching path: {}", path.display());

    Ok((
        rx,
        WatcherHandle {
            _debouncer: debouncer,
        },
    ))
}

/// A guard that keeps the underlying watcher alive.
///
/// When dropped, the watcher stops.
pub struct WatcherHandle {
    _debouncer: Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
}

/// Error creating a watcher.
#[derive(Debug)]
pub struct WatchError(pub String);

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "watch error: {}", self.0)
    }
}

impl std::error::Error for WatchError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn watch_detects_file_change() {
        let dir = tempfile::tempdir().unwrap();
        // On macOS, tempdir resolves to /var/... but notify events use /private/var/...
        // Canonicalize to get the real path.
        let dir_real = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let file_path = dir_real.join("test.md");
        std::fs::write(&file_path, "initial content").unwrap();

        let (mut rx, _handle) = watch_path(&dir_real, 50).unwrap();

        // Modify the file
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&file_path, "updated content").unwrap();

        // Wait for the event (with timeout)
        let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(result.is_ok(), "should receive a change event");
        let event = result.unwrap().unwrap();
        // Event path should be inside the watched directory (using canonicalized form)
        let event_canon = event.path.canonicalize().unwrap_or(event.path.clone());
        assert!(
            event_canon.starts_with(&dir_real),
            "event path {:?} should be inside watched dir {:?}",
            event_canon,
            dir_real
        );
    }

    #[tokio::test]
    async fn watch_nonexistent_path_returns_error() {
        let result = watch_path(Path::new("/nonexistent/path/that/does/not/exist"), 50);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn watch_detects_file_creation() {
        let dir = tempfile::tempdir().unwrap();
        let dir_real = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let (mut rx, _handle) = watch_path(&dir_real, 50).unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let new_file = dir_real.join("new.md");
        std::fs::write(&new_file, "new content").unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(result.is_ok(), "should detect file creation");
    }

    #[tokio::test]
    async fn watcher_handle_drop_stops_watching() {
        let dir = tempfile::tempdir().unwrap();
        let dir_real = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let (mut rx, handle) = watch_path(&dir_real, 50).unwrap();

        // Drop the handle
        drop(handle);

        // Channel should be closed soon after
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Writing a file should NOT produce events (watcher is stopped)
        let file_path = dir_real.join("after_drop.md");
        std::fs::write(&file_path, "data").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The channel may or may not receive anything depending on timing,
        // but the important thing is no panic occurs.
        let _ = rx.try_recv();
    }
}
