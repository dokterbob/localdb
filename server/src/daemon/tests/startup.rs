//! `start_daemon` startup tests.

use localdb_core::config::schema::RawConfig;
use localdb_core::Error;

use crate::daemon::{start_daemon, DaemonOptions};

use super::common::make_resolved_paths;

// --- Daemon startup ---

#[tokio::test]
async fn daemon_starts_and_binds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();

    let paths = make_resolved_paths(dir.path());
    let config = RawConfig {
        server: localdb_core::config::schema::ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: 0, // let OS assign a free port
            ..Default::default()
        },
        ..Default::default()
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
        server: localdb_core::config::schema::ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: 0, // let OS assign a free port
            ..Default::default()
        },
        ..Default::default()
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
    let mut config = RawConfig::default();
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
