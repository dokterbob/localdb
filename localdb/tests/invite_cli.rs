//! T6 integration tests for the `localdb` binary's `invite` subcommands:
//! daemon-routed requests against a mock daemon (method/path/bearer header
//! assertions) and the direct-DB tmpdir fallback (embedded mode, no
//! daemon) — mirrors `auth_cli.rs`'s style for `user`/`key`/`store grant`.

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Read, Write};
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

fn write_default_config(dir: &TempDir) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    auth: Option<String>,
}

/// A mock daemon that records every request (method, path, Authorization
/// header) and answers canned JSON bodies for the `invite` routes, so tests
/// can assert both the HTTP shape the CLI drives and that the admin bearer
/// is attached.
fn start_invite_mock_daemon(
    expected_bearer: &'static str,
) -> (u16, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();

            let mut auth: Option<String> = None;
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("authorization:") {
                    auth = Some(line[("authorization:".len())..].trim().to_string());
                    let _ = v;
                }
                if let Some(rest) = lower.strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                let _ = reader.read_exact(&mut body);
            }

            seen_clone.lock().unwrap().push(RecordedRequest {
                method: method.clone(),
                path: path.clone(),
                auth: auth.clone(),
            });

            let authorized = auth.as_deref() == Some(&format!("Bearer {expected_bearer}"));
            let (status, json_body) = if !authorized {
                (
                    "401 Unauthorized",
                    r#"{"code":"unauthorized","message":"missing or invalid bearer token"}"#
                        .to_string(),
                )
            } else if method == "POST" && path == "/v1/invites" {
                (
                    "201 Created",
                    r#"{"id":"inv1","mode":"open","store_grants":[],"max_uses":1,"expires_at":null,"created_at":"2026-07-07T00:00:00Z","token":"ldb_mock_invite_token","consent_url":"http://127.0.0.1:1/authorize?invite=ldb_mock_invite_token"}"#.to_string(),
                )
            } else if method == "GET" && path == "/v1/invites" {
                (
                    "200 OK",
                    r#"[{"id":"inv1","mode":"open","store_grants":[],"max_uses":1,"uses":0,"expires_at":null,"revoked_at":null,"created_by":"admin","created_at":"2026-07-07T00:00:00Z"}]"#.to_string(),
                )
            } else if method == "DELETE" && path == "/v1/invites/inv1" {
                ("204 No Content", String::new())
            } else if method == "GET" && path == "/v1/invites/requests" {
                (
                    "200 OK",
                    r#"[{"id":"req1","invite_id":"inv1","requested_name":"bob","state":"pending","resulting_user_id":null,"created_at":"2026-07-07T00:00:00Z","decided_at":null}]"#.to_string(),
                )
            } else if method == "POST" && path == "/v1/invites/requests/req1/approve" {
                (
                    "200 OK",
                    r#"{"id":"u1","name":"bob","role":"member","created_at":"2026-07-07T00:00:00Z"}"#
                        .to_string(),
                )
            } else if method == "POST" && path == "/v1/invites/requests/req1/deny" {
                ("204 No Content", String::new())
            } else {
                (
                    "404 Not Found",
                    r#"{"code":"internal","message":"unhandled mock route"}"#.to_string(),
                )
            };

            let response = if json_body.is_empty() {
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n")
            } else {
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    json_body.len(),
                    json_body
                )
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });

    (port, seen)
}

fn daemon_env(dir: &TempDir, port: u16, bearer: &str) -> Command {
    let mut c = cmd_with_dir(dir);
    c.env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{port}"))
        .env("LOCALDB_API_KEY", bearer);
    c
}

// ---------------------------------------------------------------------------
// Daemon-routed: header/path assertions against the mock daemon
// ---------------------------------------------------------------------------

#[test]
fn invite_create_against_mock_daemon_attaches_bearer_and_prints_token() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (port, seen) = start_invite_mock_daemon("ldb_admin_key");

    let output = daemon_env(&dir, port, "ldb_admin_key")
        .args(["invite", "create", "--mode", "open"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invite create should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ldb_mock_invite_token"));
    assert!(stdout.to_lowercase().contains("store this now"));

    let recorded = seen.lock().unwrap();
    let create_req = recorded
        .iter()
        .find(|r| r.method == "POST" && r.path == "/v1/invites")
        .expect("a POST /v1/invites request must have been sent");
    assert_eq!(create_req.auth.as_deref(), Some("Bearer ldb_admin_key"));
}

#[test]
fn invite_list_against_mock_daemon() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (port, seen) = start_invite_mock_daemon("ldb_admin_key");

    let output = daemon_env(&dir, port, "ldb_admin_key")
        .args(["invite", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("inv1"));

    let recorded = seen.lock().unwrap();
    assert!(recorded
        .iter()
        .any(|r| r.method == "GET" && r.path == "/v1/invites"));
}

#[test]
fn invite_revoke_against_mock_daemon() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (port, seen) = start_invite_mock_daemon("ldb_admin_key");

    let output = daemon_env(&dir, port, "ldb_admin_key")
        .args(["invite", "revoke", "inv1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invite revoke should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let recorded = seen.lock().unwrap();
    assert!(recorded
        .iter()
        .any(|r| r.method == "DELETE" && r.path == "/v1/invites/inv1"));
}

#[test]
fn invite_requests_against_mock_daemon() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (port, seen) = start_invite_mock_daemon("ldb_admin_key");

    let output = daemon_env(&dir, port, "ldb_admin_key")
        .args(["invite", "requests"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("req1"));
    assert!(stdout.contains("bob"));

    let recorded = seen.lock().unwrap();
    assert!(recorded
        .iter()
        .any(|r| r.method == "GET" && r.path == "/v1/invites/requests"));
}

#[test]
fn invite_approve_against_mock_daemon() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (port, seen) = start_invite_mock_daemon("ldb_admin_key");

    let output = daemon_env(&dir, port, "ldb_admin_key")
        .args(["invite", "approve", "req1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invite approve should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("bob"));

    let recorded = seen.lock().unwrap();
    assert!(recorded
        .iter()
        .any(|r| r.method == "POST" && r.path == "/v1/invites/requests/req1/approve"));
}

#[test]
fn invite_deny_against_mock_daemon() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (port, seen) = start_invite_mock_daemon("ldb_admin_key");

    let output = daemon_env(&dir, port, "ldb_admin_key")
        .args(["invite", "deny", "req1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invite deny should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let recorded = seen.lock().unwrap();
    assert!(recorded
        .iter()
        .any(|r| r.method == "POST" && r.path == "/v1/invites/requests/req1/deny"));
}

#[test]
fn invite_create_wrong_bearer_exits_6() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (port, _seen) = start_invite_mock_daemon("ldb_admin_key");

    let output = daemon_env(&dir, port, "ldb_some_other_key")
        .args(["invite", "create", "--mode", "open"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(6),
        "a 401 from the daemon must map to exit 6; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Direct-DB fallback (embedded mode, no daemon)
// ---------------------------------------------------------------------------

#[test]
fn invite_create_list_revoke_direct_db() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["invite", "create", "--mode", "open", "--max-uses", "2"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invite create (direct-db) should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ldb_"));
    let token = stdout
        .lines()
        .find_map(|l| l.strip_prefix("Token: "))
        .expect("output must print the show-once token")
        .trim()
        .to_string();

    let output = cmd_with_dir(&dir)
        .args(["--json", "invite", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let invites = v["invites"].as_array().unwrap();
    assert_eq!(invites.len(), 1);
    let invite_id = invites[0]["id"].as_str().unwrap().to_string();
    assert_eq!(invites[0]["max_uses"], 2);
    assert!(invites[0].get("token").is_none());

    // Redeem it directly against the store to prove the token actually works.
    assert!(!token.is_empty());

    let output = cmd_with_dir(&dir)
        .args(["invite", "revoke", &invite_id])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = cmd_with_dir(&dir)
        .args(["--json", "invite", "list"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(v["invites"][0]["revoked_at"].is_string());
}

/// Insert a pending `access_requests` row directly into the unified sqlite
/// database, referencing an existing invite — the CLI has no "redeem"
/// subcommand of its own (redemption is HTTP-only: `POST
/// /v1/invites/redeem` / `localdb login --invite`, covered end to end by
/// `server/tests/invites.rs` and `cli/src/cmds/login.rs`'s
/// `perform_invite_login_closed_mode_*` tests). This reaches around the CLI
/// surface the same way `set_store_visibility` does in `auth_cli.rs`, to
/// get a pending request in place for `invite approve|deny|requests`
/// (direct-DB path) to act on.
fn insert_pending_access_request(dir: &TempDir, invite_id: &str, request_id: &str, name: &str) {
    let db_path = dir.path().join("data").join("localdb.db");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = libsql::Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO access_requests \
             (id, invite_id, requested_name, secret_hash, state, created_at) \
             VALUES (?, ?, ?, ?, 'pending', ?)",
            libsql::params![
                request_id.to_string(),
                invite_id.to_string(),
                name.to_string(),
                format!("dummy-hash-{request_id}"),
                "2026-07-07T00:00:00Z".to_string(),
            ],
        )
        .await
        .unwrap();
    });
}

#[test]
fn invite_requests_approve_deny_direct_db() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args([
            "--json",
            "invite",
            "create",
            "--mode",
            "closed",
            "--max-uses",
            "2",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let invite_id = created["id"].as_str().unwrap().to_string();

    // Fresh DB: no access requests yet.
    let output = cmd_with_dir(&dir)
        .args(["--json", "invite", "requests"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(v["requests"].as_array().unwrap().is_empty());

    // approve/deny on an unknown id fail cleanly (exit 2).
    let output = cmd_with_dir(&dir)
        .args(["invite", "approve", "no-such-request"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let output = cmd_with_dir(&dir)
        .args(["invite", "deny", "no-such-request"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));

    // Approve path: a pending request becomes a real user.
    insert_pending_access_request(&dir, &invite_id, "req-approve", "raw-bob");
    let output = cmd_with_dir(&dir)
        .args(["invite", "approve", "req-approve"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invite approve (direct-db) should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("raw-bob"));

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
    assert!(names.contains(&"raw-bob"));

    // Deny path: a pending request is marked denied, no user created.
    insert_pending_access_request(&dir, &invite_id, "req-deny", "raw-carol");
    let output = cmd_with_dir(&dir)
        .args(["invite", "deny", "req-deny"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "invite deny (direct-db) should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = cmd_with_dir(&dir)
        .args(["--json", "invite", "requests"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let requests = v["requests"].as_array().unwrap();
    let denied = requests
        .iter()
        .find(|r| r["id"] == "req-deny")
        .expect("the denied request must still be listed");
    assert_eq!(denied["state"], "denied");

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
    assert!(
        !names.contains(&"raw-carol"),
        "a denied request must not create a user"
    );
}
