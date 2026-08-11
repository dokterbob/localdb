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
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"0.0.1"}}}"#,
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
    for expected in ["search", "get_document", "get_chunks", "list_stores"] {
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
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"0.0.1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
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

/// Phase 3: `localdb mcp` delegates to a running daemon's `/mcp` HTTP route
/// (Phase 2's mount) instead of opening the store embedded, when a daemon is
/// already up — proven by a real two-subprocess round trip, not a mock.
///
/// Also verifies that `--store` is *honored* in proxied mode
/// (specs/05-surfaces.md §4.2.1). This assertion used to be its exact
/// inverse — it pinned a warning saying the flag was ignored, which was the
/// documented v1 gap until issue #201 showed that "ignored" meant silently
/// serving the daemon's **full** store set precisely when the caller had
/// asked to narrow it.
#[test]
fn mcp_stdio_proxies_to_running_daemon() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "server:\n  port: 0\n");
    seed_indexed_store(&dir);

    let bin = assert_cmd::cargo::cargo_bin("localdb");
    let mut daemon = std::process::Command::new(&bin)
        .arg("serve")
        .env("LOCALDB_CONFIG", dir.path().join("config.yaml"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn localdb serve");

    let daemon_stdout = daemon.stdout.take().unwrap();
    let mut reader = BufReader::new(daemon_stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read announce line");
    let addr = line
        .split("http://")
        .nth(1)
        .unwrap_or_else(|| panic!("announce line must contain http:// URL, got: {line}"))
        .trim()
        .trim_end_matches('/')
        .to_string();
    let base_url = format!("http://{addr}");

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"0.0.1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_stores","arguments":{}}}"#,
        "\n",
    );

    // `--store notes` is passed on purpose: proxied mode must *honor* it.
    // `notes` is the store `seed_indexed_store` creates, so this is a valid
    // name — an invalid one would now exit 3 before serving anything.
    let assert = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &base_url)
        .args(["mcp", "--store", "notes"])
        .write_stdin(input)
        .assert()
        .success();

    daemon.kill().ok();
    daemon.wait().ok();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each stdout line is JSON"))
        .collect();
    assert_eq!(responses.len(), 3, "stdout was: {stdout}");

    // Index by JSON-RPC id rather than arrival position: the proxy issues its
    // own upstream calls, so responses are correlated by id, not ordered.
    let by_id = |id: u64| -> &serde_json::Value {
        responses
            .iter()
            .find(|r| r["id"].as_u64() == Some(id))
            .unwrap_or_else(|| panic!("no response with id {id}; stdout was: {stdout}"))
    };

    let tools = by_id(2);
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools/list result.tools is an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in ["search", "get_document", "get_chunks", "list_stores"] {
        assert!(
            names.contains(&expected),
            "missing tool {expected} via daemon-proxied mcp: {names:?}"
        );
    }

    // The scope reached the daemon: `list_stores` came back filtered to the
    // one store named by `--store`, over a real HTTP hop to a real daemon.
    let call = by_id(3);
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("list_stores result should carry text content: {call}"));
    let listed: serde_json::Value =
        serde_json::from_str(text).expect("list_stores returns JSON text content");
    let stores = listed["stores"].as_array().expect("stores array");
    assert_eq!(
        stores.len(),
        1,
        "a --store-scoped proxy must expose exactly the scoped store: {listed}"
    );
    assert_eq!(stores[0]["name"], "notes");

    // And the old "not honored" warning is gone — its presence would mean
    // the #201 regression is back.
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("--store is not honored"),
        "proxied mode now enforces --store rather than warning it away: {stderr}"
    );
}

/// Codex review fix (PR #145): when `LOCALDB_DAEMON_URL` points at an
/// unreachable daemon (stale env var, or the daemon died between
/// `probe_daemon` and the actual `/mcp` connect), `run_mcp_async` must map
/// the connect failure to `daemon_unreachable` (exit 5) — not `internal`
/// (exit 1) — matching every other daemon-backed CLI path.
/// `probe_daemon` trusts `LOCALDB_DAEMON_URL` unconditionally (no health
/// probe when the override is set — see `daemon_client::probe_daemon`), so
/// pointing it at a closed TCP port reliably drives the `Proxied` branch's
/// `ProxyHandler::connect` into a connection failure.
#[test]
fn mcp_stdio_daemon_unreachable_maps_to_daemon_unreachable_exit_code() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "");

    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
        // listener drops here, closing the port.
    };
    let stale_url = format!("http://127.0.0.1:{closed_port}");

    let assert = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &stale_url)
        .arg("mcp")
        .write_stdin("")
        .assert()
        .code(5);

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("daemon_unreachable") || stderr.contains("daemon"),
        "expected a daemon-unreachable error, got stderr: {stderr}"
    );
}

/// Codex review (P2): a malformed `--store` name must be rejected as
/// `invalid_request`/exit 2 in proxied mode too, matching embedded mode and
/// every other store-scoped command — not surface as `store_not_found`/exit 3
/// merely because the daemon's store set happens not to contain `../evil`.
///
/// `LOCALDB_DAEMON_URL` pointing at a *closed* port is what makes this sharp:
/// `probe_daemon` trusts the override without a health check, so the proxied
/// branch is definitely taken, and the connect that follows would definitely
/// fail with exit 5. Getting exit 2 therefore proves the name was rejected
/// before anything touched the network.
#[test]
fn mcp_proxied_traversal_store_name_exits_2_before_connecting() {
    let dir = TempDir::new().unwrap();
    write_config(&dir, "");

    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
        // listener drops here, closing the port.
    };
    let stale_url = format!("http://127.0.0.1:{closed_port}");

    let assert = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &stale_url)
        .args(["--store", "../evil", "mcp"])
        .write_stdin("")
        .assert()
        .code(2);

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("store not found"),
        "a malformed name is invalid usage, not a missing store: {stderr}"
    );
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
