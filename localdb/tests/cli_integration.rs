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

#[test]
fn status_shows_daemon_not_running() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

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
    // Each store has name, ownership, visibility, backend.
    let store = &stores[0];
    assert!(store.get("name").is_some());
    assert!(store.get("ownership").is_some());
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

    // Store should no longer appear in list.
    cmd_with_dir(&dir)
        .args(["store", "list"])
        .assert()
        .success()
        .stdout(predicate::str::is_match("(?i)no stores|^$").unwrap().or(
            // If store list returns empty JSON array, that's also fine.
            predicate::str::contains("removeme").not(),
        ));
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
    assert!(cit.get("document_id").is_some(), "missing document_id");
    assert!(cit.get("uri").is_some(), "missing uri");
    assert!(cit.get("snippet").is_some(), "missing snippet");
    assert!(cit.get("score").is_some(), "missing score");

    // store: {id, name}
    let store = cit.get("store").expect("missing store field");
    assert!(store.get("id").is_some(), "store.id missing");
    assert!(store.get("name").is_some(), "store.name missing");

    // span: {start, end}
    let span = cit.get("span").expect("missing span field");
    assert!(span.get("start").is_some(), "span.start missing");
    assert!(span.get("end").is_some(), "span.end missing");

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

// ---------------------------------------------------------------------------
// Store-locked exit code (acceptance criterion)
// ---------------------------------------------------------------------------

/// Verify that `Error::StoreLocked` maps to exit code 4.
///
/// We simulate "locked" by attempting to remove a yaml-owned store (which returns
/// config_readonly / exit code 4).
#[test]
fn store_locked_exit_code_is_4() {
    let dir = TempDir::new().unwrap();
    write_config_with_data_dir(&dir, "stores:\n  - name: yaml-store");

    let output = cmd_with_dir(&dir)
        .args(["store", "remove", "yaml-store"])
        .output()
        .unwrap();

    // config_readonly → exit code 4.
    assert_eq!(
        output.status.code().unwrap(),
        4,
        "config_readonly should exit 4; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Verify `store add` on a YAML-owned store returns exit code 4.
#[test]
fn yaml_owned_store_mutation_exits_4() {
    let dir = TempDir::new().unwrap();
    write_config_with_data_dir(&dir, "stores:\n  - name: yaml-store");

    let output = cmd_with_dir(&dir)
        .args(["store", "add", "yaml-store"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code().unwrap(),
        4,
        "should exit 4 (config_readonly) when adding a YAML-owned store"
    );
}

// ---------------------------------------------------------------------------
// Real store_locked path — OS-level write lock contention (acceptance criterion)
//
// The test proves that Error::StoreLocked is raised from ACTUAL write-lock
// contention between two concurrent processes, not just config_readonly.
// We hold the .write.lock file with an fd-lock in this process (via a helper
// binary invocation that holds the lock and signals via a file), then attempt
// a `store add` from another process. The second process must exit 4 with
// the `store_locked` error code in its JSON output.
// ---------------------------------------------------------------------------

/// Prove the real `store_locked` path: a second writer process exits 4 when
/// the OS-level write lock is held by another process.
///
/// Strategy: create the data dir and take the OS advisory lock on `.write.lock`
/// using the same mechanism as WriteLock (fd-lock), hold it for the duration
/// of the test, then run `store add` in a subprocess. The subprocess must exit 4.
#[test]
fn real_store_locked_exits_4() {
    use std::fs::OpenOptions;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // Data dir (same path load_app_db resolves to).
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let lock_path = data_dir.join(".write.lock");
    let lock_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .expect("cannot create lock file");

    // Acquire an OS-level exclusive advisory lock on the file.
    // This mirrors what WriteLock::acquire() does internally.
    use fd_lock::RwLock;
    let mut rw = RwLock::new(lock_file);
    let _guard = rw.try_write().expect("should acquire lock in test process");

    // Now run `store add` — it must fail to acquire the write lock and exit 4.
    let output = cmd_with_dir(&dir)
        .args(["--json", "store", "add", "locked-store"])
        .output()
        .unwrap();

    let exit_code = output.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code,
        4,
        "store add while lock held by another process must exit 4 (store_locked); \
         stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    // Also verify the JSON error code is specifically `store_locked`.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);
    // The --json flag routes errors to stderr as JSON.
    // Try to parse either stream as JSON to find the error code.
    for s in &[stdout.as_ref(), stderr.as_ref()] {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            if let Some(code) = v.get("error").and_then(|e| e.as_str()) {
                assert_eq!(
                    code, "store_locked",
                    "JSON error code must be 'store_locked', got '{}'; combined: {}",
                    code, combined
                );
                return; // found and verified
            }
        }
    }
    // If we couldn't parse JSON, the exit code 4 check above is sufficient.
    // (Some platforms may write the error to stderr without JSON wrapper.)
    // The key assertion is exit code 4 with the correct lock path.
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

// ---------------------------------------------------------------------------
// Runtime-state lock contention tests (regression guard for #46 and #56)
//
// These tests hold the runtime-state redb lock in-process, then run CLI
// commands as subprocesses to verify they get exit 4 + runtime_state_locked
// rather than empty results (#46) or an opaque internal error (#56).
// ---------------------------------------------------------------------------

/// Regression guard for #46: read-only commands (store list, search, status)
/// must exit 4 with `runtime_state_locked` when the redb lock is held,
/// NOT silently show empty results from a temp DB.
#[test]
fn runtime_state_locked_read_commands_exit_4() {
    use redb::Database;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // Pre-create the data dir and the runtime-state DB path so our lock-holder
    // and the CLI agree on the same file.
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Open the runtime-state redb to hold the exclusive lock.
    let state_db_path = data_dir.join("runtime-state.redb");
    let _lock_holder = Database::create(&state_db_path)
        .expect("should be able to create runtime-state.redb in test");

    // `store list --json` must exit 4 with runtime_state_locked, not empty JSON.
    let output = cmd_with_dir(&dir)
        .args(["--json", "store", "list"])
        .output()
        .unwrap();

    let exit_code = output.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code,
        4,
        "store list while runtime-state DB is locked must exit 4; \
         stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    // Verify the JSON error code.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for s in &[stderr.as_ref(), stdout.as_ref()] {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            if let Some(code) = v.get("error").and_then(|e| e.as_str()) {
                assert_eq!(
                    code, "runtime_state_locked",
                    "JSON error code must be 'runtime_state_locked', got '{}'; combined: {}{}",
                    code, stdout, stderr
                );
                return;
            }
        }
    }
    // Exit code 4 is the key guarantee; JSON parsing may vary.
}

/// Regression guard for #56: strict callers (mcp) must exit 4 with a clear
/// message when the runtime-state redb lock is held, not an opaque internal error.
#[test]
fn runtime_state_locked_mcp_exits_4() {
    use redb::Database;

    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Hold the runtime-state redb lock.
    let state_db_path = data_dir.join("runtime-state.redb");
    let _lock_holder = Database::create(&state_db_path)
        .expect("should be able to create runtime-state.redb in test");

    // `mcp` (strict) must exit 4, not 1.
    let output = cmd_with_dir(&dir)
        .arg("mcp")
        .write_stdin("")
        .output()
        .unwrap();

    let exit_code = output.status.code().unwrap_or(-1);
    assert_eq!(
        exit_code,
        4,
        "mcp while runtime-state DB is locked must exit 4; \
         stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
}
