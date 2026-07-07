//! Integration tests for the T3 auth surface of the `localdb` binary:
//! break-glass `user add` / `key create` (embedded, direct-DB), the
//! daemon-running refusal, exit code 6 on 401/403 daemon answers, and
//! bearer injection (`LOCALDB_API_KEY`, `credentials.json`) into
//! daemon-routed requests.

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn cmd() -> Command {
    Command::cargo_bin("localdb").expect("localdb binary must exist")
}

fn cmd_with_dir(dir: &TempDir) -> Command {
    let mut c = cmd();
    c.env("LOCALDB_CONFIG", dir.path().join("config.yaml"));
    c
}

/// Minimal valid config with `paths.data` inside the temp dir and the fake
/// embedder so everything runs offline (mirrors cli_integration.rs).
fn write_default_config(dir: &TempDir) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

// ---------------------------------------------------------------------------
// Break-glass user/key management (embedded mode)
// ---------------------------------------------------------------------------

#[test]
fn user_add_then_key_create_prints_show_once_ldb_secret() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "alice", "--admin"])
        .assert()
        .success()
        .stdout(predicates::str::contains("alice"))
        .stdout(predicates::str::contains("admin"));

    let output = cmd_with_dir(&dir)
        .args(["key", "create", "--user", "alice"])
        .output()
        .unwrap();
    assert!(output.status.success(), "key create should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ldb_"),
        "key create must print the ldb_-prefixed show-once secret; got: {stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("store this now"),
        "key create must carry the store-this-now warning; got: {stdout}"
    );
}

#[test]
fn user_add_and_key_create_json_shapes() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--json", "user", "add", "bob"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["name"], "bob");
    assert_eq!(v["role"], "member", "no --admin flag means member");
    assert!(v["id"].is_string());

    let output = cmd_with_dir(&dir)
        .args(["--json", "key", "create", "--user", "bob"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["user"], "bob");
    assert!(
        v["secret"].as_str().unwrap().starts_with("ldb_"),
        "secret must be the ldb_-prefixed plaintext"
    );
}

#[test]
fn duplicate_user_add_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "carol"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["user", "add", "carol"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "duplicate user is invalid_request → exit 2"
    );
}

#[test]
fn key_create_for_unknown_user_exits_2_with_hint() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["key", "create", "--user", "nobody"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("user add"),
        "error should hint at `localdb user add`; got: {stderr}"
    );
}

#[test]
fn user_add_refuses_while_daemon_running_exit_4() {
    // LOCALDB_DAEMON_URL makes probe_daemon report Running without a socket
    // file — the same override every other daemon-routing test uses.
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", "http://127.0.0.1:19999")
        .args(["user", "add", "dave", "--admin"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(4),
        "break-glass user add must refuse with daemon_running (exit 4) while a daemon is up"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon"),
        "refusal should mention the daemon; got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Bearer injection + exit-code mapping against a mock daemon
// ---------------------------------------------------------------------------

/// A minimal mock daemon: records each request's Authorization header. A
/// request carrying `Bearer <expected>` gets 200 + a JSON body; anything
/// else gets 401 with the standard error envelope (matching
/// server/src/error.rs, WWW-Authenticate included).
fn start_auth_checking_daemon(expected: &'static str) -> (u16, Arc<Mutex<Vec<Option<String>>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();
    let seen_auth: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_auth_clone = seen_auth.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);

            // Drain headers, capturing Authorization.
            let mut auth: Option<String> = None;
            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line
                    .to_ascii_lowercase()
                    .strip_prefix("authorization:")
                    .map(|_| line["authorization:".len()..].trim().to_string())
                {
                    auth = Some(value);
                }
            }
            seen_auth_clone.lock().unwrap().push(auth.clone());

            let authorized = auth.as_deref() == Some(&format!("Bearer {expected}"));
            let response = if authorized {
                let body = r#"{"status":"ok","name":"daemon-store","id":"daemon-id-1"}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
            } else {
                let body = r#"{"code":"unauthorized","message":"missing bearer token"}"#;
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nWWW-Authenticate: Bearer\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                )
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });

    (port, seen_auth)
}

#[test]
fn daemon_401_maps_to_exit_6_without_key() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let (port, seen) = start_auth_checking_daemon("ldb_expected_key");

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{port}"))
        .args(["store", "add", "auth-store"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(6),
        "unauthorized daemon answer must map to exit 6; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = seen.lock().unwrap();
    assert_eq!(
        recorded.last(),
        Some(&None),
        "no credential anywhere → no Authorization header should be sent"
    );
}

#[test]
fn localdb_api_key_env_is_attached_and_succeeds() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let (port, seen) = start_auth_checking_daemon("ldb_expected_key");

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{port}"))
        .env("LOCALDB_API_KEY", "ldb_expected_key")
        .args(["store", "add", "auth-store"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "with LOCALDB_API_KEY the daemon-routed request must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = seen.lock().unwrap();
    assert_eq!(
        recorded.last().cloned().flatten().as_deref(),
        Some("Bearer ldb_expected_key"),
        "the env credential must arrive as an Authorization: Bearer header"
    );
}

#[test]
fn credentials_json_is_used_when_env_key_absent() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let (port, seen) = start_auth_checking_daemon("ldb_cached_key");
    let base_url = format!("http://127.0.0.1:{port}");

    // credentials.json lives next to config.yaml, keyed by daemon base URL
    // (specs/03-config.md §6). Written by `localdb login` in T4; hand-rolled
    // here since T3 ships only the reader.
    std::fs::write(
        dir.path().join("credentials.json"),
        format!(r#"{{"version":1,"credentials":{{"{base_url}":{{"secret":"ldb_cached_key"}}}}}}"#),
    )
    .unwrap();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &base_url)
        .args(["store", "add", "auth-store"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "cached credentials.json entry must authenticate the request; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = seen.lock().unwrap();
    assert_eq!(
        recorded.last().cloned().flatten().as_deref(),
        Some("Bearer ldb_cached_key"),
        "the cached credential must arrive as an Authorization: Bearer header"
    );
}

// ---------------------------------------------------------------------------
// CLI surface sanity
// ---------------------------------------------------------------------------

#[test]
fn user_and_key_subcommands_are_registered() {
    cmd()
        .args(["user", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("add"));
    cmd()
        .args(["key", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("create"));
}
