//! `probe_daemon`/`probe_daemon_health_inner` socket discovery,
//! `decode_daemon_error` daemon-error-code mapping, and
//! `encode_path_segment` URL-segment percent-encoding.

use tempfile::TempDir;

use crate::daemon_client::{
    decode_daemon_error, encode_path_segment, probe_daemon, probe_daemon_health_inner, DaemonState,
};
use localdb_core::Error;

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

/// Round-trip through the *real* producer, not a hand-typed bare
/// message: `server::error::ApiError::into_response` serializes the
/// JSON `{code, message}` body exactly as the daemon's axum handlers do,
/// and this feeds that body straight into `decode_daemon_error`. Guards
/// against the producer/consumer prefix-doubling regression (issue #187
/// review, finding F4): before the fix, the JSON body's `message`
/// already carried the "invalid config: " `Display` prefix, and
/// `Error::from_code` re-added the same prefix on reconstruction,
/// doubling it in the final `Display`ed error.
#[tokio::test]
async fn decode_daemon_error_round_trips_api_error_response_without_doubling_the_prefix() {
    use axum::response::IntoResponse;
    use server::error::ApiError;

    let source_err = Error::InvalidConfig {
        message: "unconfigured embedder provider".to_string(),
    };
    let response = ApiError::from(source_err.clone()).into_response();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let code = body["code"].as_str().unwrap().to_string();
    let msg = body["message"].as_str().unwrap().to_string();

    let err = decode_daemon_error(&code, msg, status);
    assert_eq!(err, source_err);
    let rendered = err.to_string();
    assert_eq!(
        rendered.matches("invalid config:").count(),
        1,
        "the \"invalid config: \" prefix must appear exactly once, got: {rendered:?}"
    );
}

#[test]
fn encode_path_segment_leaves_unreserved_characters_alone() {
    assert_eq!(encode_path_segment("my_store-1.2~x"), "my_store-1.2~x");
}

#[test]
fn encode_path_segment_escapes_fragment_char() {
    // The exact regression this fixes (finding 1): an unescaped '#'
    // interpolated into `format!("{base}/v1/stores/{name}/sources")`
    // truncates the path at the '#', turning everything after it into a
    // URL fragment the server never receives.
    assert_eq!(encode_path_segment("a#b"), "a%23b");
}

#[test]
fn encode_path_segment_escapes_query_and_path_delimiters() {
    assert_eq!(encode_path_segment("a?b=c"), "a%3Fb%3Dc");
    assert_eq!(encode_path_segment("a/b"), "a%2Fb");
}
