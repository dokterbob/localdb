//! Integration tests for the `localdb` binary.
//!
//! These tests use `assert_cmd` to drive the binary as a subprocess,
//! verifying the CLI surface from specs/05-surfaces.md §2.
//!
//! Test categories:
//! - Help and version flags
//! - End-to-end workflow: init → store add → source add → index → search
//! - --json output shape
//! - Locked-store exit code (exit 4)
//! - Daemon-probe state (no daemon → embedded mode)

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper: build a Command for the localdb binary pointing at a temp config
// ---------------------------------------------------------------------------

fn cmd() -> Command {
    Command::cargo_bin("localdb").expect("localdb binary must exist")
}

/// Build a Command pre-loaded with a config pointing to a temporary directory.
fn cmd_with_dir(dir: &TempDir) -> Command {
    let mut c = cmd();
    c.env("LOCALDB_CONFIG", dir.path().join("config.yaml"));
    c
}

/// Write a minimal valid config to `dir/config.yaml`, with `paths.data`
/// pointing inside the temp dir to avoid polluting the user's data dir.
/// Pins `provider: fake` so integration tests run offline without any API key.
fn write_default_config(dir: &TempDir) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

/// Write a YAML config with a specific data dir and extra content.
fn write_config_with_data_dir(dir: &TempDir, extra: &str) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\n{}\n",
        data_dir.to_string_lossy(),
        extra
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

// ---------------------------------------------------------------------------
// Basic CLI surface tests (from T01 acceptance criteria, still valid)
// ---------------------------------------------------------------------------

/// `localdb --help` must list all subcommands from specs/05-surfaces.md §2.
#[test]
fn help_lists_all_subcommands() {
    let output = cmd()
        .arg("--help")
        .output()
        .expect("localdb --help should succeed");

    assert!(output.status.success(), "--help should exit 0");

    let help_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    for subcommand in &[
        "init", "serve", "mcp", "status", "store", "source", "index", "search",
    ] {
        assert!(
            help_text.contains(subcommand),
            "--help output is missing subcommand '{subcommand}';\nfull output:\n{help_text}",
        );
    }
}

/// `localdb --version` must exit 0 and print a version string.
#[test]
fn version_flag() {
    cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("localdb"));
}

/// `localdb store --help` must list add/list/remove.
#[test]
fn store_subcommand_help() {
    cmd()
        .args(["store", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("add"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("remove"));
}

/// `localdb source --help` must list add/list/remove.
#[test]
fn source_subcommand_help() {
    cmd()
        .args(["source", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("add"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("remove"));
}

/// Unknown subcommand must exit non-zero with a helpful error.
#[test]
fn unknown_subcommand_fails() {
    cmd().arg("nonexistent-subcommand").assert().failure();
}

/// `localdb search` requires a query argument.
#[test]
fn search_requires_query() {
    cmd().arg("search").assert().failure();
}

// ---------------------------------------------------------------------------
// serve / mcp wiring
// ---------------------------------------------------------------------------
// Full behavioral coverage lives in tests/surface_wiring.rs; here we only
// check that the subcommands exist and run (mcp exits 0 on stdin EOF).

#[test]
fn mcp_exits_cleanly_on_stdin_eof() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .arg("mcp")
        .write_stdin("")
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

#[test]
fn init_creates_config_and_data_dir() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // Run init via env var (config already has paths.data set to temp dir).
    cmd_with_dir(&dir).arg("init").assert().success();

    // Config file must exist.
    assert!(dir.path().join("config.yaml").exists());
    // Data dir must exist.
    assert!(dir.path().join("data").exists());
}

#[test]
fn init_json_output_has_status_ok() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--json", "init"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("init --json must emit valid JSON; got: {stdout}"));
    assert_eq!(v["status"].as_str().unwrap(), "ok");
    assert!(v.get("config_path").is_some());
}

#[test]
fn init_is_idempotent() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    // First init.
    cmd_with_dir(&dir).arg("init").assert().success();
    // Second init — should still succeed.
    cmd_with_dir(&dir).arg("init").assert().success();
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

// `status` resolves its store scope like every other all-stores command
// (specs/05-surfaces.md §2.2): a database with zero stores is exit 2, not a
// silent empty success (see the zero-store tests further down). These two
// tests exercise the success path, so they need at least one store to exist
// first.

#[test]
fn status_shows_daemon_not_running() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .args(["store", "add", "mystore"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("not running"));
}

#[test]
fn status_json_has_daemon_field() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .args(["store", "add", "mystore"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .arg("--json")
        .arg("status")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("status --json must emit valid JSON; got: {stdout}"));
    assert!(v.get("daemon").is_some());
    assert!(v.get("stores").is_some());
}

// ---------------------------------------------------------------------------
// store add / list / remove
// ---------------------------------------------------------------------------

#[test]
fn store_add_and_list() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "mystore"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mystore"));

    cmd_with_dir(&dir)
        .args(["store", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mystore"));
}

#[test]
fn store_add_json_output() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--json", "store", "add", "jsonstore"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["status"].as_str().unwrap(), "ok");
    assert_eq!(v["name"].as_str().unwrap(), "jsonstore");
    assert!(v.get("id").is_some(), "id should be present");
}

#[test]
fn store_list_json_has_stores_array() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "store", "list"])
        .output()
        .unwrap();

    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let stores = v["stores"].as_array().expect("stores must be an array");
    assert!(!stores.is_empty());
    // Each store has name, visibility, backend (ownership removed — DB-only now).
    let store = &stores[0];
    assert!(store.get("name").is_some());
    assert!(store.get("visibility").is_some());
    assert!(store.get("backend").is_some());
}

#[test]
fn store_remove_success() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "removeme"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["store", "remove", "--yes", "removeme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removeme"));

    // Store should no longer appear in list. With the store removed, the
    // database now has zero stores — under the all-stores scope policy
    // (specs/05-surfaces.md §2.2) that's a loud exit 2, not a silent empty
    // success (see the zero-store tests further down for the rationale).
    let output = cmd_with_dir(&dir).args(["store", "list"]).output().unwrap();
    assert_eq!(output.status.code().unwrap(), 2);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no stores"),
        "expected a 'no stores' message; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn store_remove_not_found_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["store", "remove", "--yes", "nosuchstore"])
        .output()
        .unwrap();

    // Exit code 3 = not found.
    assert_eq!(output.status.code().unwrap(), 3);
}

// ---------------------------------------------------------------------------
// source add / list / remove
// ---------------------------------------------------------------------------

#[test]
fn source_add_and_list() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // Create store first.
    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    cmd_with_dir(&dir)
        .args(["--store", "s1", "source", "add", fixture.to_str().unwrap()])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["--store", "s1", "source", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("path"));
}

#[test]
fn source_add_json_output() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s2"])
        .assert()
        .success();

    let fixture = dir.path().join("docs2");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "s2",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["status"].as_str().unwrap(), "ok");
    assert!(v.get("id").is_some());
    assert_eq!(v["kind"].as_str().unwrap(), "path");
}

#[test]
fn source_remove_not_found_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--store", "s1", "source", "remove", "nosuchid"])
        .output()
        .unwrap();

    assert_eq!(output.status.code().unwrap(), 3);
}

/// `localdb add <path>` is an alias for `localdb source add`.
#[test]
fn add_alias_works_like_source_add() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "alias-store"])
        .assert()
        .success();

    let fixture = dir.path().join("docs-alias");
    std::fs::create_dir_all(&fixture).unwrap();

    cmd_with_dir(&dir)
        .args(["--store", "alias-store", "add", fixture.to_str().unwrap()])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["--store", "alias-store", "source", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("path"));
}

// ---------------------------------------------------------------------------
// End-to-end: init → store add → source add → index → search
//
// This is the key acceptance criterion from the T09 ticket.
// Uses FakeEmbedder + LanceDB tmpdir (no real model downloads needed).
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_init_store_source_index_search() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // --- init ---
    cmd_with_dir(&dir).arg("init").assert().success();

    // --- store add ---
    cmd_with_dir(&dir)
        .args(["store", "add", "e2e-store"])
        .assert()
        .success();

    // --- create fixture document ---
    let docs_dir = dir.path().join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();
    std::fs::write(
        docs_dir.join("hello.md"),
        "# Hello World\n\nThis is a test document about localdb search.\n",
    )
    .unwrap();

    // --- source add ---
    cmd_with_dir(&dir)
        .args(["--store", "e2e-store", "source", "add"])
        .arg(docs_dir.to_str().unwrap())
        .assert()
        .success();

    // --- index ---
    cmd_with_dir(&dir)
        .args(["--store", "e2e-store", "index"])
        .assert()
        .success();

    // --- search ---
    let output = cmd_with_dir(&dir)
        .arg("--json")
        .args(["--store", "e2e-store", "search", "hello world test"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "search should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("search --json must emit valid JSON; got: {stdout}"));

    // Must have citations array.
    let citations = v["citations"].as_array().expect("citations must be array");

    // At least one citation must be returned from the indexed document.
    assert!(
        !citations.is_empty(),
        "search should return at least one citation for the indexed document;\ngot: {stdout}"
    );

    // Citation must have the FULL canonical shape from specs/02-domain-model.md §6.
    let cit = &citations[0];
    assert!(cit.get("chunk_id").is_some(), "missing chunk_id");
    assert!(cit.get("resource_id").is_some(), "missing resource_id");
    assert!(cit.get("uri").is_some(), "missing uri");
    assert!(cit.get("snippet").is_some(), "missing snippet");
    assert!(cit.get("score").is_some(), "missing score");

    // store: {id, name}
    let store = cit.get("store").expect("missing store field");
    assert!(store.get("id").is_some(), "store.id missing");
    assert!(store.get("name").is_some(), "store.name missing");

    // block: {seq, kind}
    let block = cit.get("block").expect("missing block field");
    assert!(block.get("seq").is_some(), "block.seq missing");
    assert!(block.get("kind").is_some(), "block.kind missing");

    // chunk_position: {seq_in_block}
    let chunk_position = cit
        .get("chunk_position")
        .expect("missing chunk_position field");
    assert!(
        chunk_position.get("seq_in_block").is_some(),
        "chunk_position.seq_in_block missing"
    );

    // location: {span: {start, end}, window_block_seqs?}
    let location = cit.get("location").expect("missing location field");
    let span = location.get("span").expect("missing location.span field");
    assert!(span.get("start").is_some(), "location.span.start missing");
    assert!(span.get("end").is_some(), "location.span.end missing");

    // heading_path (array, may be empty)
    assert!(
        cit.get("heading_path")
            .map(|v| v.is_array())
            .unwrap_or(false),
        "heading_path must be a JSON array"
    );

    // provenance: {fetched_at, content_hash}
    let prov = cit.get("provenance").expect("missing provenance field");
    assert!(
        prov.get("fetched_at").is_some(),
        "provenance.fetched_at missing"
    );
    assert!(
        prov.get("content_hash").is_some(),
        "provenance.content_hash missing"
    );

    // score sub-fields
    let score = cit.get("score").unwrap();
    assert!(score.get("fused").is_some(), "score.fused missing");

    // URI must point to our fixture file.
    let uri = cit["uri"].as_str().unwrap();
    assert!(
        uri.contains("hello.md"),
        "citation URI should point to hello.md; got: {}",
        uri
    );
}

// ---------------------------------------------------------------------------
// --json output canonical shapes
// ---------------------------------------------------------------------------

#[test]
fn search_json_citations_canonical_shape() {
    // Verify the JSON citation shape has all required top-level fields.
    // We test with an empty store — an empty citations array is valid.
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "test-store"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "--store", "test-store", "search", "anything"])
        .output()
        .unwrap();

    // Either success (empty results) or an error that isn't a parse failure.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        let v: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| panic!("search --json must emit valid JSON; got: {stdout}"));
        assert!(v.get("citations").is_some(), "must have citations key");
    }
}

/// `stores:` key in config is now rejected (DB is the single source of truth).
#[test]
fn config_with_stores_key_exits_2() {
    let dir = TempDir::new().unwrap();
    write_config_with_data_dir(&dir, "stores:\n  - name: yaml-store");

    let output = cmd_with_dir(&dir).args(["store", "list"]).output().unwrap();

    // deny_unknown_fields rejects stores: → invalid config → exit 2.
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "stores: key should be rejected with exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Adding a duplicate store name exits 2 (invalid request).
#[test]
fn duplicate_store_name_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "dup-store"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["store", "add", "dup-store"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "duplicate store name should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

// ---------------------------------------------------------------------------
// Daemon-attached routing — mock HTTP server (acceptance criterion)
//
// When a daemon socket file is present (daemon.sock exists in data dir),
// mutating commands must route to the daemon's HTTP API.
// This test spins up a minimal mock HTTP server that records requests,
// creates the daemon.sock sentinel file pointing to the mock server's port,
// then runs `store add` and verifies the request was forwarded to the mock.
//
// Per specs/05-surfaces.md §2 and specs/01-architecture.md §3.
// ---------------------------------------------------------------------------

/// Spin up a minimal mock HTTP server on a random port, return the port.
/// The server responds 200 OK with a fixed JSON body to any POST /v1/stores.
fn start_mock_daemon() -> (std::net::TcpListener, u16) {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// Daemon-routing: `store add` is routed to the HTTP API when daemon is running.
///
/// We create the `daemon.sock` sentinel file (the probe_daemon() check),
/// start a mock HTTP server, and verify that `store add` forwards the request
/// to it (rather than writing directly to the local DB).
#[test]
fn store_add_routes_to_daemon_when_running() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::thread;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Start mock HTTP server.
    let (listener, port) = start_mock_daemon();
    let received_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_paths_clone = received_paths.clone();

    thread::spawn(move || {
        // Accept one or more connections.
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            // Read the request line.
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_ok() {
                received_paths_clone
                    .lock()
                    .unwrap()
                    .push(request_line.trim().to_string());
            }

            // Drain headers.
            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }

            // Respond 200 OK.
            let body = r#"{"status":"ok","name":"daemon-store","id":"daemon-id-123"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    // Create daemon.sock sentinel — this is how probe_daemon() detects the daemon.
    // The base_url is overridden by writing the port into the sock file content
    // OR we need the probe to return the right port. Since probe_daemon currently
    // hardcodes port 7700, we use env var LOCALDB_DAEMON_URL to override it in tests.
    std::fs::write(
        data_dir.join("daemon.sock"),
        format!("http://127.0.0.1:{}", port),
    )
    .unwrap();

    // Run `store add` — it should route to the mock daemon.
    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "store", "add", "daemon-store"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The daemon mock returned {"status":"ok",...} so the CLI should succeed.
    assert!(
        output.status.success(),
        "store add with daemon running should succeed (routed to mock); \
         exit={:?} stderr={} stdout={}",
        output.status.code(),
        stderr,
        stdout,
    );

    // Verify the mock received a request to /v1/stores.
    let paths = received_paths.lock().unwrap();
    assert!(
        !paths.is_empty(),
        "mock HTTP daemon should have received at least one request from 'store add'"
    );
    assert!(
        paths.iter().any(|p| p.contains("/v1/stores")),
        "daemon routing must POST to /v1/stores; received: {:?}",
        paths
    );
}

/// Daemon-routing: `store remove` routes to daemon when running.
#[test]
fn store_remove_routes_to_daemon_when_running() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::thread;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (listener, port) = start_mock_daemon();
    let received_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_paths_clone = received_paths.clone();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_ok() {
                received_paths_clone
                    .lock()
                    .unwrap()
                    .push(request_line.trim().to_string());
            }

            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }

            // 200 for remove.
            let body = r#"{"status":"ok","name":"mystore"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    std::fs::write(
        data_dir.join("daemon.sock"),
        format!("http://127.0.0.1:{}", port),
    )
    .unwrap();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "--yes", "store", "remove", "mystore"])
        .output()
        .unwrap();

    let paths = received_paths.lock().unwrap();
    assert!(
        !paths.is_empty(),
        "mock HTTP daemon should have received a request from 'store remove'"
    );
    assert!(
        paths.iter().any(|p| p.contains("/v1/stores")),
        "daemon routing must target /v1/stores; received: {:?}",
        paths
    );

    // Exit 0 (routed to daemon which returned 200) or exit 3/4/5 if daemon
    // returned an error — either way, it must have *contacted* the daemon.
    let _ = output.status.code(); // just check it ran
}

/// Daemon-routing: `search` routes to daemon when running.
#[test]
fn search_routes_to_daemon_when_running() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::thread;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (listener, port) = start_mock_daemon();
    let received_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_paths_clone = received_paths.clone();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_ok() {
                received_paths_clone
                    .lock()
                    .unwrap()
                    .push(request_line.trim().to_string());
            }

            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }

            // Drain body if any (POST /v1/search sends a body).
            let body_resp = r#"{"citations":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body_resp.len(),
                body_resp
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    std::fs::write(
        data_dir.join("daemon.sock"),
        format!("http://127.0.0.1:{}", port),
    )
    .unwrap();

    let _output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "search", "hello world"])
        .output()
        .unwrap();

    let paths = received_paths.lock().unwrap();
    assert!(
        !paths.is_empty(),
        "mock HTTP daemon should have received a request from 'search'"
    );
    assert!(
        paths.iter().any(|p| p.contains("/v1/search")),
        "daemon routing must POST to /v1/search; received: {:?}",
        paths
    );
}

/// Daemon-routing: `source add` routes to daemon without panicking.
///
/// Regression test for issue #53: `source add` used the sync `daemon_request`
/// wrapper from inside an already-running tokio runtime, causing a nested
/// `block_on` panic. This test verifies that the command reaches the mock
/// daemon (proving the async path is exercised) and does NOT panic.
#[test]
fn source_add_routes_to_daemon_without_panic() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::thread;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (listener, port) = start_mock_daemon();
    let received_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_paths_clone = received_paths.clone();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_ok() {
                received_paths_clone
                    .lock()
                    .unwrap()
                    .push(request_line.trim().to_string());
            }

            // Drain headers.
            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }

            // Respond with a plausible source-created payload.
            let body = r#"{"id":"01ABCDEFGHIJKLMNOPQRSTUVWX","store":"mystore","kind":"path"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    std::fs::write(
        data_dir.join("daemon.sock"),
        format!("http://127.0.0.1:{}", port),
    )
    .unwrap();

    // First create a store so that store-validation passes in the CLI before
    // the daemon probe (store-add itself will also be routed, that's fine).
    // We use the mock daemon for everything — no real DB needed.
    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "source", "add", "--store", "mystore", "."])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The critical invariant: the process must NOT have panicked.
    // A panic exits with a non-zero status AND prints "panicked at" to stderr.
    assert!(
        !stderr.contains("panicked at"),
        "source add must not panic (nested block_on regression); stderr: {}",
        stderr
    );

    // The mock returned 200 with a valid source-like body, so the CLI should
    // have succeeded (or possibly exited non-zero for other reasons, e.g.
    // the store validation happening client-side, but it must have reached the
    // daemon without panicking).
    let paths = received_paths.lock().unwrap();
    assert!(
        !paths.is_empty(),
        "mock HTTP daemon should have received a request from 'source add'; \
         exit={:?} stdout={} stderr={}",
        output.status.code(),
        stdout,
        stderr
    );
    assert!(
        paths.iter().any(|p| p.contains("/v1/stores")),
        "daemon routing from 'source add' must target /v1/stores/{{name}}/sources; \
         received: {:?}",
        paths
    );
}

/// Daemon-routing: `source remove` converted to async does not panic.
///
/// Regression test for issue #53: `source remove` was refactored from sync
/// (calling the sync `daemon_request` wrapper) to async (calling
/// `daemon_request_async(..).await`).  When `source remove` is invoked with a
/// daemon running and `--store` given but the store is not in the runtime DB,
/// the CLI should exit with a structured error (exit 3), NOT with a panic.
///
/// Note: `source remove` exits before reaching the daemon in this scenario due
/// to the D1 store-existence check (the temp placeholder DB opened in daemon
/// mode is empty).  The key invariant is no panic.
#[test]
fn source_remove_with_daemon_running_exits_cleanly_without_panic() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Create daemon.sock sentinel pointing to a (potentially non-existent) port.
    // probe_daemon_health will return false (no listener), so probe_daemon()
    // falls back to DaemonState::NotRunning after removing the stale sock.
    // We use LOCALDB_DAEMON_URL to force daemon-mode detection instead.
    std::fs::write(data_dir.join("daemon.sock"), "http://127.0.0.1:19999").unwrap();

    // With LOCALDB_DAEMON_URL set and no default store, source remove must exit
    // with a non-panic error (exit 2 "no stores" because the placeholder DB is
    // empty).  It must NOT panic with "Cannot start a runtime from within a
    // runtime."
    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", "http://127.0.0.1:19999")
        .args(["--json", "source", "remove", "01ABCDEFGHIJKLMNOPQRSTUVWX"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Must not panic — this is the regression guard for issue #53.
    assert!(
        !stderr.contains("panicked at"),
        "source remove must not panic even when daemon is running; \
         exit={:?} stdout={} stderr={}",
        output.status.code(),
        stdout,
        stderr
    );

    // The process must exit non-zero (structured error, not panic/abort).
    assert!(
        !output.status.success(),
        "source remove with no stores and daemon running should not succeed"
    );
}

// ---------------------------------------------------------------------------
// Regression guard for #67 — concurrent DB access no longer fails
//
// Previously, holding the redb handle open in-process (e.g. by a daemon or
// MCP server) would prevent the CLI from opening the same DB file, causing
// exit 4 with `runtime_state_locked`. With SQLite WAL mode each operation
// opens a short-lived connection; multiple concurrent openers are fine.
// ---------------------------------------------------------------------------

/// Regression guard for #67: CLI commands succeed even when another libsql
/// connection is already open on the same DB file.
#[tokio::test]
async fn store_list_succeeds_while_db_held_open_by_another_connection() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    // A zero-store DB now exits 2 (specs/05-surfaces.md §2.2), so a store
    // must exist for this to exercise the "succeeds despite the held-open
    // connection" behavior rather than the unrelated no-stores exit.
    cmd_with_dir(&dir)
        .args(["store", "add", "mystore"])
        .assert()
        .success();

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Open a libsql connection and keep it alive (simulates another
    // process — e.g. the MCP server — that has the DB open).
    let state_db_path = data_dir.join("localdb.db");
    let _holder_db = libsql::Builder::new_local(&state_db_path)
        .build()
        .await
        .expect("should be able to open localdb.db");
    let _holder_conn = _holder_db.connect().expect("should be able to connect");

    // `store list --json` must exit 0 (success), not 4 (locked).
    let output = cmd_with_dir(&dir)
        .args(["--json", "store", "list"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "store list should succeed while DB is held open by another connection; \
         exit={:?} stderr={} stdout={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
}

/// Regression guard for #67: two concurrent `store list` CLI processes both exit 0.
#[test]
fn two_concurrent_store_list_calls_both_succeed() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    // A zero-store DB now exits 2 (specs/05-surfaces.md §2.2); a store must
    // exist so this test still exercises the concurrent-access behavior.
    cmd_with_dir(&dir)
        .args(["store", "add", "mystore"])
        .assert()
        .success();

    // Run two store-list commands at the same time (non-blocking spawn).
    // Both must point at the same temp config so they share the same localdb.db.
    let config_path = dir.path().join("config.yaml");
    let binary = env!("CARGO_BIN_EXE_localdb");

    let mut child1 = std::process::Command::new(binary)
        .env("LOCALDB_CONFIG", &config_path)
        .args(["store", "list"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child1");
    let mut child2 = std::process::Command::new(binary)
        .env("LOCALDB_CONFIG", &config_path)
        .args(["store", "list"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child2");

    let s1 = child1.wait().expect("wait child1");
    let s2 = child2.wait().expect("wait child2");

    assert!(s1.success(), "first store list failed: {:?}", s1.code());
    assert!(s2.success(), "second store list failed: {:?}", s2.code());
}

/// With a minimal valid config (version: 1 + temp data dir, no `stores:` key, no embedder
/// policy), `store list` must load config via the lenient path without an *invalid config*
/// failure — that's what this test guards (F1-cli). It used to also assert exit 0 with an
/// empty store list, but the project is moving toward implicit init (a `default` store
/// auto-created idempotently), so a database with zero stores is now a deliberate loud
/// failure under the all-stores scope policy (specs/05-surfaces.md §2.2), not a silent
/// empty-list success. Since a config-load failure is *also* exit 2, the exit code alone
/// can no longer distinguish "config was fine, there just aren't any stores" from "config
/// itself was rejected" — so this asserts on the stderr message instead, proving the
/// lenient-config path succeeded and the "no stores" branch is what actually fired.
#[test]
fn store_list_with_minimal_config_and_no_stores_exits_2_with_no_stores_message() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    // Minimal config: version + fresh data dir only — no `stores:` key, no embedder config.
    let config = format!(
        "version: 1\npaths:\n  data: {}\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
    let output = cmd_with_dir(&dir).args(["store", "list"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "store list with zero stores should exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("no stores"),
        "expected the no-stores message (proving the minimal config loaded fine via the \
         lenient path, rather than failing as invalid config); stderr: {stderr}"
    );
    assert!(
        !stderr.contains("invalid config"),
        "the minimal config must not be rejected as invalid; stderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Finding B — Reject refresh intervals on path sources
// ---------------------------------------------------------------------------

#[test]
fn source_add_refresh_on_path_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "notes"])
        .assert()
        .success();

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "notes",
            "source",
            "add",
            "--refresh",
            "1h",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "source add --refresh on a path source should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

// ---------------------------------------------------------------------------
// Finding A — Persist store policy on auto-index
// ---------------------------------------------------------------------------

#[tokio::test]
async fn source_add_auto_index_updates_store_policy_version() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let config_d1 = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config_d1).unwrap();

    cmd_with_dir(&dir)
        .args(["store", "add", "notes"])
        .assert()
        .success();

    let docs1 = dir.path().join("docs1");
    std::fs::create_dir_all(&docs1).unwrap();
    std::fs::write(docs1.join("first.md"), "# First\n\nFirst document.\n").unwrap();

    cmd_with_dir(&dir)
        .args(["--store", "notes", "source", "add", docs1.to_str().unwrap()])
        .assert()
        .success();

    let db_path = data_dir.join("localdb.db");
    let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn
        .query(
            "SELECT policy_version FROM stores WHERE name = ?",
            libsql::params!["notes".to_string()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let v1: String = row.get(0).unwrap();
    drop(rows);
    drop(conn);
    drop(db);

    let config_d2 = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n    parsers:\n      - pdf\n      - html\n      - markdown\n      - plaintext\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config_d2).unwrap();

    let docs2 = dir.path().join("docs2");
    std::fs::create_dir_all(&docs2).unwrap();
    std::fs::write(docs2.join("second.md"), "# Second\n\nSecond document.\n").unwrap();

    cmd_with_dir(&dir)
        .args(["--store", "notes", "source", "add", docs2.to_str().unwrap()])
        .assert()
        .success();

    let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn
        .query(
            "SELECT policy_version FROM stores WHERE name = ?",
            libsql::params!["notes".to_string()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let v2: String = row.get(0).unwrap();
    drop(rows);

    assert_ne!(
        v1,
        v2,
        "policy_version should be updated after source add with changed indexing policy; v1={v1}, v2={v2}"
    );
}

// ---------------------------------------------------------------------------
// db status / db migrate / db downgrade — specs/05-surfaces.md §2.1
//
// These commands must resolve (db path, embedding shape) from config alone,
// never through `AppDb::open` (which refuses on the very version mismatch
// they exist to fix) and never by constructing an embedder. They must also
// refuse cleanly while a daemon is running, exactly like every other
// daemon-aware write command (`daemon_running`, exit 4) — unlike `store`/
// `source`, they never route to the daemon's HTTP API.
// ---------------------------------------------------------------------------

/// Stamp `PRAGMA user_version = version` on a raw db file at `path`,
/// bypassing any of the CLI's normal open paths — simulates a legacy
/// (pre-migration-framework) store for `db migrate` tests.
async fn stamp_user_version(path: &std::path::Path, version: i64) {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.query(&format!("PRAGMA user_version = {version}"), ())
        .await
        .unwrap();
}

#[test]
fn db_status_on_fresh_healthy_store_reports_current_equals_head() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // `store add` opens the store via the normal init path, creating the
    // schema fresh at this binary's head version.
    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "db", "status"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "db status should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("db status --json must emit valid JSON; got: {stdout}"));

    let current = v["current_version"].as_i64().unwrap();
    let head = v["head_version"].as_i64().unwrap();
    assert_eq!(current, head, "fresh store should be exactly at head");
    assert_eq!(
        current, 5,
        "current baseline/head is v5 (baseline v4 + the block_id-drop migration)"
    );
    assert_eq!(v["pending"].as_i64().unwrap(), 0);
    assert!(!v["legacy"].as_bool().unwrap());
}

/// `db status` on a missing store file exits 2 (invalid config), not a panic.
#[test]
fn db_status_missing_store_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir).args(["db", "status"]).output().unwrap();
    assert_eq!(output.status.code().unwrap(), 2);
}

/// Codex review #152 fix 1: an existing-but-uninitialized store (a zero-byte
/// file the user pointed at, `PRAGMA user_version` still 0) must be reported
/// distinctly, not folded into "up to date" just because `pending == 0`.
#[test]
fn db_status_on_uninitialized_store_reports_uninitialized_not_up_to_date() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("localdb.db");
    // A zero-byte file: `open_for_maintenance` only requires `path.is_file()`
    // to succeed, and a fresh/empty sqlite file reports `PRAGMA user_version`
    // == 0, exactly like the maintenance path's documented "fresh file"
    // case (see `migrate_store`'s `current == 0` branch).
    std::fs::File::create(&db_path).unwrap();

    let output = cmd_with_dir(&dir).args(["db", "status"]).output().unwrap();
    assert!(
        output.status.success(),
        "db status on an uninitialized store should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("uninitialized"),
        "stdout should mention the store is uninitialized: {stdout}"
    );
    assert!(
        !stdout.contains("up to date"),
        "an uninitialized store must not be reported as 'up to date': {stdout}"
    );

    let json_output = cmd_with_dir(&dir)
        .args(["--json", "db", "status"])
        .output()
        .unwrap();
    assert!(json_output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&json_output.stdout)).unwrap();
    assert_eq!(v["current_version"].as_i64().unwrap(), 0);
    assert!(
        v["uninitialized"].as_bool().unwrap(),
        "--json output should carry an explicit uninitialized flag: {v}"
    );
}

/// `db migrate` on a store already at head is a no-op and exits 0.
#[test]
fn db_migrate_noop_at_head() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["db", "migrate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already at head"));
}

/// `db migrate` on an at-head store whose migration bookkeeping has been
/// tampered with (a stored checksum no longer matches what the compiled
/// chain would produce) must fail loudly, not report "already at head".
///
/// This is the regression test for the bug where `run_db_migrate` decided
/// "already at head" from a read-only pre-inspect and returned *without*
/// ever calling `migrate_store` — skipping the checksum/bookkeeping
/// verification that `migrate_store`'s own no-op-at-head path performs.
/// Every other command refuses to open a store in this state; `db migrate`
/// is the one meant to fix/diagnose it, so it must go through the library
/// even when the pre-inspect says nothing looks pending.
#[test]
fn db_migrate_on_corrupted_at_head_store_fails_verification() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    let db_path = data_dir.join("localdb.db");

    // `store add` creates a fresh store at head (v4), seeding
    // schema_migrations with a valid baseline checksum.
    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    // Tamper with the baseline row's stored checksum directly, bypassing
    // every CLI path — simulates on-disk corruption or an out-of-band edit.
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE schema_migrations SET checksum = 'deadbeef' WHERE version = 4",
            (),
        )
        .await
        .unwrap();
    });

    let output = cmd_with_dir(&dir).args(["db", "migrate"]).output().unwrap();
    assert_ne!(
        output.status.code().unwrap(),
        0,
        "db migrate on a corrupted at-head store must not exit 0; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("checksum"),
        "stderr should surface the checksum-mismatch error: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("already at head"),
        "a corrupted at-head store must not be reported as 'already at head'; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// `db migrate` on a legacy (pre-baseline) store without confirmation
/// aborts (non-interactive + no `--yes` → exit 2 via `confirm_destructive`)
/// and leaves the store completely untouched.
#[test]
fn db_migrate_legacy_without_confirmation_aborts_and_leaves_store_untouched() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("localdb.db");

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(stamp_user_version(&db_path, 2));

    let output = cmd_with_dir(&dir).args(["db", "migrate"]).output().unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "declining (non-interactive, no --yes) must exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let version = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let v: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        v
    });
    assert_eq!(
        version, 2,
        "a refused legacy migrate must not touch the store"
    );
}

/// `db migrate --yes` on a legacy store rebuilds it to head.
#[test]
fn db_migrate_legacy_with_yes_rebuilds_to_head() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("localdb.db");

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(stamp_user_version(&db_path, 2));

    let output = cmd_with_dir(&dir)
        .args(["--json", "--yes", "db", "migrate"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "db migrate --yes on a legacy store should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["from_version"].as_i64().unwrap(), 2);
    assert!(v["legacy_rebuilt"].as_bool().unwrap());
    assert!(
        v["staleness_marked"].as_bool().unwrap(),
        "a legacy rebuild erases all indexed content, so JSON should carry \
         staleness_marked=true: {v}"
    );

    // Verify db status now reports a healthy at-head store.
    let status_output = cmd_with_dir(&dir)
        .args(["--json", "db", "status"])
        .output()
        .unwrap();
    let status: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&status_output.stdout)).unwrap();
    assert_eq!(status["current_version"], status["head_version"]);
    assert!(!status["legacy"].as_bool().unwrap());

    // Same scenario without `--json`: `migrate_store` now sets
    // `staleness_marked = true` for legacy rebuilds (a recent library
    // change), so the human-readable path must print the re-index hint —
    // verify the CLI's existing hint-printing code actually fires for it.
    let dir2 = TempDir::new().unwrap();
    write_default_config(&dir2);
    let data_dir2 = dir2.path().join("data");
    std::fs::create_dir_all(&data_dir2).unwrap();
    let db_path2 = data_dir2.join("localdb.db");
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(stamp_user_version(&db_path2, 2));

    let plain_output = cmd_with_dir(&dir2)
        .args(["--yes", "db", "migrate"])
        .output()
        .unwrap();
    assert!(
        plain_output.status.success(),
        "db migrate --yes on a legacy store should succeed; stderr: {}",
        String::from_utf8_lossy(&plain_output.stderr)
    );
    let plain_stdout = String::from_utf8_lossy(&plain_output.stdout);
    assert!(
        plain_stdout.contains("rebuilt legacy store"),
        "stdout: {plain_stdout}"
    );
    assert!(
        plain_stdout.contains("localdb index"),
        "a confirmed legacy rebuild should print the re-index hint: {plain_stdout}"
    );
}

/// Read `current_version` off a fresh store's `db status --json`, without
/// hardcoding it: the real migration chain (`store-libsql/src/migrations/
/// chain.rs`) grows over time, so a fresh store's head — and therefore its
/// current version — isn't a fixed literal across the codebase's lifetime.
fn fresh_store_current_version(dir: &TempDir) -> i64 {
    let output = cmd_with_dir(dir)
        .args(["--json", "db", "status"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("db status --json must emit valid JSON; got: {stdout}"));
    v["current_version"].as_i64().unwrap()
}

/// `db downgrade --to <current-version> --yes` has nothing to do; the
/// library's own "nothing to downgrade" `InvalidConfig` maps to exit 2.
///
/// Codex review #152 fix 2 reconciliation: before that fix, `--yes` skipped
/// `confirm_destructive`'s prompt entirely (it always returns `true` for
/// `--yes`) and the "nothing to downgrade" error only surfaced once
/// `downgrade_store` itself ran. After the fix, `run_db_downgrade_async`
/// pre-validates the target (via `validate_downgrade_target`, reusing the
/// library's own wording) *before* even reaching `confirm_destructive` — so
/// for this `--yes` case the error now arrives one step earlier, but the
/// exit code and message are unchanged; no assertion here needed updating.
#[test]
fn db_downgrade_nothing_to_do_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();
    let current = fresh_store_current_version(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--yes", "db", "downgrade", "--to", &current.to_string()])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "downgrading to the current version should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nothing to downgrade"),
        "stderr should surface the library's own message: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Codex review #152 fix 2, scenario (b): `--to <current>` on a fresh store
/// (already at head — nothing to downgrade), non-interactive and without
/// `--yes`. Before the fix this exited 2 with the generic "re-run with
/// --yes" refusal from `confirm_destructive`, because the impossible target
/// was only checked *after* the confirmation gate. After the fix, the CLI
/// pre-validates the target first and the real "nothing to downgrade" error
/// surfaces directly — the confirmation prompt is never reached.
///
/// (Formerly named `db_downgrade_without_confirmation_aborts`; renamed and
/// tightened to assert the actual message, not just the exit code, since the
/// message is exactly what this fix changes.)
#[test]
fn db_downgrade_to_current_version_without_confirmation_reports_real_error() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();
    let current = fresh_store_current_version(&dir);

    let output = cmd_with_dir(&dir)
        .args(["db", "downgrade", "--to", &current.to_string()])
        .output()
        .unwrap();

    assert_eq!(output.status.code().unwrap(), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nothing to downgrade"),
        "stderr should surface the real library error, not a generic refusal: {stderr}"
    );
    assert!(
        !stderr.contains("re-run with --yes"),
        "an impossible downgrade must not demand confirmation for an operation that can only \
         fail: {stderr}"
    );
}

/// Codex review #152 fix 2, scenario (a): an explicit `--to` below the
/// frozen baseline, non-interactive and without `--yes`, must be rejected by
/// `validate_downgrade_target` before `confirm_destructive` ever prompts.
///
/// This no longer uses the CLI's *default* (no `--to`) target to reach the
/// below-baseline case, unlike the original version of this test: the
/// default resolves to `current_version - 1`, which only lands below the
/// frozen baseline (v4) when `current_version == baseline_version` — true
/// for a fresh store back when the real migration chain was empty, but not
/// anymore. The chain's first entry
/// (`drop_chunks_block_id_and_retag_resource_metadata`) is `Down::Unsupported`,
/// so a real store can never legitimately be downgraded back down to
/// exactly the baseline in the first place — there is no CLI-reachable
/// store left for which the *default* target computation lands below
/// baseline. An explicit out-of-range `--to` exercises the same
/// `validate_downgrade_target` branch regardless of how the target was
/// derived.
#[test]
fn db_downgrade_explicit_target_below_baseline_without_confirmation_reports_real_error() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["db", "downgrade", "--to", "3"])
        .output()
        .unwrap();

    assert_eq!(output.status.code().unwrap(), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot downgrade below the frozen baseline"),
        "stderr should surface the real library error: {stderr}"
    );
    assert!(
        !stderr.contains("re-run with --yes"),
        "an impossible downgrade must not demand confirmation for an operation that can only \
         fail: {stderr}"
    );
}

/// All three `db` subcommands refuse with exit 4 (`daemon_running`) while a
/// daemon is running — per specs/05-surfaces.md §2.1 they are CLI-only and
/// never route to the daemon's HTTP API, unlike `store`/`source`/`search`.
#[test]
fn db_commands_refuse_while_daemon_running() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    for args in [
        vec!["db", "status"],
        vec!["db", "migrate"],
        vec!["--yes", "db", "migrate"],
        vec!["--yes", "db", "downgrade"],
    ] {
        let output = cmd_with_dir(&dir)
            .env("LOCALDB_DAEMON_URL", "http://127.0.0.1:19999")
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code().unwrap(),
            4,
            "`localdb {}` should exit 4 while daemon is running; stderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

// ---------------------------------------------------------------------------
// Multi-store `--store` scope — specs/05-surfaces.md §2.2, issue #178
//
// Before the fix, every command except `search`/`mcp` called a helper that
// used `ctx.stores.first()` and otherwise picked an ARBITRARY store
// (`list_stores()[0]`) when `--store` was omitted. These tests create three
// stores (`books`, `default`, `research`) and exercise the resolution rules
// in the §2.2 table: `-s` is repeatable, every name is validated and
// resolved (not just the first), unknown names are exit 3, and each
// command's no-`-s` default is deterministic rather than "whichever store
// sorts first".
// ---------------------------------------------------------------------------

/// Create three stores — `books`, `default`, `research` — each seeded with
/// one path source pointing at its own fixture directory (auto-indexed via
/// `source add`, so `index` has real, if trivial, work to do per store).
/// Returns each store's fixture directory so callers can assert on exact
/// paths rather than just counts.
fn setup_multi_store(dir: &TempDir) -> std::collections::HashMap<&'static str, std::path::PathBuf> {
    write_default_config(dir);
    let mut fixtures = std::collections::HashMap::new();
    for name in ["books", "default", "research"] {
        cmd_with_dir(dir)
            .args(["store", "add", name])
            .assert()
            .success();

        let fixture = dir.path().join(format!("{name}-docs"));
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(
            fixture.join("doc.md"),
            format!("# {name}\n\nDocument for store {name}.\n"),
        )
        .unwrap();

        cmd_with_dir(dir)
            .args(["--store", name, "source", "add", fixture.to_str().unwrap()])
            .assert()
            .success();

        fixtures.insert(name, fixture);
    }
    fixtures
}

/// Headline regression test for issue #178: `source list` with no `--store`
/// must NOT silently resolve to an arbitrary store (the pre-fix behavior
/// picked `list_stores()[0]`, which in practice was often not `default`).
/// It must deterministically resolve to the store named `default`
/// (specs/05-surfaces.md §2.2) — proven here by showing the bare invocation
/// differs from `-s books` and is identical to the explicit `-s default`.
#[test]
fn source_list_no_store_flag_is_default_store_not_arbitrary_178() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let bare = cmd_with_dir(&dir)
        .args(["--json", "source", "list"])
        .output()
        .unwrap();
    assert!(bare.status.success());
    let bare_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&bare.stdout)).unwrap();

    let explicit_books = cmd_with_dir(&dir)
        .args(["--json", "--store", "books", "source", "list"])
        .output()
        .unwrap();
    assert!(explicit_books.status.success());
    let books_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&explicit_books.stdout)).unwrap();

    // The headline #178 assertion: omitting --store must not be equivalent
    // to picking an arbitrary other store (here, `books`).
    assert_ne!(
        bare_v, books_v,
        "issue #178 regression: `source list` with no --store must not silently \
         resolve to an arbitrary store (e.g. `books`); got identical output: {bare_v}"
    );

    // And it must positively be the `default` store's view, not just "some
    // other store".
    let explicit_default = cmd_with_dir(&dir)
        .args(["--json", "--store", "default", "source", "list"])
        .output()
        .unwrap();
    assert!(explicit_default.status.success());
    let default_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&explicit_default.stdout)).unwrap();
    assert_eq!(
        bare_v, default_v,
        "no --store should resolve deterministically to the store named 'default'"
    );
}

/// `source add` with no `--store` lands in the store named `default`
/// (specs/05-surfaces.md §2.2), verified by re-listing that store's sources.
#[test]
fn source_add_no_store_flag_lands_in_default_store() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // each of books/default/research already has 1 source

    let fixture = dir.path().join("extra-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    cmd_with_dir(&dir)
        .args(["source", "add", fixture.to_str().unwrap()])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "--store", "default", "source", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let sources = v["sources"].as_array().expect("sources must be an array");
    assert_eq!(
        sources.len(),
        2,
        "default store should now hold its original source plus the new one: {v}"
    );
    assert!(
        sources
            .iter()
            .any(|s| s["root"].as_str() == Some(fixture.to_str().unwrap())),
        "the newly added source should be on 'default': {v}"
    );

    // The other two stores must be untouched by the bare `source add`.
    let books_output = cmd_with_dir(&dir)
        .args(["--json", "--store", "books", "source", "list"])
        .output()
        .unwrap();
    let books_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&books_output.stdout)).unwrap();
    assert_eq!(
        books_v["sources"].as_array().unwrap().len(),
        1,
        "books should be untouched by a bare `source add`: {books_v}"
    );
}

/// `source add` with no `--store` requires a store literally named `default`
/// — this fires even when exactly one store exists under a different name,
/// per specs/05-surfaces.md §2.2 ("predictability wins over guessing the
/// sole store").
#[test]
fn source_add_no_store_flag_exits_2_even_with_exactly_one_other_store() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .args(["store", "add", "onlystore"])
        .assert()
        .success();

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args(["source", "add", fixture.to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no store named 'default'; pass --store <name>"),
        "stderr: {stderr}"
    );
}

/// After `store remove default`, a bare `source add` (no `--store`) exits 2
/// and the message names `--store` — the store set genuinely has no
/// `default` member anymore, distinct from the "never had one" case above.
#[test]
fn source_add_no_store_flag_after_default_removed_exits_2() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    cmd_with_dir(&dir)
        .args(["store", "remove", "--yes", "default"])
        .assert()
        .success();

    let fixture = dir.path().join("orphan-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args(["source", "add", fixture.to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--store"),
        "error message should name --store; stderr: {stderr}"
    );
    assert!(
        stderr.contains("no store named 'default'"),
        "stderr: {stderr}"
    );
}

/// `source list` output gains a store-name column only when more than one
/// store is in scope (specs/05-surfaces.md §2.2); with exactly one store the
/// output is byte-identical to the pre-multi-store format.
#[test]
fn source_list_shows_store_column_only_when_multi_store_in_scope() {
    let dir = TempDir::new().unwrap();
    let fixtures = setup_multi_store(&dir);
    let books_fixture = fixtures.get("books").unwrap().to_str().unwrap();

    // Exactly one store in scope: no column.
    let single = cmd_with_dir(&dir)
        .args(["--store", "books", "source", "list"])
        .output()
        .unwrap();
    assert!(single.status.success());
    let single_stdout = String::from_utf8_lossy(&single.stdout);
    let single_lines: Vec<&str> = single_stdout.lines().collect();
    assert_eq!(single_lines.len(), 1, "stdout: {single_stdout}");
    assert!(
        single_lines[0].ends_with(&format!("[path] {books_fixture}")),
        "single-store line must be `{{id}} [path] {{loc}}` with no store column: {}",
        single_lines[0]
    );
    assert!(
        !single_lines[0].starts_with("books"),
        "single-store output must not carry a store-name column: {}",
        single_lines[0]
    );

    // More than one store in scope: a store-name column appears, padded to
    // the widest name in scope ("default", 7 chars) + 2 spaces — matching
    // the worked example in specs/05-surfaces.md §2.2.
    let multi = cmd_with_dir(&dir)
        .args(["--store", "books", "--store", "default", "source", "list"])
        .output()
        .unwrap();
    assert!(multi.status.success());
    let multi_stdout = String::from_utf8_lossy(&multi.stdout);
    let multi_lines: Vec<&str> = multi_stdout.lines().collect();
    assert_eq!(multi_lines.len(), 2, "stdout: {multi_stdout}");
    assert!(
        multi_lines.iter().any(|l| l.starts_with("books    ")),
        "expected a 'books' line padded to width 9: {multi_lines:?}"
    );
    assert!(
        multi_lines.iter().any(|l| l.starts_with("default  ")),
        "expected a 'default' line padded to width 9: {multi_lines:?}"
    );
}

/// `index` with no `--store` touches every store, not just the first —
/// verified via the multi-store `--json` shape (`{"stores": [...], "total":
/// {...}}`, specs/05-surfaces.md §2.2).
#[test]
fn index_no_store_flag_touches_all_stores() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index with no --store should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("index --json must emit valid JSON; got: {stdout}"));

    let stores = v["stores"]
        .as_array()
        .expect("multi-store index --json must have a 'stores' array");
    let names: std::collections::HashSet<&str> = stores
        .iter()
        .map(|s| s["store"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["books", "default", "research"].into_iter().collect(),
        "index with no --store should touch every store; got: {v}"
    );
    assert!(
        v.get("total").is_some(),
        "multi-store index --json must include a combined 'total': {v}"
    );
}

/// `db migrate` is not store-scoped (specs/05-surfaces.md §2.1/§2.2): passing
/// `--store` at all, even in a multi-store database, must exit 2 rather than
/// silently migrating (or being interpreted as migrating) just one store.
#[test]
fn db_migrate_with_store_flag_exits_2() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["db", "migrate", "--store", "books"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--store is not applicable"),
        "stderr: {stderr}"
    );
}

/// `-s`/`--store` is repeatable and every name is resolved, not truncated to
/// the first (the exact #178 failure mode for explicit multi-name usage):
/// `source list -s books -s research` must return sources from both stores.
#[test]
fn source_list_repeated_store_flags_returns_both_not_just_first() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args([
            "--json", "--store", "books", "--store", "research", "source", "list",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let sources = v["sources"].as_array().expect("sources must be an array");
    let store_names: std::collections::HashSet<&str> = sources
        .iter()
        .map(|s| s["store"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        store_names,
        ["books", "research"].into_iter().collect(),
        "repeated -s flags must resolve every name, not just the first: {v}"
    );
}

// -- Error branches on data-modifying paths (source add/remove, index) -----
// coverage gate: data-modifying paths must be >=90% (CLAUDE.md).

/// `source add --store <unknown>` exits 3 (store_not_found), even though the
/// implicit-default resolution would otherwise apply.
#[test]
fn source_add_unknown_store_exits_3() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);
    let fixture = dir.path().join("unknown-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "nosuchstore",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 3);
}

/// `source add --store ../evil` exits 2 (invalid/traversal store name),
/// rejected before any store lookup is attempted.
#[test]
fn source_add_traversal_store_name_exits_2() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);
    let fixture = dir.path().join("evil-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "../evil",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 2);
}

/// `source remove --store <unknown> <id>` exits 3.
#[test]
fn source_remove_unknown_store_exits_3() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "nosuchstore",
            "source",
            "remove",
            "01ABCDEFGHIJKLMNOPQRSTUVWX",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 3);
}

/// `source remove --store ../evil <id>` exits 2.
#[test]
fn source_remove_traversal_store_name_exits_2() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "../evil",
            "source",
            "remove",
            "01ABCDEFGHIJKLMNOPQRSTUVWX",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 2);
}

/// `index --store <unknown>` exits 3.
#[test]
fn index_unknown_store_exits_3() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--store", "nosuchstore", "index"])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 3);
}

/// `index --store ../evil` exits 2.
#[test]
fn index_traversal_store_name_exits_2() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--store", "../evil", "index"])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 2);
}

// ---------------------------------------------------------------------------
// Coverage gate: data-modifying paths (source.rs, index.rs) must be >=90%
// line coverage (specs/01-architecture.md §7 / CLAUDE.md). The tests below
// close gaps found via `cargo llvm-cov report --text` after the store-scope
// defaults rework (#178/#118/#144).
// ---------------------------------------------------------------------------

/// Requests recorded by [`start_recording_mock_server`] /
/// [`start_routing_mock_server`]: one `(start_line, json_body)` pair per
/// request received, in arrival order.
type RecordedRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// A single `(method, path_prefix, status_line, body)` route for
/// [`start_routing_mock_server`].
///
/// `method` matches the request's HTTP method exactly (e.g. `"GET"`,
/// `"POST"`), or matches *any* method when left `""`; `path_prefix` matches
/// via [`str::starts_with`] against the request's path **with its query
/// string still attached** (e.g. `"/v1/stores?cursor=20"`) — `""` matches
/// any path. A bare prefix with no `?` (e.g. `"/v1/stores"`) therefore still
/// matches every page of that resource regardless of `?cursor=`, exactly as
/// before; a prefix that includes a literal `?cursor=...` matches only that
/// specific page, which is how pagination-trap fixtures give page 1 and page
/// 2 of the same endpoint different bodies — list the cursor-specific route
/// before the bare-path fallback (first-match-wins). `body` is owned
/// (`String`, not `&'static str`) so callers can build it at runtime — e.g.
/// [`paginated_list_body`]/[`paginated_list_page`] for a `GET /v1/stores`
/// page — without resorting to `Box::leak`.
type MockRoute = (&'static str, &'static str, &'static str, String);

/// Fallback response served when no route matches: a 404 with a JSON error
/// body shaped like the daemon's real error envelope (`{"code": ...,
/// "message": ...}`, see `cli/src/daemon_client.rs::decode_daemon_error`),
/// so a test that forgets a route fails with a clear CLI-level error
/// instead of the mock server hanging or panicking.
const UNMATCHED_ROUTE_STATUS: &str = "HTTP/1.1 404 Not Found";
const UNMATCHED_ROUTE_BODY: &str =
    r#"{"code":"resource_not_found","message":"no mock route matched this request"}"#;

/// Spin up a minimal mock HTTP server that dispatches each request to the
/// first route in `routes` whose method matches (exactly, or any method if
/// `""`) and whose path starts with `path_prefix` — **first-match-wins**,
/// so callers should list more specific routes (e.g. an exact path) before
/// more general ones (e.g. a shared prefix or a catch-all `("", "", ..,
/// ..)`). Requests matching no route get [`UNMATCHED_ROUTE_STATUS`] /
/// [`UNMATCHED_ROUTE_BODY`] rather than hanging.
///
/// Every request's start-line and raw JSON body (if any) is recorded for
/// assertions, mirroring `start_recording_mock_server`. `routes` is taken
/// by value (rather than `&'static [MockRoute]`) so callers can build it
/// from ordinary runtime `&'static str` arguments (as
/// `start_recording_mock_server` does) without needing const-promotion or
/// leaking memory.
fn start_routing_mock_server(routes: Vec<MockRoute>) -> (u16, RecordedRequests) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};
    use std::thread;

    let (listener, port) = start_mock_daemon();
    let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line.trim().to_string();

            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body_buf = vec![0u8; content_length];
            let req_body = if content_length > 0 && reader.read_exact(&mut body_buf).is_ok() {
                String::from_utf8_lossy(&body_buf).to_string()
            } else {
                String::new()
            };

            received_clone
                .lock()
                .unwrap()
                .push((path.clone(), req_body));

            // The recorded `path` is the whole trimmed request line, e.g.
            // `"GET /v1/stores?limit=50 HTTP/1.1"`; pull out method + the
            // path *with its query string still attached* for route
            // matching (see `MockRoute`'s doc comment — this is what lets a
            // cursor-specific route prefix match only that page).
            let mut parts = path.split_whitespace();
            let req_method = parts.next().unwrap_or("");
            let req_path = parts.next().unwrap_or("");

            let (status_line, body) = routes
                .iter()
                .find(|(method, prefix, _, _)| {
                    (method.is_empty() || *method == req_method) && req_path.starts_with(prefix)
                })
                .map(|(_, _, status_line, body)| (*status_line, body.clone()))
                .unwrap_or((UNMATCHED_ROUTE_STATUS, UNMATCHED_ROUTE_BODY.to_string()));

            let response = format!(
                "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                status_line,
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    (port, received)
}

/// Spin up a minimal mock HTTP server that answers every request with the
/// same fixed status line + JSON body, recording each request's start-line
/// and raw JSON body (if any) for assertions. Unlike `start_mock_daemon`'s
/// inline callers above, this variant also captures the request body so
/// tests can assert on what the CLI actually sent (e.g. the `spec` object
/// for url-kind sources).
///
/// A thin wrapper over [`start_routing_mock_server`] with a single
/// catch-all route (`""`/`""`) matching any method and any path.
fn start_recording_mock_server(
    status_line: &'static str,
    body: &'static str,
) -> (u16, RecordedRequests) {
    start_routing_mock_server(vec![("", "", status_line, body.to_string())])
}

/// Build a `PaginatedList` JSON body (`server/src/handlers/mod.rs`) with no
/// further pages, for stubbing routes like `GET /v1/stores` in
/// [`start_routing_mock_server`] tests.
fn paginated_list_body(items_json: &[&str]) -> String {
    format!(
        r#"{{"items":[{}],"next_cursor":null,"total":{}}}"#,
        items_json.join(","),
        items_json.len()
    )
}

/// Like [`paginated_list_body`], but with an explicit `next_cursor` (`None`
/// renders `null`) and `total` — for building a *page* of a larger list, to
/// drive the pagination-trap tests (a match sitting on page 2+, or a scope
/// with more than `default_limit()` (20) items).
fn paginated_list_page(items_json: &[String], next_cursor: Option<&str>, total: usize) -> String {
    let cursor_json = match next_cursor {
        Some(c) => format!("\"{c}\""),
        None => "null".to_string(),
    };
    format!(
        r#"{{"items":[{}],"next_cursor":{},"total":{}}}"#,
        items_json.join(","),
        cursor_json,
        total
    )
}

/// One `StoreRecord` (`server/src/state.rs`) JSON object, for
/// [`paginated_list_body`]/[`paginated_list_page`] fixtures stubbing
/// `GET /v1/stores`.
fn store_record_json(name: &str) -> String {
    format!(r#"{{"name":"{name}","visibility":"private","backend":"libsql"}}"#)
}

/// One `SourceRecord` (`server/src/state.rs`) JSON object, for
/// [`paginated_list_body`]/[`paginated_list_page`] fixtures stubbing
/// `GET /v1/stores/{name}/sources`. Only `id` is inspected by the CLI's
/// owner-walk (`cli/src/cmds/index.rs::daemon_store_has_source`), but the
/// rest of the shape is filled in so the body is a valid `SourceRecord`.
fn source_record_json(id: &str, store_name: &str) -> String {
    format!(
        r#"{{"id":"{id}","store_id":"{store_name}","kind":"path","spec":{{"root":"/tmp/x"}},"preset":"prose","refresh":null}}"#
    )
}

// -- source add: local (non-daemon) error/success branches -----------------

/// `source add <nonexistent path>` exits 2 (`normalize_path_source` fails).
#[test]
fn source_add_nonexistent_path_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let missing = dir.path().join("does-not-exist-at-all");
    let output = cmd_with_dir(&dir)
        .args(["--store", "s1", "source", "add", missing.to_str().unwrap()])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "adding a nonexistent path should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `source add --refresh <garbage>` exits 2 (`validate_refresh_interval`
/// fails) before the source row is ever created.
#[test]
fn source_add_invalid_refresh_interval_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "s1",
            "source",
            "add",
            "--refresh",
            "not-a-duration",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "invalid --refresh value should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `source add <url>` (no daemon) creates a url-kind source locally. The
/// target host refuses the connection immediately (nothing listens on
/// 127.0.0.1:1), so the WarnAndContinue auto-index step fails quietly — the
/// command itself must still succeed and the source must be persisted with
/// `kind: url`.
#[test]
fn source_add_url_kind_local_creates_url_source() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "webstore"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "webstore",
            "source",
            "add",
            "http://127.0.0.1:1/doc.txt",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "adding a url source should succeed even if the fetch later fails; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["kind"].as_str().unwrap(), "url");

    let list = cmd_with_dir(&dir)
        .args(["--json", "--store", "webstore", "source", "list"])
        .output()
        .unwrap();
    let lv: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&list.stdout)).unwrap();
    let sources = lv["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["kind"].as_str().unwrap(), "url");
    assert_eq!(
        sources[0]["url"].as_str().unwrap(),
        "http://127.0.0.1:1/doc.txt"
    );
    assert!(sources[0]["root"].is_null());
}

/// A source root that becomes unreadable between `source add` and its
/// auto-index step surfaces as a warning (WarnAndContinue mode), not a
/// command failure: `run_source_ingestion` returns `Err`, which
/// `run_embedded_index_with` folds into the summary and reports via
/// `eprintln!` rather than propagating.
#[test]
#[cfg(unix)]
fn source_add_auto_index_permission_denied_warns_but_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "permstore"])
        .assert()
        .success();

    let fixture = dir.path().join("perm-docs");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("note.md"), "# Note\n\nhello\n").unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o000)).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "permstore",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // Restore permissions immediately so `TempDir`'s Drop can clean up even
    // if an assertion below fails.
    let _ = std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755));

    assert!(
        output.status.success(),
        "source add should still succeed; auto-index errors only warn. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: auto-index error for source"),
        "expected an auto-index warning in stderr; got: {stderr}"
    );
}

// -- source add: daemon-routing branches ------------------------------------

/// `source add <url>` with a daemon running, non-`--json`: exercises the
/// url-kind `spec` shape (`{"url": ...}`) and the plain-text success print
/// (`Added source ... (via daemon)`), both cold in the pre-existing
/// `source_add_routes_to_daemon_without_panic` test (which only used
/// `--json` and a path source).
#[test]
fn source_add_daemon_url_kind_non_json_prints_and_sends_url_spec() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body = paginated_list_body(&[&store_record_json("mystore")]);
    let add_body = r#"{"id":"01ABCDEFGHIJKLMNOPQRSTUVWX","store":"mystore","kind":"url"}"#;
    let (port, received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        (
            "POST",
            "/v1/stores/mystore/sources",
            "HTTP/1.1 200 OK",
            add_body.to_string(),
        ),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args([
            "source",
            "add",
            "--store",
            "mystore",
            "https://example.com/page",
        ])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "daemon-routed source add should succeed; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("Added source 01ABCDEFGHIJKLMNOPQRSTUVWX to store 'mystore' (via daemon)"),
        "expected non-json daemon success line; got: {stdout}"
    );

    let reqs = received.lock().unwrap();
    let (path, req_body) = reqs
        .iter()
        .find(|(line, _)| line.starts_with("POST"))
        .expect("mock daemon should have received the POST /v1/stores/mystore/sources request");
    assert!(path.contains("/v1/stores/mystore/sources"), "path: {path}");
    let body_json: serde_json::Value = serde_json::from_str(req_body).unwrap();
    assert_eq!(body_json["kind"].as_str().unwrap(), "url");
    assert_eq!(
        body_json["spec"]["url"].as_str().unwrap(),
        "https://example.com/page"
    );
}

/// `source add --store 'a#b'` (daemon-routed): the store name must be
/// percent-encoded into the URL path segment, not interpolated raw via
/// `format!`. Before the fix, `format!("{base_url}/v1/stores/{store_name}/sources")`
/// with `store_name = "a#b"` builds a URL whose path is `/v1/stores/a` with
/// fragment `b/sources` — the fragment is client-side-only and never reaches
/// the server, so the POST silently hits the wrong endpoint (finding 1).
#[test]
fn source_add_daemon_percent_encodes_store_name_with_fragment_char() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body = paginated_list_body(&[&store_record_json("a#b")]);
    let add_body = r#"{"id":"01ABCDEFGHIJKLMNOPQRSTUVWX","store":"a#b","kind":"path"}"#;
    let (port, received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        (
            "POST",
            "/v1/stores/a%23b/sources",
            "HTTP/1.1 200 OK",
            add_body.to_string(),
        ),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "source", "add", "--store", "a#b", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "daemon-routed source add with a fragment-char store name should still reach the right \
         endpoint; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let reqs = received.lock().unwrap();
    assert!(
        reqs.iter()
            .any(|(line, _)| line.starts_with("POST /v1/stores/a%23b/sources")),
        "expected the POST to target the percent-encoded path segment \
         '/v1/stores/a%23b/sources', not be silently truncated at the raw '#'; got: {:?}",
        reqs
    );
}

/// `source add` with a daemon running that responds with an error status:
/// the CLI must map the error body to the matching exit code (3 for
/// `store_not_found`), exercising the `Err(e) => exit_err(...)` arm.
#[test]
fn source_add_daemon_error_response_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let body = r#"{"code":"store_not_found","message":"no such store"}"#;
    let (port, _received) = start_recording_mock_server("HTTP/1.1 404 Not Found", body);

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args([
            "--json",
            "source",
            "add",
            "--store",
            "nosuchstore",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        3,
        "daemon store_not_found error should exit 3; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// -- source add: daemon-routed default-store distinction (finding 4) --------
//
// `resolve_daemon_store_scope` (`cli/src/app_db.rs`) must preserve the same
// implicit-vs-explicit `default` distinction embedded mode already has: an
// *implicit* `default` (no `--store` given) missing from the daemon's store
// set is `invalid_request`, exit 2; an *explicit* `--store default` missing
// is `store_not_found`, exit 3, the same as any other explicit unknown name.
// Collapsing these two into one case was the reviewer's framing error.

/// `source add` with `--store` omitted and a daemon whose store set has no
/// `default` member: exit 2 with the exact embedded-mode message, not exit 3.
#[test]
fn source_add_daemon_implicit_default_missing_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // The daemon knows about a store, just not one named "default".
    let stores_body = paginated_list_body(&[&store_record_json("other")]);
    let (port, received) =
        start_routing_mock_server(vec![("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body)]);

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["source", "add", fixture.to_str().unwrap()])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "an implicit default missing from the daemon's store set must exit 2, not 3; stderr: {stderr}"
    );
    assert!(
        stderr.contains("no store named 'default'; pass --store <name>"),
        "stderr: {stderr}"
    );

    // No POST should ever fire: pre-flight scope resolution must fail before
    // any mutating request.
    let reqs = received.lock().unwrap();
    assert!(
        !reqs.iter().any(|(l, _)| l.starts_with("POST")),
        "no source-add POST should fire when scope resolution itself fails; got: {:?}",
        reqs
    );
}

/// `source add --store default` (explicit) against a daemon whose store set
/// has no `default` member: exit 3 `store_not_found`, same as any other
/// explicit unknown name — distinct from the implicit-omission case above.
#[test]
fn source_add_daemon_explicit_default_missing_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body = paginated_list_body(&[&store_record_json("other")]);
    let (port, received) =
        start_routing_mock_server(vec![("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body)]);

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args([
            "source",
            "add",
            "--store",
            "default",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        3,
        "an explicit --store default absent from the daemon's store set must exit 3, not 2; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reqs = received.lock().unwrap();
    assert!(
        !reqs.iter().any(|(l, _)| l.starts_with("POST")),
        "no source-add POST should fire when scope resolution itself fails; got: {:?}",
        reqs
    );
}

// -- source list: empty-scope messages --------------------------------------

/// `source list` on a single, empty store prints the single-store message.
#[test]
fn source_list_single_store_empty_prints_singular_message() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "empty1"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["--store", "empty1", "source", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No sources on store 'empty1'."));
}

/// `source list` across more than one empty store prints the plural,
/// scope-wide message rather than naming any single store.
#[test]
fn source_list_multi_store_empty_prints_scope_message() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "empty1"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["store", "add", "empty2"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["--store", "empty1", "--store", "empty2", "source", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No sources in scope."));
}

// -- source remove: local (non-daemon) branches ------------------------------

/// `source remove <path>` with no `--store` and no daemon running exits 2
/// (D3: a path/url argument can't fall back to the implicit default store).
#[test]
fn source_remove_path_no_store_flag_exits_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let output = cmd_with_dir(&dir)
        .args(["source", "remove", "/some/fake/path"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "source remove by path with no --store should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires --store"),
        "expected the requires---store message; got: {stderr}"
    );
}

/// `source remove <ulid>` (single match) succeeds and prints the single-line
/// non-json format; the source is actually gone afterwards.
#[test]
fn source_remove_by_ulid_success_prints_removed_and_deletes() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "rs1"])
        .assert()
        .success();

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let add_out = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "rs1",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let add_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&add_out.stdout)).unwrap();
    let id = add_v["id"].as_str().unwrap().to_string();

    cmd_with_dir(&dir)
        .args(["--store", "rs1", "source", "remove", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("Removed source: {id}")));

    cmd_with_dir(&dir)
        .args(["--store", "rs1", "source", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No sources on store 'rs1'."));
}

/// `source remove --json <ulid>` (single match) prints the flat
/// `{"status": "ok", "id": ...}` shape.
#[test]
fn source_remove_by_ulid_json_output_shape() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "rs2"])
        .assert()
        .success();

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let add_out = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "rs2",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let add_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&add_out.stdout)).unwrap();
    let id = add_v["id"].as_str().unwrap().to_string();

    let output = cmd_with_dir(&dir)
        .args(["--json", "--store", "rs2", "source", "remove", &id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["status"].as_str().unwrap(), "ok");
    assert_eq!(v["id"].as_str().unwrap(), id);
}

/// `source remove <ulid>` for a ulid that simply doesn't exist locally
/// (`get_source` returns `Ok(None)`) exits 3 — distinct from the
/// `find_source_by_root_or_url` not-found path already covered by
/// `source_remove_not_found_exits_3`.
#[test]
fn source_remove_by_ulid_not_found_locally_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "rs3"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "rs3",
            "source",
            "remove",
            "01ABCDEFGHIJKLMNOPQRSTUVWX",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 3);
}

/// D2: a ulid that resolves to a real source, but whose owning store is not
/// in the resolved scope, is reported as not-found rather than leaking
/// cross-store existence.
#[test]
fn source_remove_by_ulid_store_not_in_scope_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "storeA"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["store", "add", "storeB"])
        .assert()
        .success();

    let fixture = dir.path().join("docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let add_out = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "storeA",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let add_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&add_out.stdout)).unwrap();
    let id = add_v["id"].as_str().unwrap().to_string();

    // The source belongs to storeA; scoping the remove to storeB only must
    // not find it.
    let output = cmd_with_dir(&dir)
        .args(["--store", "storeB", "source", "remove", &id])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "removing a ulid whose store is out of scope should exit 3 (not found); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `source remove <path>` scoped to two stores that both have a source at
/// that same path deletes both, printing one line per store (non-json,
/// `deleted.len() > 1` branch).
#[test]
fn source_remove_by_path_across_two_stores_deletes_both_text() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "m1"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["store", "add", "m2"])
        .assert()
        .success();

    let fixture = dir.path().join("shared-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    cmd_with_dir(&dir)
        .args(["--store", "m1", "source", "add", fixture.to_str().unwrap()])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["--store", "m2", "source", "add", fixture.to_str().unwrap()])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "m1",
            "--store",
            "m2",
            "source",
            "remove",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "removing a shared path across two stores should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("from store 'm1'") && stdout.contains("from store 'm2'"),
        "expected a per-store removal line for each store; got: {stdout}"
    );
}

/// Same scenario as above, but `--json`: verifies the `{"results": [...]}`
/// multi-delete shape.
#[test]
fn source_remove_by_path_across_two_stores_json_results_array() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "m1"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["store", "add", "m2"])
        .assert()
        .success();

    let fixture = dir.path().join("shared-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    cmd_with_dir(&dir)
        .args(["--store", "m1", "source", "add", fixture.to_str().unwrap()])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["--store", "m2", "source", "add", fixture.to_str().unwrap()])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "m1",
            "--store",
            "m2",
            "source",
            "remove",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["status"].as_str().unwrap(), "ok");
    let results = v["results"].as_array().expect("results must be an array");
    assert_eq!(results.len(), 2);
    let store_names: std::collections::HashSet<&str> = results
        .iter()
        .map(|r| r["store"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(store_names, ["m1", "m2"].into_iter().collect());
}

// -- source remove: daemon-routing success branch ----------------------------

/// `source remove <ulid>` with a daemon actually responding 200 (not just
/// unreachable, as the existing regression test uses): exercises the
/// `Ok(v)` success arm for both `--json` and plain-text output.
#[test]
fn source_remove_daemon_success_json_and_text() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let body = r#"{"status":"ok","id":"01ABCDEFGHIJKLMNOPQRSTUVWX"}"#;
    let (port, received) = start_recording_mock_server("HTTP/1.1 200 OK", body);
    let daemon_url = format!("http://127.0.0.1:{}", port);

    let json_out = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args(["--json", "source", "remove", "01ABCDEFGHIJKLMNOPQRSTUVWX"])
        .output()
        .unwrap();
    assert!(
        json_out.status.success(),
        "daemon-routed source remove --json should succeed; stderr: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let jv: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&json_out.stdout)).unwrap();
    assert_eq!(jv["id"].as_str().unwrap(), "01ABCDEFGHIJKLMNOPQRSTUVWX");

    let text_out = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args(["source", "remove", "01ABCDEFGHIJKLMNOPQRSTUVWX"])
        .output()
        .unwrap();
    assert!(text_out.status.success());
    let stdout = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        stdout.contains("Removed source: 01ABCDEFGHIJKLMNOPQRSTUVWX (via daemon)"),
        "expected non-json daemon removal line; got: {stdout}"
    );

    let reqs = received.lock().unwrap();
    assert_eq!(reqs.len(), 2, "expected exactly two DELETE requests");
    for (path, _) in reqs.iter() {
        assert!(
            path.starts_with("DELETE "),
            "expected a DELETE, got: {path}"
        );
        assert!(path.contains("/v1/sources/01ABCDEFGHIJKLMNOPQRSTUVWX"));
    }
}

// ---------------------------------------------------------------------------
// index.rs coverage gap-fills
// ---------------------------------------------------------------------------

/// `index --source <unknown-id>` (embedded, single store) exits 3 —
/// `run_embedded_index_with`'s `StrictExit`-mode `SourceNotFound` arm,
/// propagated through `run_index_async`'s `exit_err`.
#[test]
fn index_unknown_source_id_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args([
            "--store",
            "s1",
            "index",
            "--source",
            "01NOSUCHSOURCEIDXXXXXX",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "index --source <unknown> should exit 3; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `index` on a store with zero sources reports "no sources to index"
/// rather than an empty/zeroed summary.
#[test]
fn index_store_with_no_sources_reports_no_sources_message() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "emptystore"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["--store", "emptystore", "index"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No sources to index on store 'emptystore'.",
        ));
}

/// A source root that's unreadable at explicit-`index` time (as opposed to
/// at `source add` auto-index time) is a `StrictExit`-mode error: it's
/// counted, printed via the non-warn `eprintln!` arm, and — combined with
/// `--strict` — forces exit 2. No existing test exercised `--strict`'s
/// actual failure path at all.
#[test]
#[cfg(unix)]
fn index_permission_denied_root_with_strict_exits_2() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "permstore2"])
        .assert()
        .success();

    let fixture = dir.path().join("perm-docs2");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("note.md"), "# Note\n\nhello\n").unwrap();

    cmd_with_dir(&dir)
        .args([
            "--store",
            "permstore2",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .assert()
        .success();

    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o000)).unwrap();

    let output = cmd_with_dir(&dir)
        .args(["--store", "permstore2", "index", "--strict"])
        .output()
        .unwrap();

    let _ = std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755));

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "index --strict should exit 2 when a source root became unreadable; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error indexing source"),
        "expected the strict-mode error line in stderr; got: {stderr}"
    );
}

/// A source row with a preset that isn't a recognized chunker preset (only
/// reachable by writing the row directly — the CLI always writes
/// `preset: "prose"`, so this defends against rows created through another
/// surface, e.g. a future daemon API accepting an arbitrary preset) is
/// counted as an indexing error rather than panicking or aborting the run.
#[tokio::test]
async fn index_reports_error_for_source_with_invalid_chunker_preset() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    cmd_with_dir(&dir)
        .args(["store", "add", "presetstore"])
        .assert()
        .success();

    let data_dir = dir.path().join("data");
    let db_path = data_dir.join("localdb.db");
    let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
    let conn = db.connect().unwrap();

    let store_id: String = {
        let mut rows = conn
            .query(
                "SELECT id FROM stores WHERE name = ?",
                libsql::params!["presetstore".to_string()],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().expect("store row must exist");
        row.get(0).unwrap()
    };

    conn.execute(
        "INSERT INTO sources (id, store_id, kind, root, url, include, exclude, preset, refresh, created_at)
         VALUES (?1, ?2, 'path', ?3, NULL, '[]', '[]', ?4, NULL, ?5)",
        libsql::params![
            "01BOGUSPRESETSOURCEID0001".to_string(),
            store_id,
            "/nonexistent-root".to_string(),
            "not-a-real-preset".to_string(),
            "2024-01-01T00:00:00Z".to_string(),
        ],
    )
    .await
    .unwrap();
    drop(conn);
    drop(db);

    let output = cmd_with_dir(&dir)
        .args(["--store", "presetstore", "index"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid chunker preset"),
        "expected an invalid-chunker-preset error in stderr; got: {stderr}"
    );
}

/// `index` fails fast (exit 2, `InvalidConfig`) when the configured
/// embedding provider can't be constructed — e.g. `perplexity` with no
/// matching `providers:` block. This is the direct (non-daemon,
/// non-auto-index) embedder-build call in `run_index_async`, distinct from
/// the auto-index path's `warn_or_default!`-wrapped one.
///
/// The store here MUST have a real source. Since #180 review finding 2, the
/// embedder is built lazily — only once a store in scope actually has
/// sources to index — so a store with zero sources never touches the
/// embedder at all and this config would otherwise report "no sources to
/// index" (exit 0), not fail. Do not "simplify" this back to a bare
/// `store add` with no `source add`: that would silently stop exercising the
/// embedder-creation failure path this test exists to cover. The `source
/// add` step's own post-add auto-index runs in `WarnAndContinue` mode, so
/// the broken provider config only warns there — it does not fail the add
/// itself — leaving the plain `index` call below as the first place this
/// config is expected to hard-fail.
#[test]
fn index_embedder_creation_failure_exits_2() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: perplexity\n      model: pplx-embed-context-v1\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();

    cmd_with_dir(&dir)
        .args(["store", "add", "pstore"])
        .assert()
        .success();

    let fixture = dir.path().join("pstore-docs");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("doc.md"), "# Doc\n\nhello\n").unwrap();

    cmd_with_dir(&dir)
        .args([
            "--store",
            "pstore",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--store", "pstore", "index"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "index with an unconfigured provider should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// -- index: daemon-routing (run_daemon_index) --------------------------------

/// `index --json --source <id>` with a daemon running, single store in
/// scope: exercises the unwrapped single-submission JSON print and the
/// `source_id` field being folded into the request body.
///
/// Also covers finding 4: even with only one store in the resolved scope,
/// the CLI must still verify that store actually owns `source_id` via the
/// `GET /v1/stores/{name}/sources` owner walk before submitting — the old
/// single-store short circuit skipped this check entirely, so this fixture
/// deliberately makes the mock's source list the *authority* the id must be
/// found in (see `index_daemon_single_store_unknown_source_exits_3` below for
/// the negative case).
#[test]
fn index_daemon_single_store_json_includes_source_id() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Note: NOT created locally — the daemon (via the mock `GET /v1/stores`
    // route below) is the sole authority on store scope for this path
    // (finding 1), so the local DB is deliberately left empty.
    let stores_body = paginated_list_body(&[&store_record_json("onlystore")]);
    let sources_body = paginated_list_body(&[&source_record_json("src-123", "onlystore")]);
    let job_body = r#"{"id":"job-1","status":"queued"}"#;
    let (port, received) = start_routing_mock_server(vec![
        (
            "GET",
            "/v1/stores/onlystore/sources",
            "HTTP/1.1 200 OK",
            sources_body,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index", "--source", "src-123"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "daemon-routed index submission should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["id"].as_str().unwrap(), "job-1");
    assert!(
        v.get("jobs").is_none(),
        "single-store index --json must not wrap in a jobs array"
    );

    let reqs = received.lock().unwrap();
    // finding 4: a single-store scope no longer short-circuits the owner
    // walk — GET /v1/stores (scope resolution), GET
    // /v1/stores/onlystore/sources (ownership check), then POST /v1/jobs.
    assert_eq!(reqs.len(), 3, "unexpected requests: {:?}", reqs);
    let (path, req_body) = reqs
        .iter()
        .find(|(line, _)| line.starts_with("POST"))
        .expect("mock daemon should have received the POST /v1/jobs request");
    assert!(path.contains("/v1/jobs"), "path: {path}");
    let body_json: serde_json::Value = serde_json::from_str(req_body).unwrap();
    assert_eq!(body_json["store_name"].as_str().unwrap(), "onlystore");
    assert_eq!(body_json["source_id"].as_str().unwrap(), "src-123");
}

/// `index --source <unknown-id>` (daemon-routed, single store in scope) must
/// exit 3, matching embedded mode's `index_unknown_source_id_exits_3`
/// (finding 4). Before the fix, a single-store scope short-circuited straight
/// to submission with zero ownership verification — this exact case used to
/// exit 0 with `docs_attached: 0` instead.
#[test]
fn index_daemon_single_store_unknown_source_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body = paginated_list_body(&[&store_record_json("onlystore")]);
    let empty_sources = paginated_list_body(&[]);
    let (port, received) = start_routing_mock_server(vec![
        (
            "GET",
            "/v1/stores/onlystore/sources",
            "HTTP/1.1 200 OK",
            empty_sources,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index", "--source", "bogus-id"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "an unknown --source with a single store in scope must exit 3, matching embedded mode; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reqs = received.lock().unwrap();
    assert!(
        !reqs.iter().any(|(l, _)| l.starts_with("POST")),
        "no job should ever be submitted for an unverified source id; got: {:?}",
        reqs
    );
}

/// `index --json` with a daemon running and more than one store in scope:
/// wraps submissions into `{"jobs": [...], }`, each entry tagged with its
/// store name.
#[test]
fn index_daemon_multi_store_json_wraps_with_store_field() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body =
        paginated_list_body(&[&store_record_json("alpha"), &store_record_json("beta")]);
    let job_body = r#"{"id":"job-x","status":"queued"}"#;
    let (port, received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "daemon-routed multi-store index should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let jobs = v["jobs"].as_array().expect("jobs must be an array");
    assert_eq!(jobs.len(), 2);
    let store_names: std::collections::HashSet<&str> =
        jobs.iter().map(|j| j["store"].as_str().unwrap()).collect();
    assert_eq!(store_names, ["alpha", "beta"].into_iter().collect());

    let reqs = received.lock().unwrap();
    let post_reqs = reqs.iter().filter(|(l, _)| l.starts_with("POST")).count();
    assert_eq!(post_reqs, 2, "expected one POST per store; got: {:?}", reqs);
}

/// `index` (non-json) with a daemon running and a single store in scope
/// prints the plain "submitted to daemon" line without a store prefix.
#[test]
fn index_daemon_single_store_text_output() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body = paginated_list_body(&[&store_record_json("onlystore2")]);
    let job_body = r#"{"id":"job-2","status":"queued"}"#;
    let (port, _received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .arg("index")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Index job submitted to daemon: job-2 (poll with status)",
        ));
}

/// `index` (non-json) with a daemon running and more than one store in
/// scope prefixes each submission line with its store name.
#[test]
fn index_daemon_multi_store_text_output_prefixes_store_name() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body =
        paginated_list_body(&[&store_record_json("gamma"), &store_record_json("delta")]);
    let job_body = r#"{"id":"job-3","status":"queued"}"#;
    let (port, _received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .arg("index")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .contains("Index job submitted to daemon for store 'gamma': job-3 (poll with status)")
            && stdout.contains(
                "Index job submitted to daemon for store 'delta': job-3 (poll with status)"
            ),
        "expected a per-store submission line for each store; got: {stdout}"
    );
}

/// `index` with a daemon running that rejects every request (including the
/// scope-resolution `GET /v1/stores` call itself): the CLI must map the
/// error and exit non-zero.
#[test]
fn index_daemon_submission_error_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let body = r#"{"code":"store_not_found","message":"errstore"}"#;
    let (port, _received) = start_recording_mock_server("HTTP/1.1 404 Not Found", body);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "daemon job-submission error should exit 3; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// index.rs: daemon-routed scope resolution asks the daemon, not the local
// database (Codex review round 2, findings 1 & 2 — see
// cli/src/cmds/index.rs's `run_index_async`/`run_daemon_index` for the fixes
// these tests cover).
// ---------------------------------------------------------------------------

/// `index --store <name>` where the daemon knows the store but the local DB
/// does not: must succeed (finding 1). Before the fix, `run_index_async`
/// resolved `--store` against the local DB *before* ever probing the daemon,
/// so a daemon-valid, locally-unknown store was rejected `store_not_found`.
#[test]
fn index_daemon_explicit_store_known_to_daemon_not_local_succeeds() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    // Deliberately never created locally.

    let stores_body = paginated_list_body(&[&store_record_json("remote-only")]);
    let job_body = r#"{"id":"job-9","status":"queued"}"#;
    let (port, received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--store", "remote-only", "index"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "a --store the daemon knows (but the local DB does not) must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reqs = received.lock().unwrap();
    let (_, post_body) = reqs
        .iter()
        .find(|(l, _)| l.starts_with("POST"))
        .expect("a job should have been submitted");
    let body_json: serde_json::Value = serde_json::from_str(post_body).unwrap();
    assert_eq!(body_json["store_name"].as_str().unwrap(), "remote-only");
}

/// `index` with `--store` omitted against a daemon whose store set differs
/// entirely from the local DB: jobs must be submitted for the *daemon's*
/// stores, not the local database's (finding 1, omitted-flag half).
#[test]
fn index_daemon_omitted_store_uses_daemon_stores_not_local() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // The local DB has a store the daemon does not report...
    cmd_with_dir(&dir)
        .args(["store", "add", "local-only"])
        .assert()
        .success();

    // ...and the daemon reports a completely different one.
    let stores_body = paginated_list_body(&[&store_record_json("daemon-only")]);
    let job_body = r#"{"id":"job-10","status":"queued"}"#;
    let (port, received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reqs = received.lock().unwrap();
    let (_, post_body) = reqs
        .iter()
        .find(|(l, _)| l.starts_with("POST"))
        .expect("a job should have been submitted");
    let body_json: serde_json::Value = serde_json::from_str(post_body).unwrap();
    assert_eq!(
        body_json["store_name"].as_str().unwrap(),
        "daemon-only",
        "jobs must target the daemon's own store set, not the local DB's"
    );
}

/// A hostile/malformed daemon that returns a `GET /v1/stores` page whose
/// `next_cursor` never advances must not spin the CLI forever: the
/// non-advancing-cursor guard in `fetch_all_daemon_store_names`
/// (`cli/src/app_db.rs`) bails with `Error::Internal`, exit 1.
#[test]
fn index_daemon_store_scope_non_advancing_cursor_exits_1() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Page 1 (no cursor) claims a next page at cursor "5"; page 2
    // (?cursor=5) claims *another* page also at cursor "5" — a
    // non-advancing cursor a well-behaved daemon would never produce.
    let page1 = paginated_list_page(&[store_record_json("a")], Some("5"), 2);
    let page2 = paginated_list_page(&[store_record_json("b")], Some("5"), 2);
    let (port, _received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores?cursor=5", "HTTP/1.1 200 OK", page2),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", page1),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        1,
        "a non-advancing pagination cursor must exit 1 (Error::Internal), not hang; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A hostile/malformed daemon whose `GET /v1/stores` pagination *alternates*
/// between two cursors (`(none)->2->1->2->1->...`) must not spin the CLI
/// forever either. The naive guard this replaces only compared each new
/// cursor against the immediately-preceding one, so an alternating cycle
/// never tripped it — reproduced empirically as a genuine non-terminating
/// loop (finding 2) before this fix. A `.timeout()` bounds the test itself:
/// if the cursor-cycle guard regresses, this fails (killed, non-`Some(1)`
/// exit code) rather than hanging the whole suite.
#[test]
fn index_daemon_store_scope_alternating_cursor_exits_1() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // No-cursor page -> next "2"; cursor=2 page -> next "1"; cursor=1 page ->
    // next "2" again, closing the 1<->2 cycle without ever repeating the
    // *immediately preceding* cursor.
    let page_start = paginated_list_page(&[store_record_json("a")], Some("2"), 4);
    let page_at_2 = paginated_list_page(&[store_record_json("b")], Some("1"), 4);
    let page_at_1 = paginated_list_page(&[store_record_json("c")], Some("2"), 4);
    let (port, _received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores?cursor=2", "HTTP/1.1 200 OK", page_at_2),
        ("GET", "/v1/stores?cursor=1", "HTTP/1.1 200 OK", page_at_1),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", page_start),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .timeout(std::time::Duration::from_secs(15))
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "an alternating pagination cursor must exit 1 (Error::Internal), not hang; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `GET /v1/stores` itself must be paginated to exhaustion: an all-stores
/// scope with more than `default_limit()` (20) stores must include every
/// one of them, not just the first page.
#[test]
fn index_daemon_store_scope_paginates_over_20_stores() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let page1_names: Vec<String> = (0..20).map(|i| format!("store-{i:02}")).collect();
    let page1_items: Vec<String> = page1_names.iter().map(|n| store_record_json(n)).collect();
    let page1 = paginated_list_page(&page1_items, Some("20"), 21);
    let page2 = paginated_list_page(&[store_record_json("store-20")], None, 21);

    let job_body = r#"{"id":"job-page2","status":"queued"}"#;
    let (port, received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores?cursor=20", "HTTP/1.1 200 OK", page2),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", page1),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let jobs = v["jobs"].as_array().expect("jobs must be an array");
    assert_eq!(
        jobs.len(),
        21,
        "every store across both pages must be in the all-stores scope: {v}"
    );
    let submitted_names: std::collections::HashSet<&str> =
        jobs.iter().map(|j| j["store"].as_str().unwrap()).collect();
    assert!(
        submitted_names.contains("store-20"),
        "the store sitting on page 2 must not be dropped: {:?}",
        submitted_names
    );

    let reqs = received.lock().unwrap();
    let get_stores_reqs = reqs
        .iter()
        .filter(|(l, _)| l.starts_with("GET /v1/stores"))
        .count();
    assert_eq!(
        get_stores_reqs, 2,
        "expected exactly two GET /v1/stores pages; got: {:?}",
        reqs
    );
}

/// `index --source <id>` with more than one store in the resolved daemon
/// scope must submit exactly one job, to the id's actual owning store
/// (finding 2) — not one job per store, since `/v1/jobs`'s `create_job`
/// never validates `source_id`.
#[test]
fn index_daemon_source_owner_walk_narrows_to_single_job() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body =
        paginated_list_body(&[&store_record_json("alpha"), &store_record_json("beta")]);
    let alpha_sources = paginated_list_body(&[]);
    let beta_sources = paginated_list_body(&[&source_record_json("src-owned", "beta")]);
    let job_body = r#"{"id":"job-11","status":"queued"}"#;

    let (port, received) = start_routing_mock_server(vec![
        (
            "GET",
            "/v1/stores/alpha/sources",
            "HTTP/1.1 200 OK",
            alpha_sources,
        ),
        (
            "GET",
            "/v1/stores/beta/sources",
            "HTTP/1.1 200 OK",
            beta_sources,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index", "--source", "src-owned"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["id"], "job-11");
    assert!(
        v.get("jobs").is_none(),
        "narrowed to one store, this must render the flat single-job shape: {v}"
    );

    let reqs = received.lock().unwrap();
    let post_reqs: Vec<_> = reqs.iter().filter(|(l, _)| l.starts_with("POST")).collect();
    assert_eq!(
        post_reqs.len(),
        1,
        "exactly one job must be submitted, for the owning store; got: {:?}",
        reqs
    );
    let body_json: serde_json::Value = serde_json::from_str(&post_reqs[0].1).unwrap();
    assert_eq!(body_json["store_name"].as_str().unwrap(), "beta");
}

/// A multi-store `--store` scope that excludes the source's true owner must
/// exit 3 — the daemon walk searches only the resolved (explicit) scope, not
/// every store the daemon has, reproducing embedded mode's hard-filter rule
/// (`index_source_owner_not_in_explicit_store_scope_exits_3`) for the daemon
/// path.
#[test]
fn index_daemon_source_owner_outside_explicit_multi_store_scope_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // The daemon knows three stores; the source actually lives on "beta",
    // which is deliberately left out of the explicit --store scope below.
    let stores_body = paginated_list_body(&[
        &store_record_json("alpha"),
        &store_record_json("beta"),
        &store_record_json("gamma"),
    ]);
    let alpha_sources = paginated_list_body(&[]);
    let gamma_sources = paginated_list_body(&[]);

    let (port, received) = start_routing_mock_server(vec![
        (
            "GET",
            "/v1/stores/alpha/sources",
            "HTTP/1.1 200 OK",
            alpha_sources,
        ),
        (
            "GET",
            "/v1/stores/gamma/sources",
            "HTTP/1.1 200 OK",
            gamma_sources,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args([
            "--store",
            "alpha",
            "--store",
            "gamma",
            "index",
            "--source",
            "src-on-beta",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "a source outside the explicit --store scope must exit 3, not silently redirect; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reqs = received.lock().unwrap();
    assert!(
        !reqs.iter().any(|(l, _)| l.starts_with("POST")),
        "no job should ever be submitted when the owner isn't in scope; got: {:?}",
        reqs
    );
}

/// A source that isn't owned by any store in the (implicit, all-stores)
/// scope must exit 3, same as the explicit-scope case above.
#[test]
fn index_daemon_source_not_found_in_any_scoped_store_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body =
        paginated_list_body(&[&store_record_json("alpha"), &store_record_json("beta")]);
    let empty_sources = paginated_list_body(&[]);

    let (port, received) = start_routing_mock_server(vec![
        (
            "GET",
            "/v1/stores/alpha/sources",
            "HTTP/1.1 200 OK",
            empty_sources.clone(),
        ),
        (
            "GET",
            "/v1/stores/beta/sources",
            "HTTP/1.1 200 OK",
            empty_sources,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index", "--source", "nowhere"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "a source id owned by no scoped store must exit 3; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reqs = received.lock().unwrap();
    assert!(
        !reqs.iter().any(|(l, _)| l.starts_with("POST")),
        "{:?}",
        reqs
    );
}

/// The per-store source-owner walk (`GET /v1/stores/{name}/sources`) must
/// itself paginate to exhaustion: a match sitting on page 2+ of one store's
/// source list must still be found, not silently missed the way a single
/// unpaginated fetch would miss it.
#[test]
fn index_daemon_source_owner_walk_paginates_to_page_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body =
        paginated_list_body(&[&store_record_json("alpha"), &store_record_json("beta")]);
    let alpha_sources = paginated_list_body(&[]);
    // beta has 21 sources; the matching one sits on page 2.
    let beta_page1_items: Vec<String> = (0..20)
        .map(|i| source_record_json(&format!("src-{i:02}"), "beta"))
        .collect();
    let beta_page1 = paginated_list_page(&beta_page1_items, Some("20"), 21);
    let beta_page2 = paginated_list_page(&[source_record_json("src-on-page-2", "beta")], None, 21);
    let job_body = r#"{"id":"job-page2-src","status":"queued"}"#;

    let (port, received) = start_routing_mock_server(vec![
        (
            "GET",
            "/v1/stores/beta/sources?cursor=20",
            "HTTP/1.1 200 OK",
            beta_page2,
        ),
        (
            "GET",
            "/v1/stores/alpha/sources",
            "HTTP/1.1 200 OK",
            alpha_sources,
        ),
        (
            "GET",
            "/v1/stores/beta/sources",
            "HTTP/1.1 200 OK",
            beta_page1,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        ("POST", "/v1/jobs", "HTTP/1.1 200 OK", job_body.to_string()),
    ]);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args(["--json", "index", "--source", "src-on-page-2"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "a match on page 2 of a store's source list must still be found; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reqs = received.lock().unwrap();
    let post_reqs: Vec<_> = reqs.iter().filter(|(l, _)| l.starts_with("POST")).collect();
    assert_eq!(post_reqs.len(), 1, "{:?}", reqs);
    let body_json: serde_json::Value = serde_json::from_str(&post_reqs[0].1).unwrap();
    assert_eq!(body_json["store_name"].as_str().unwrap(), "beta");
}

// ---------------------------------------------------------------------------
// index.rs: --source scoped to its owning store, and lazy embedder
// construction (PR #180 code-review findings 1 & 2 — see
// cli/src/cmds/index.rs's `run_index_async` for the fix these tests cover).
// ---------------------------------------------------------------------------

/// Look up a store's source ULID via `source list --json`, for tests that
/// need a real source id owned by a specific store (`setup_multi_store`
/// hands back fixture paths, not ids).
fn source_id_for_store(dir: &TempDir, store: &str) -> String {
    let output = cmd_with_dir(dir)
        .args(["--json", "--store", store, "source", "list"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "source list --store {store} failed"
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    v["sources"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("store '{store}' should have one source: {v}"))
        .to_string()
}

/// `index --source <id>` with NO `--store` flag (scope = all stores) must
/// resolve the source's owning store and index only that store — not abort
/// on the first store in scope that doesn't own it. Pre-fix, `run_index_async`
/// passed the same globally-unique `source_id` to every store in the
/// resolved scope; `run_embedded_index_with` looked it up within each
/// store's own source list and, under `StrictExit`, returned
/// `Err(SourceNotFound)` the instant it reached a store that didn't own it —
/// aborting the whole run rather than reaching `research`.
///
/// Note the JSON shape assertion: narrowing to research's one store means
/// `render_index_json` collapses to the flat single-store shape (no
/// `stores`/`store` wrapper — that wrapper is reserved for >1 outcome, and
/// single-store JSON must stay byte-identical to the pre-multi-store
/// format), not a 3-entry `stores` array with only `research` populated.
#[test]
fn index_source_scoped_to_owning_store_when_no_store_flag() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // books, default, research
    let research_source_id = source_id_for_store(&dir, "research");

    let output = cmd_with_dir(&dir)
        .args(["--json", "index", "--source", &research_source_id])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index --source <id owned by research>, no --store, should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert!(
        v.get("stores").is_none(),
        "a --source scoped to exactly one store must render single-store JSON, not the \
         multi-store wrapper (which would mean every store in scope got touched): {v}"
    );
    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(v["errors"], 0, "{v}");
    // `setup_multi_store`'s `source add` already auto-indexed this document,
    // so this explicit re-index of the same source finds it unchanged
    // (skipped) rather than indexing it again — that's expected, and still
    // proves the run reached research: the pre-fix bug never got this far at
    // all (it exited 3 on the first non-owning store).
    assert_eq!(
        v["docs_skipped"], 1,
        "research's one fixture document should have been seen (and skipped as \
         already-indexed): {v}"
    );
}

/// `--store books --source <id-owned-by-research>` must exit 3: an explicit
/// `--store` scope is a hard filter — a source outside it is exactly as
/// "not found" as an id that doesn't exist at all. The fix must not silently
/// redirect to the source's real owner just because it's reachable.
#[test]
fn index_source_owner_not_in_explicit_store_scope_exits_3() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);
    let research_source_id = source_id_for_store(&dir, "research");

    let output = cmd_with_dir(&dir)
        .args(["--store", "books", "index", "--source", &research_source_id])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "a source outside the explicit --store scope must exit 3; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Multi-store `index` where every store in scope has zero sources: the run
/// must succeed (exit 0) and report "no sources" for each — and, per review
/// finding 2, must do so WITHOUT ever constructing the embedder. Proven here
/// (not just asserted) by pointing config at an embedding provider that
/// would fail to construct (no matching `providers:` entry): under the
/// pre-fix eager-build behavior — which built the embedder up front,
/// unconditionally, before checking whether any store had sources — this
/// would exit 2 instead.
#[test]
fn index_multi_store_all_empty_reports_no_sources_and_skips_embedder_build() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: perplexity\n      model: pplx-embed-context-v1\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();

    cmd_with_dir(&dir)
        .args(["store", "add", "empty-a"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["store", "add", "empty-b"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args(["--json", "index"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "an all-empty multi-store scope must succeed even with a broken embedding \
         provider config, since no store has sources requiring an embedder; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let stores = v["stores"].as_array().expect("stores array");
    assert_eq!(stores.len(), 2);
    for s in stores {
        assert_eq!(s["status"], "ok", "store entry: {s}");
        assert_eq!(s["message"], "no sources to index", "store entry: {s}");
    }
    assert_eq!(v["total"]["message"], "no sources to index");
}

// ---------------------------------------------------------------------------
// source.rs: PR #180 code-review findings 3 & 5 — see
// cli/src/cmds/source.rs's `run_source_add_async`/`run_source_remove_async`
// for the fixes these tests cover.
//
// Finding 3: `source add --json` across more than one store printed one
// complete JSON document per store, back to back, so the whole of stdout was
// not parseable by a single `serde_json::from_str`. Fixed by accumulating
// per-store results and emitting exactly one top-level document (flat shape
// for exactly one store, `{"status":"ok","results":[...]}` for more than
// one — mirroring `run_source_remove_async`'s existing convention), in both
// the local and daemon-routed branches.
//
// Finding 5: `run_source_remove_async`'s daemon branch fired the DELETE
// before ever validating `--store` names for traversal-safety, unlike
// `source add`'s daemon branch. Fixed by validating every `ctx.stores` name
// with `validate_store_name` before the DELETE — syntax-checking only, no
// local existence check (a daemon may own a different data directory than
// the local DB, per `resolve_daemon_store_scope`'s doc comment in
// `cli/src/app_db.rs`).
// ---------------------------------------------------------------------------

/// Local (non-daemon) `source add --json` across two stores must produce
/// exactly one parseable JSON document — the core finding-3 regression: the
/// pre-fix code called `print_json` once per store inside the loop, so
/// `serde_json::from_str` over the whole of stdout would fail here.
#[test]
fn source_add_json_multi_store_is_single_document_local() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // books, default, research, each pre-seeded

    let fixture = dir.path().join("shared-add-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "books",
            "--store",
            "default",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "multi-store source add --json should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must parse as exactly one JSON document (finding 3 regression): {e}\n\
             stdout:\n{stdout}"
        )
    });

    assert_eq!(v["status"], "ok", "{v}");
    let results = v["results"].as_array().expect("results must be an array");
    assert_eq!(results.len(), 2, "{v}");
    let names: std::collections::HashSet<&str> = results
        .iter()
        .map(|r| r["store"]["name"].as_str().expect("store.name"))
        .collect();
    assert_eq!(names, ["books", "default"].into_iter().collect());
}

/// Single-store `source add --json` keeps the exact pre-existing flat shape
/// (no `results` key) — the counterpart to the multi-store test above.
/// `source_add_json_output` already covers this; this test additionally
/// pins down the negative assertion (`results` must be absent) so the
/// single-vs-multi branch split can't silently start wrapping everything.
#[test]
fn source_add_json_single_store_has_no_results_key() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let fixture = dir.path().join("single-add-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "books",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    assert_eq!(v["status"], "ok", "{v}");
    assert!(v.get("id").is_some(), "{v}");
    assert_eq!(v["store"]["name"], "books", "{v}");
    assert!(
        v.get("results").is_none(),
        "single-store output must not gain a 'results' wrapper: {v}"
    );
}

/// Daemon-routed `source add --json` across two stores must also collapse to
/// exactly one parseable JSON document — the daemon branch has the same
/// per-store `print_json` bug as the local branch, fixed the same way.
#[test]
fn source_add_json_multi_store_is_single_document_daemon() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body =
        paginated_list_body(&[&store_record_json("alpha"), &store_record_json("beta")]);
    let add_body = r#"{"id":"01ABCDEFGHIJKLMNOPQRSTUVWX","store":"whichever","kind":"path"}"#;
    let (port, received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        (
            "POST",
            "/v1/stores",
            "HTTP/1.1 200 OK",
            add_body.to_string(),
        ),
    ]);
    let daemon_url = format!("http://127.0.0.1:{}", port);

    let fixture = dir.path().join("daemon-multi-add-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args([
            "--json",
            "--store",
            "alpha",
            "--store",
            "beta",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "daemon multi-store source add --json should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must parse as exactly one JSON document (finding 3 regression, daemon \
             path): {e}\nstdout:\n{stdout}"
        )
    });
    assert_eq!(v["status"], "ok", "{v}");
    let results = v["results"].as_array().expect("results must be an array");
    assert_eq!(results.len(), 2, "{v}");

    let reqs = received.lock().unwrap();
    let post_reqs = reqs.iter().filter(|(l, _)| l.starts_with("POST")).count();
    assert_eq!(post_reqs, 2, "expected one POST per store; got: {:?}", reqs);
}

// -- source add: finding 5, mid-loop --json failures preserve results ------

/// Local (non-daemon) `source add --json` across two stores where the
/// *second* store's write genuinely fails must not discard the first
/// store's already-persisted result (Codex review round 2, finding 5's
/// residual "genuine mid-loop error" case — the common unknown-store-name
/// case is already closed by work item 1's/finding-4's pre-flight scope
/// validation, so this test forces a different, real failure: a duplicate
/// `(store_id, root)` trips the registry's `UNIQUE constraint failed` ->
/// `invalid_request` mapping, exit 2, in `store-libsql/src/registry/sources.rs`).
#[test]
fn source_add_json_multi_store_mid_loop_failure_preserves_partial_results_local() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .args(["store", "add", "a"])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args(["store", "add", "b"])
        .assert()
        .success();

    let fixture = dir.path().join("dup-root-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    // Pre-seed store "b" with a source at the same root, so the loop's
    // second iteration (store "b") hits a genuine UNIQUE-constraint failure
    // while the first iteration (store "a") succeeds.
    cmd_with_dir(&dir)
        .args(["--store", "b", "source", "add", fixture.to_str().unwrap()])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "a",
            "--store",
            "b",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "a genuine mid-loop store failure should exit with the error's own code (2, \
         invalid_request); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must still be exactly one JSON document on a mid-loop failure: {e}\n\
             stdout:\n{stdout}"
        )
    });
    assert_eq!(v["status"], "error", "{v}");
    assert_eq!(v["error"]["code"], "invalid_request", "{v}");
    let results = v["results"].as_array().expect("results must be an array");
    assert_eq!(
        results.len(),
        1,
        "store a's already-persisted result must be preserved: {v}"
    );
    assert_eq!(results[0]["store"]["name"], "a", "{v}");
}

/// Daemon-routed `source add --json` across two stores where the daemon
/// fails the *second* store's request (e.g. a transient 500): the first
/// store's already-succeeded result must not be discarded — the daemon-branch
/// counterpart to the local-branch test above.
#[test]
fn source_add_daemon_json_multi_store_mid_loop_failure_preserves_partial_results() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let stores_body =
        paginated_list_body(&[&store_record_json("alpha"), &store_record_json("beta")]);
    let ok_body = r#"{"id":"01ABCDEFGHIJKLMNOPQRSTUVWX","store":"alpha","kind":"path"}"#;
    let err_body = r#"{"code":"invalid_request","message":"boom"}"#;
    let (port, _received) = start_routing_mock_server(vec![
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
        (
            "POST",
            "/v1/stores/alpha/sources",
            "HTTP/1.1 200 OK",
            ok_body.to_string(),
        ),
        (
            "POST",
            "/v1/stores/beta/sources",
            "HTTP/1.1 500 Internal Server Error",
            err_body.to_string(),
        ),
    ]);

    let fixture = dir.path().join("daemon-mid-loop-docs");
    std::fs::create_dir_all(&fixture).unwrap();

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", format!("http://127.0.0.1:{}", port))
        .args([
            "--json",
            "--store",
            "alpha",
            "--store",
            "beta",
            "source",
            "add",
            fixture.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "a mid-loop daemon error should exit with the mapped error's own code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must still be exactly one JSON document on a mid-loop failure: {e}\n\
             stdout:\n{stdout}"
        )
    });
    assert_eq!(v["status"], "error", "{v}");
    assert_eq!(v["error"]["code"], "invalid_request", "{v}");
    let results = v["results"].as_array().expect("results must be an array");
    assert_eq!(
        results.len(),
        1,
        "alpha's already-succeeded result must be preserved: {v}"
    );
    assert_eq!(results[0]["id"], "01ABCDEFGHIJKLMNOPQRSTUVWX", "{v}");
}

/// Daemon-routed `source remove` with a syntactically invalid `--store`
/// (traversal attempt) must exit 2 *before* the DELETE ever fires — the core
/// finding-5 regression. Proven not just by the exit code but by asserting
/// the mock daemon recorded zero requests.
#[test]
fn source_remove_daemon_invalid_store_name_exits_2_and_sends_no_request() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let body = r#"{"status":"ok","id":"01ABCDEFGHIJKLMNOPQRSTUVWX"}"#;
    let (port, received) = start_recording_mock_server("HTTP/1.1 200 OK", body);
    let daemon_url = format!("http://127.0.0.1:{}", port);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args([
            "--store",
            "../evil",
            "source",
            "remove",
            "01ABCDEFGHIJKLMNOPQRSTUVWX",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        2,
        "an unsafe --store name must exit 2 before the DELETE fires; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reqs = received.lock().unwrap();
    assert!(
        reqs.is_empty(),
        "mock daemon must receive no request when --store fails validation; got: {:?}",
        reqs
    );
}

/// Daemon-routed `source remove` with a `--store` name that is syntactically
/// valid but unknown to the *local* database must still reach the daemon:
/// the daemon (not the local DB) is the authority on which stores exist for
/// this path (`resolve_daemon_store_scope`'s doc comment in
/// `cli/src/app_db.rs`), and `LOCALDB_DAEMON_URL` may point at a daemon with
/// an entirely different data directory. This is the deliberate flip side of
/// the test above: validation must reject bad syntax, but must NOT reject
/// names just because this process has never heard of them.
#[test]
fn source_remove_daemon_unknown_but_valid_store_name_reaches_daemon() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let body = r#"{"status":"ok","id":"01ABCDEFGHIJKLMNOPQRSTUVWX"}"#;
    let (port, received) = start_recording_mock_server("HTTP/1.1 200 OK", body);
    let daemon_url = format!("http://127.0.0.1:{}", port);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args([
            "--store",
            "totally-unknown-store",
            "source",
            "remove",
            "01ABCDEFGHIJKLMNOPQRSTUVWX",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "a syntactically valid --store name must reach the daemon even if locally unknown; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reqs = received.lock().unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "expected exactly one DELETE request to reach the daemon; got: {:?}",
        reqs
    );
    assert!(reqs[0].0.starts_with("DELETE "), "{:?}", reqs[0]);
    assert!(
        reqs[0].0.contains("/v1/sources/01ABCDEFGHIJKLMNOPQRSTUVWX"),
        "{:?}",
        reqs[0]
    );
}

// ---------------------------------------------------------------------------
// Finding 4 — `status` and `store list` now validate/resolve explicit
// `--store` instead of silently ignoring it — see
// cli/src/cmds/status.rs's `run_status_async` and cli/src/cmds/store.rs's
// `run_store_list_async`, both of which now route through
// `resolve_store_scope(ctx, &db, StoreScopePolicy::AllStores)`
// (cli/src/app_db.rs) instead of calling `db.backend().list_stores()`
// directly. specs/05-surfaces.md §2.2's repeatable-and-validated rule for
// `--store` was never actually an exemption for these two commands — only
// the *default* (all stores) when `-s` is omitted was already correct.
//
// A deliberate side effect (approved, not a bug): a database with zero
// stores now falls into `resolve_store_scope`'s `AllStores` empty-set
// branch, which is exit 2 ("no stores; run `localdb store add <name>` or
// pass --store"), not a silent empty-list exit 0. This is intentional ahead
// of implicit init (an auto-created `default` store) — see the reworked
// `store_list_with_minimal_config_and_no_stores_exits_2_with_no_stores_message`
// test above, and `status_zero_stores_exits_2_with_no_stores_message` below.
// ---------------------------------------------------------------------------

#[test]
fn store_list_unknown_store_name_exits_3() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--store", "typo", "store", "list"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "unknown --store name should exit 3; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn status_unknown_store_name_exits_3() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--store", "typo", "status"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        3,
        "unknown --store name should exit 3; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn store_list_traversal_store_name_exits_2() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--store", "../evil", "store", "list"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "traversal --store name should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn status_traversal_store_name_exits_2() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir);

    let output = cmd_with_dir(&dir)
        .args(["--store", "../evil", "status"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "traversal --store name should exit 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `--store books status` on a multi-store DB must show only `books`, not
/// every store — the core Finding-4 regression for `status`.
#[test]
fn status_explicit_store_filters_to_that_store() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // books, default, research

    let output = cmd_with_dir(&dir)
        .args(["--json", "--store", "books", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let stores = v["stores"].as_array().expect("stores must be an array");
    assert_eq!(stores.len(), 1, "expected exactly one store; got: {v}");
    assert_eq!(stores[0]["name"].as_str().unwrap(), "books");
}

/// `--store books store list` on a multi-store DB must show only `books` —
/// the core Finding-4 regression for `store list`.
#[test]
fn store_list_explicit_store_filters_to_that_store() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // books, default, research

    let output = cmd_with_dir(&dir)
        .args(["--json", "--store", "books", "store", "list"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let stores = v["stores"].as_array().expect("stores must be an array");
    assert_eq!(stores.len(), 1, "expected exactly one store; got: {v}");
    assert_eq!(stores[0]["name"].as_str().unwrap(), "books");
}

/// Repeated `-s a -s b` must resolve both, deduped, in first-seen order —
/// exercised here for `status`; `store list` shares the same resolver.
#[test]
fn status_repeated_store_flags_resolve_both_in_order() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // books, default, research

    let output = cmd_with_dir(&dir)
        .args([
            "--json", "--store", "research", "--store", "books", "status",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let stores = v["stores"].as_array().expect("stores must be an array");
    let names: Vec<&str> = stores.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["research", "books"], "got: {v}");
}

/// Repeated `-s a -s b` for `store list`, mirroring the `status` case above.
#[test]
fn store_list_repeated_store_flags_resolve_both_in_order() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // books, default, research

    let output = cmd_with_dir(&dir)
        .args([
            "--json", "--store", "research", "--store", "books", "store", "list",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let stores = v["stores"].as_array().expect("stores must be an array");
    let names: Vec<&str> = stores.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["research", "books"], "got: {v}");
}

/// No `--store` at all must still behave exactly as before: every store in
/// scope, for both commands.
#[test]
fn status_and_store_list_no_flag_show_all_stores() {
    let dir = TempDir::new().unwrap();
    setup_multi_store(&dir); // books, default, research

    for args in [vec!["--json", "status"], vec!["--json", "store", "list"]] {
        let output = cmd_with_dir(&dir).args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{:?}; stderr: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
        let stores = v["stores"].as_array().expect("stores must be an array");
        let mut names: Vec<&str> = stores.iter().map(|s| s["name"].as_str().unwrap()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["books", "default", "research"], "{:?}", args);
    }
}

/// `status` equivalent of `store_list_with_minimal_config_and_no_stores_exits_2_with_no_stores_message`
/// above: a minimal config with a fresh, empty data dir (no stores at all)
/// must still load via the lenient path (F1-cli) — proven by the "no
/// stores" message rather than an "invalid config" one — and then fail
/// loudly with exit 2 per the all-stores zero-store policy, ahead of
/// implicit init.
#[test]
fn status_zero_stores_exits_2_with_no_stores_message() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();

    let output = cmd_with_dir(&dir).arg("status").output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "status with zero stores should exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("no stores"),
        "expected the no-stores message; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("invalid config"),
        "the minimal config must not be rejected as invalid; stderr: {stderr}"
    );
}
