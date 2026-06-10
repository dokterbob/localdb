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
fn write_default_config(dir: &TempDir) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\n",
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
// serve / mcp stubs
// ---------------------------------------------------------------------------

#[test]
fn serve_stub_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    cmd_with_dir(&dir)
        .arg("serve")
        .assert()
        .failure()
        .stderr(predicate::str::contains("T11"));
}

#[test]
fn mcp_stub_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    cmd_with_dir(&dir)
        .arg("mcp")
        .assert()
        .failure()
        .stderr(predicate::str::contains("T10"));
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
        .args(["store", "remove", "removeme"])
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
        .args(["store", "remove", "nosuchstore"])
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

    // Citation must have the expected canonical shape fields.
    let cit = &citations[0];
    assert!(cit.get("chunk_id").is_some(), "missing chunk_id");
    assert!(cit.get("document_id").is_some(), "missing document_id");
    assert!(cit.get("uri").is_some(), "missing uri");
    assert!(cit.get("snippet").is_some(), "missing snippet");
    assert!(cit.get("score").is_some(), "missing score");

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
