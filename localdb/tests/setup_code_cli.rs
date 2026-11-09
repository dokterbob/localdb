//! Integration tests for the `localdb serve` setup-code presentation: the
//! first-admin bootstrap code is a CLI/`--json` concern, not something the
//! daemon process decides to put on stderr — see specs/05-surfaces.md §2.

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn cmd() -> Command {
    Command::cargo_bin("localdb").expect("localdb binary must exist")
}

fn cmd_with_dir(dir: &TempDir) -> Command {
    let mut c = cmd();
    c.env("LOCALDB_CONFIG", dir.path().join("config.yaml"));
    c
}

/// A raw (non-`assert_cmd`) spawnable command for a test that needs to pipe
/// and read the child's stdout/stderr while it keeps running, the way
/// `surface_wiring.rs` spawns `localdb serve`.
fn spawnable_cmd_with_dir(dir: &TempDir) -> std::process::Command {
    let bin = assert_cmd::cargo::cargo_bin("localdb");
    let mut c = std::process::Command::new(bin);
    c.env("LOCALDB_CONFIG", dir.path().join("config.yaml"));
    c
}

/// Write a config with `paths.data` inside the temp dir plus optional extra YAML.
/// Always pins `provider: fake` so tests run offline without an API key.
fn write_config(dir: &TempDir, extra: &str) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n{}",
        data_dir.to_string_lossy(),
        extra
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

#[derive(Debug)]
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

/// Minimal blocking HTTP/1.1 client, in the same hand-rolled-raw-socket style
/// `surface_wiring.rs`/`invite_cli.rs` already use, rather than adding an
/// HTTP client dependency to this test binary just for a form POST.
fn post_form(addr: &str, path: &str, pairs: &[(&str, &str)]) -> RawResponse {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = std::net::TcpStream::connect(addr).expect("connect to daemon");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    RawResponse {
        status,
        headers,
        body: body.to_string(),
    }
}

/// Read the first JSON value from a spawned `localdb --json serve`'s
/// stdout. `print_json` pretty-prints across several lines, so this reads a
/// single JSON value off the stream rather than assuming it fits on one
/// line; a `StreamDeserializer` stops right after that value instead of
/// blocking on `serde_json::from_reader`'s own trailing-data check, which
/// would otherwise wait on bytes the still-running daemon has no reason to
/// send.
fn read_announce_json(child: &mut std::process::Child) -> serde_json::Value {
    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut stream = serde_json::Deserializer::from_reader(reader).into_iter::<serde_json::Value>();
    stream
        .next()
        .expect("stdout must yield at least one JSON value")
        .expect("first stdout value must be valid JSON")
}

/// Present `setup_code` as the consent-form credential and exchange the
/// resulting authorization code for a token, proving the printed code
/// actually completes first-admin bootstrap rather than merely looking
/// plausible.
fn complete_setup_with_code(addr: &str, setup_code: &str) {
    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let redirect_uri = "http://127.0.0.1:1/callback";

    let authorize = post_form(
        addr,
        "/authorize",
        &[
            ("response_type", "code"),
            ("client_id", localdb_core::auth::LOCALDB_CLI_CLIENT_ID),
            ("redirect_uri", redirect_uri),
            ("state", "s"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("credential", setup_code),
        ],
    );
    assert_eq!(
        authorize.status, 303,
        "the setup code must authorize successfully: {authorize:?}"
    );
    let location = authorize
        .headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.clone())
        .expect("redirect must carry a Location header");
    let (_, query) = location
        .split_once('?')
        .expect("Location must carry a query string");
    let code = url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .expect("Location must carry `code`");

    let token = post_form(
        addr,
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", redirect_uri),
            ("client_id", localdb_core::auth::LOCALDB_CLI_CLIENT_ID),
            ("code_verifier", &verifier),
        ],
    );
    assert_eq!(
        token.status, 200,
        "the issued authorization code must exchange for a token: {token:?}"
    );
    let token_body: serde_json::Value = serde_json::from_str(&token.body).unwrap();
    assert!(
        token_body["access_token"]
            .as_str()
            .unwrap()
            .starts_with("ldb_"),
        "token response must carry an ldb_-prefixed access token: {token_body}"
    );
}

#[test]
fn serve_json_announces_setup_code_when_auth_required_and_no_admin() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "server:\n  auth: required\n  port: 0\n");

    let mut child = spawnable_cmd_with_dir(&dir)
        .args(["--json", "serve"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn localdb --json serve");

    let announced = read_announce_json(&mut child);
    assert_eq!(announced["status"], "listening");
    let addr = announced["url"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .to_string();
    let setup_code = announced["setup_code"]
        .as_str()
        .expect("setup_code must be present when auth is required and no admin exists")
        .to_string();
    assert!(
        setup_code.starts_with("ldb_"),
        "setup_code must be an ldb_-prefixed minted secret, got: {setup_code}"
    );

    complete_setup_with_code(&addr, &setup_code);

    child.kill().ok();
    child.wait().ok();
}

#[test]
fn serve_json_omits_setup_code_when_admin_already_exists() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "server:\n  auth: required\n  port: 0\n");
    cmd_with_dir(&dir)
        .args(["user", "add", "admin", "--admin"])
        .assert()
        .success();

    let mut child = spawnable_cmd_with_dir(&dir)
        .args(["--json", "serve"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn localdb --json serve");

    let announced = read_announce_json(&mut child);
    assert!(
        announced.get("setup_code").is_none(),
        "an existing admin must suppress setup_code: {announced}"
    );

    child.kill().ok();
    child.wait().ok();
}

#[test]
fn serve_json_omits_setup_code_in_open_mode() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "server:\n  port: 0\n");

    let mut child = spawnable_cmd_with_dir(&dir)
        .args(["--json", "serve"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn localdb --json serve");

    let announced = read_announce_json(&mut child);
    assert!(
        announced.get("setup_code").is_none(),
        "open mode must not include setup_code: {announced}"
    );

    child.kill().ok();
    child.wait().ok();
}

#[test]
fn serve_human_mode_prints_setup_code_block_with_next_step() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "server:\n  auth: required\n  port: 0\n");

    let mut child = spawnable_cmd_with_dir(&dir)
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn localdb serve");
    let stdout = child.stdout.take().unwrap();

    // Drain stdout on a background thread so a line that never arrives
    // can't block the test forever; the main thread bounds the wait itself.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut collected = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(line) => {
                collected.push_str(&line);
                if collected.contains("--setup-code") {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    child.kill().ok();
    child.wait().ok();

    assert!(
        collected.contains("--setup-code"),
        "human-mode serve must print a ready-to-run `localdb login --setup-code` command: {collected}"
    );
}
