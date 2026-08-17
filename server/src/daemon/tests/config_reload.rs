//! `run_config_watcher`/`reload_config_file` tests.

use std::path::PathBuf;

use localdb_core::Error;

use crate::daemon::{reload_config_file, run_config_watcher};

use super::common::make_state;

#[tokio::test]
async fn run_config_watcher_returns_invalid_config_when_path_has_no_parent() {
    let (_dir, state) = make_state().await;

    let err = run_config_watcher(PathBuf::new(), state).await.unwrap_err();

    assert!(
        matches!(err, Error::InvalidConfig { .. }),
        "expected InvalidConfig, got: {:?}",
        err
    );
}

#[test]
fn reload_config_file_maps_parse_errors_to_internal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "::not-yaml::").unwrap();

    let err = reload_config_file(&path).unwrap_err();

    assert!(
        matches!(err, Error::Internal { ref correlation_id, .. } if correlation_id == "daemon_config_reload"),
        "expected Internal with daemon_config_reload correlation id, got: {:?}",
        err
    );
}

/// Hot reload must apply the *same* validation the startup path does, not
/// just YAML syntax. Before this, a config that parsed but was rejected by
/// `validate_config` entered a running daemon through the file watcher and
/// failed later at the point of use — for `http.user_agent`, as an opaque
/// "failed to build HTTP client" on the next index job (issue #207).
#[test]
fn reload_config_file_rejects_a_parseable_but_invalid_config() {
    let dir = tempfile::tempdir().unwrap();
    for yaml in [
        "version: 1\nhttp:\n  user_agent: \"bad\\nagent\"\n",
        "version: 1\nhttp:\n  rate_limit:\n    burst: 0\n",
    ] {
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, yaml).unwrap();

        let err = reload_config_file(&path)
            .expect_err("hot reload must reject what startup validation rejects");
        assert!(
            matches!(err, Error::Internal { ref correlation_id, .. } if correlation_id == "daemon_config_reload"),
            "expected Internal with daemon_config_reload correlation id, got: {err:?}"
        );
    }
}
