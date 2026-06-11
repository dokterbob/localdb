//! Binary-level wiring tests for `localdb mcp` (T10) and `localdb serve` (T11).
//!
//! T10/T11 implemented the MCP server and HTTP daemon as library crates with
//! their own tests, but the `localdb` binary surface must actually dispatch to
//! them. These tests drive the real binary as a subprocess, per
//! specs/05-surfaces.md §2 (CLI), §3 (HTTP API), §4 (MCP).

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Read, Write};
use tempfile::TempDir;

fn cmd() -> Command {
    Command::cargo_bin("localdb").expect("localdb binary must exist")
}

fn cmd_with_dir(dir: &TempDir) -> Command {
    let mut c = cmd();
    c.env("LOCALDB_CONFIG", dir.path().join("config.yaml"));
    c
}

/// Write a config with `paths.data` inside the temp dir plus optional extra YAML.
fn write_config(dir: &TempDir, extra: &str) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\n{}",
        data_dir.to_string_lossy(),
        extra
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

/// Seed a store named `notes` with one indexed markdown file.
fn seed_indexed_store(dir: &TempDir) {
    let corpus = dir.path().join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::write(
        corpus.join("zebra.md"),
        "# Zebra facts\n\nZebras have distinctive stripe patterns used for identification.\n",
    )
    .unwrap();
    cmd_with_dir(dir)
        .args(["store", "add", "notes"])
        .assert()
        .success();
    cmd_with_dir(dir)
        .args([
            "source",
            "add",
            corpus.to_str().unwrap(),
            "--store",
            "notes",
        ])
        .assert()
        .success();
    cmd_with_dir(dir)
        .args(["index", "--store", "notes"])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// MCP over stdio
// ---------------------------------------------------------------------------

/// `localdb mcp` must speak MCP over stdio: initialize → tools/list →
/// tools/call search against an indexed store, then exit 0 on stdin EOF.
#[test]
fn mcp_stdio_initialize_tools_list_and_search() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "");
    seed_indexed_store(&dir);

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"zebra stripes"}}}"#,
        "\n",
    );

    let assert = cmd_with_dir(&dir)
        .arg("mcp")
        .write_stdin(input)
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each stdout line is JSON"))
        .collect();
    // initialize + tools/list + tools/call get responses; the notification does not.
    assert_eq!(responses.len(), 3, "stdout was: {stdout}");

    let init = &responses[0];
    assert_eq!(init["id"], 1);
    assert!(
        init["result"]["protocolVersion"].is_string(),
        "initialize result must carry protocolVersion: {init}"
    );

    let tools = &responses[1];
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools/list result.tools is an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in ["search", "get_document", "list_stores"] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}: {names:?}"
        );
    }

    let call = &responses[2];
    assert!(call["error"].is_null(), "tools/call search errored: {call}");
    assert_eq!(call["result"]["isError"], false);
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("tools/call result has text content");
    assert!(
        text.contains("citations") && text.contains("zebra"),
        "search result must contain citations for the corpus: {text}"
    );
}

/// `--allow-write` is parsed but must be rejected at tool level in v1
/// (server still starts and serves read-only tools).
#[test]
fn mcp_allow_write_flag_still_serves_tools() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "");

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
    );
    let assert = cmd_with_dir(&dir)
        .args(["mcp", "--allow-write"])
        .write_stdin(input)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("tools"), "stdout was: {stdout}");
}

// ---------------------------------------------------------------------------
// HTTP daemon
// ---------------------------------------------------------------------------

/// `localdb serve` must start the daemon: announce the listening address,
/// create the discovery socket, and answer GET /v1/status over HTTP.
#[test]
fn serve_starts_listens_and_serves_status() {
    let dir = TempDir::new().unwrap();
    // Port 0: let the OS pick a free port; the binary must announce the real one.
    write_config(&dir, "server:\n  port: 0\n");

    let bin = assert_cmd::cargo::cargo_bin("localdb");
    let mut child = std::process::Command::new(bin)
        .arg("serve")
        .env("LOCALDB_CONFIG", dir.path().join("config.yaml"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn localdb serve");

    // The first stdout line must announce the bound address.
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read announce line");
    let addr = line
        .split("http://")
        .nth(1)
        .unwrap_or_else(|| panic!("announce line must contain http:// URL, got: {line}"))
        .trim()
        .trim_end_matches('/')
        .to_string();

    // Discovery socket must exist.
    let sock = dir.path().join("data").join("daemon.sock");
    assert!(sock.exists(), "daemon.sock must be created at {sock:?}");

    // Raw HTTP GET /v1/status.
    let mut stream = std::net::TcpStream::connect(&addr).expect("connect to daemon");
    write!(
        stream,
        "GET /v1/status HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    child.kill().ok();
    child.wait().ok();

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "GET /v1/status must return 200, got: {response}"
    );
    assert!(
        response.contains("\"daemon\":true") && response.contains("store_count"),
        "/v1/status body must report daemon status and store_count: {response}"
    );
}
