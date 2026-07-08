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
fn user_add_without_direct_db_routes_to_daemon_and_reports_unreachable() {
    // T5: `user add` without `--direct-db` now prefers the daemon when one
    // appears to be running (LOCALDB_DAEMON_URL override, same trick every
    // other daemon-routing test uses) — nothing is actually listening on
    // this port, so the HTTP attempt fails with daemon_unreachable (exit 5),
    // not a refusal. This replaces the old T3 behavior where `user add`
    // hard-refused (exit 4) whenever a daemon appeared to be running.
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", "http://127.0.0.1:19999")
        .args(["user", "add", "dave", "--admin"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(5),
        "an unreachable 'daemon' must surface daemon_unreachable (exit 5), not silently \
         fall back to a direct-DB write; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn user_add_direct_db_flag_bypasses_daemon_even_when_running() {
    // `--direct-db` is the lockout-recovery escape hatch: it always writes
    // directly, warning (not refusing) if a daemon appears to be running.
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", "http://127.0.0.1:19999")
        .args(["user", "add", "dave", "--admin", "--direct-db"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "--direct-db must succeed even though a daemon appears to be running; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("daemon"),
        "should warn that a daemon is running; got: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dave"));
}

#[test]
fn user_add_daemon_routed_succeeds_with_admin_bearer() {
    // The primary T5 path: a daemon is genuinely up, and an admin bearer
    // routes the write over HTTP instead of falling back to direct-DB.
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let (port, seen) = start_auth_checking_daemon("ldb_admin_key");

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{port}"))
        .env("LOCALDB_API_KEY", "ldb_admin_key")
        .args(["--json", "user", "add", "irene", "--admin"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "daemon-routed user add with a valid admin bearer should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = seen.lock().unwrap();
    assert_eq!(
        recorded.last().cloned().flatten().as_deref(),
        Some("Bearer ldb_admin_key"),
        "the admin bearer must have been sent to the daemon"
    );
}

#[test]
fn key_create_direct_db_flag_bypasses_daemon_even_when_running() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "kelly", "--direct-db"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", "http://127.0.0.1:19999")
        .args(["key", "create", "--user", "kelly", "--direct-db"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "--direct-db must succeed even though a daemon appears to be running; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ldb_"));
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

// ---------------------------------------------------------------------------
// T4: login / logout CLI surface
// ---------------------------------------------------------------------------

#[test]
fn login_and_logout_subcommands_are_registered_with_expected_flags() {
    cmd()
        .args(["login", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--url"))
        .stdout(predicates::str::contains("--setup-code"))
        .stdout(predicates::str::contains("--no-browser"));
    cmd()
        .args(["logout", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--url"));
}

#[test]
fn login_without_a_reachable_daemon_exits_5() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // No daemon.sock/daemon.url and no LOCALDB_DAEMON_URL override: there is
    // nothing to log into (specs/05-surfaces.md §2 — login only makes sense
    // daemon-attached).
    let output = cmd_with_dir(&dir).args(["login"]).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(5),
        "login without a reachable daemon must exit 5 (daemon_unreachable); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn logout_without_a_cached_credential_reports_nothing_to_clear() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["logout", "--url", "http://127.0.0.1:19999"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "logout with no cached credential should still succeed as a no-op; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.to_lowercase().contains("no cached credentials"),
        "expected a 'no cached credentials' message; got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// T5: user list/remove/set-role, key list/revoke, store grant/revoke
// (direct-DB, daemon-absent mode).
// ---------------------------------------------------------------------------

/// Flip a store's visibility directly in the unified sqlite database — the
/// CLI itself has no `--visibility` flag on `store add` (every CLI-created
/// store is `private`), so tests that need a `shared` store to exercise
/// `store grant` reach around the CLI surface the same way a human with a
/// SQLite client would.
fn set_store_visibility(dir: &TempDir, name: &str, visibility: &str) {
    let db_path = dir.path().join("data").join("localdb.db");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = libsql::Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE stores SET visibility = ? WHERE name = ?",
            libsql::params![visibility, name],
        )
        .await
        .unwrap();
    });
}

#[test]
fn user_list_shows_created_users() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "alice", "--admin"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["user", "add", "bob"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "user", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = v["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alice"));
    assert!(names.contains(&"bob"));
}

#[test]
fn user_set_role_then_remove_non_last_admin() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "carla", "--admin"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["user", "add", "dan"])
        .assert()
        .success();

    // Promote dan to admin — two admins now.
    cmd_with_dir(&dir)
        .args(["user", "set-role", "dan", "admin"])
        .assert()
        .success()
        .stdout(predicates::str::contains("admin"));

    // carla can now be removed — dan remains an admin.
    cmd_with_dir(&dir)
        .args(["user", "remove", "carla"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "user", "list"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = v["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"carla"));
    assert!(names.contains(&"dan"));
}

#[test]
fn user_set_role_unknown_user_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["user", "set-role", "nobody", "admin"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn user_remove_last_admin_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "solo", "--admin"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["user", "remove", "solo"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "removing the last admin must be refused (exit 2, invalid_request); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn user_set_role_last_admin_to_member_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "solo2", "--admin"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["user", "set-role", "solo2", "member"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn key_list_shows_keys_without_secrets_then_revoke() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "erin"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "key", "create", "--user", "erin"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let key_id = v["key_id"].as_str().unwrap().to_string();
    let secret = v["secret"].as_str().unwrap().to_string();

    let output = cmd_with_dir(&dir)
        .args(["--json", "key", "list", "--user", "erin"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&key_id),
        "key list must show the key id; got: {stdout}"
    );
    assert!(
        !stdout.contains(&secret),
        "key list must never show the plaintext secret; got: {stdout}"
    );

    cmd_with_dir(&dir)
        .args(["key", "revoke", &key_id])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "key", "list", "--user", "erin"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let revoked_at = v["keys"].as_array().unwrap()[0]["revoked_at"].clone();
    assert!(
        !revoked_at.is_null(),
        "revoked key must show a revoked_at timestamp"
    );
}

#[test]
fn key_revoke_unknown_id_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["key", "revoke", "no-such-key-id"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn store_grant_revoke_direct_db_roundtrip() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "shared-notes"])
        .assert()
        .success();
    set_store_visibility(&dir, "shared-notes", "shared");

    cmd_with_dir(&dir)
        .args(["user", "add", "frank"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["store", "grant", "shared-notes", "frank"])
        .assert()
        .success()
        .stdout(predicates::str::contains("frank"));

    cmd_with_dir(&dir)
        .args(["store", "revoke", "shared-notes", "frank"])
        .assert()
        .success()
        .stdout(predicates::str::contains("frank"));

    // Revoking again (no grant left) is a no-op error.
    let output = cmd_with_dir(&dir)
        .args(["store", "revoke", "shared-notes", "frank"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn store_grant_unknown_user_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "shared-notes2"])
        .assert()
        .success();
    set_store_visibility(&dir, "shared-notes2", "shared");

    let output = cmd_with_dir(&dir)
        .args(["store", "grant", "shared-notes2", "nobody"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn store_grant_unknown_store_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["user", "add", "gwen"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["store", "grant", "no-such-store", "gwen"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn store_grant_on_private_store_exits_6() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // CLI-created stores default to private; no visibility flip here.
    cmd_with_dir(&dir)
        .args(["store", "add", "private-notes"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["user", "add", "harry"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["store", "grant", "private-notes", "harry"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(6),
        "granting a private store must be forbidden (exit 6); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// T5: member key against a mock daemon that answers 403 for an admin op
// ---------------------------------------------------------------------------

/// A mock daemon that always answers 403 Forbidden, regardless of path or
/// credential — simulates a member's bearer hitting an admin-only route.
fn start_forbidden_daemon() -> u16 {
    use std::io::{BufRead, BufReader, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            let body = r#"{"code":"forbidden","message":"user has role 'member'; admin required"}"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    port
}

#[test]
fn member_key_against_mock_daemon_admin_op_exits_6() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let port = start_forbidden_daemon();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{port}"))
        .env("LOCALDB_API_KEY", "ldb_some_member_key")
        .args(["user", "list"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(6),
        "a 403 from the daemon on an admin op must map to exit 6; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn logout_url_flag_bypasses_daemon_probing() {
    // `--url` is honored even when no daemon is detected via the socket —
    // logout should not require a live daemon to clear a local credential.
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"version":1,"credentials":{"http://127.0.0.1:19999":{"secret":"ldb_stale"}}}"#,
    )
    .unwrap();

    let output = cmd_with_dir(&dir)
        .args(["logout", "--url", "http://127.0.0.1:19999"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.to_lowercase().contains("logged out"));

    let contents = std::fs::read_to_string(dir.path().join("credentials.json")).unwrap();
    assert!(
        !contents.contains("ldb_stale"),
        "the cached credential must be removed: {contents}"
    );
}
