//! Integration tests for `localdb document list` / `localdb document get`
//! (specs/05-surfaces.md §2).
//!
//! Own crate, separate from `cli_integration.rs` (which is already close to
//! the file-size ceiling) — setup helpers below are copied from that file's
//! patterns rather than shared, per the same module-boundary convention
//! `cli_integration.rs` itself follows relative to the rest of the test
//! suite.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn cmd() -> Command {
    Command::cargo_bin("localdb").expect("localdb binary must exist")
}

fn cmd_with_dir(dir: &TempDir) -> Command {
    let mut c = cmd();
    c.env("LOCALDB_CONFIG", dir.path().join("config.yaml"));
    c
}

/// Write a minimal valid config to `dir/config.yaml`, with `paths.data`
/// pointing inside the temp dir to avoid polluting the user's data dir.
/// Pins `provider: fake` so these tests run offline without any API key or
/// the ~706 MB local-model download `provider: local` would otherwise
/// trigger on first `index` — the same idiom `cli_integration.rs`'s
/// `write_default_config` uses.
fn write_default_config(dir: &TempDir) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

/// Create a store, add a path source pointing at `fixture_dir` (auto-indexed
/// by `source add`), and return the store name.
fn store_with_indexed_dir(dir: &TempDir, store_name: &str, fixture_dir: &std::path::Path) {
    cmd_with_dir(dir)
        .args(["store", "add", store_name])
        .assert()
        .success();
    cmd_with_dir(dir)
        .args([
            "--store",
            store_name,
            "source",
            "add",
            fixture_dir.to_str().unwrap(),
        ])
        .assert()
        .success();
}

fn write_fixture_file(
    dir: &TempDir,
    subdir: &str,
    filename: &str,
    body: &str,
) -> std::path::PathBuf {
    let fixture_dir = dir.path().join(subdir);
    std::fs::create_dir_all(&fixture_dir).unwrap();
    std::fs::write(fixture_dir.join(filename), body).unwrap();
    fixture_dir
}

fn document_list_json(dir: &TempDir, extra_args: &[&str]) -> serde_json::Value {
    let mut args = vec!["--json", "document", "list"];
    args.extend_from_slice(extra_args);
    let output = cmd_with_dir(dir).args(&args).output().unwrap();
    assert!(
        output.status.success(),
        "document list --json should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "document list --json must emit valid JSON: {e}; got: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

// ---------------------------------------------------------------------------
// document list
// ---------------------------------------------------------------------------

#[test]
fn document_list_on_empty_store_reports_no_documents() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .args(["store", "add", "empty-store"])
        .assert()
        .success();

    cmd_with_dir(&dir)
        .args(["--store", "empty-store", "document", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No documents on store 'empty-store'",
        ));
}

#[test]
fn document_list_after_indexing_shows_the_document() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let fixture = write_fixture_file(&dir, "docs", "hello.md", "# Hello\n\nSome content.\n");
    store_with_indexed_dir(&dir, "s1", &fixture);

    cmd_with_dir(&dir)
        .args(["--store", "s1", "document", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.md"));
}

#[test]
fn document_list_json_shape_has_documents_array_with_expected_fields() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let fixture = write_fixture_file(&dir, "docs", "hello.md", "# Hello\n\nSome content.\n");
    store_with_indexed_dir(&dir, "s1", &fixture);

    let v = document_list_json(&dir, &["--store", "s1"]);
    let docs = v["documents"].as_array().expect("documents must be array");
    assert_eq!(docs.len(), 1);
    let d = &docs[0];
    assert!(d
        .get("id")
        .and_then(|x| x.as_str())
        .is_some_and(|s| !s.is_empty()));
    assert!(d["uri"].as_str().unwrap().contains("hello.md"));
    assert_eq!(d["store"]["name"], "s1");
    assert!(d.get("store_id").is_some());
    assert!(d.get("source_id").is_some());
    assert!(d.get("content_hash").is_some());
    assert!(d.get("fetched_at").is_some());
}

#[test]
fn document_list_source_filter_narrows_to_one_source() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let fixture_a = write_fixture_file(&dir, "docs-a", "a.md", "# A\n\nDoc A content.\n");
    let fixture_b = write_fixture_file(&dir, "docs-b", "b.md", "# B\n\nDoc B content.\n");
    cmd_with_dir(&dir)
        .args([
            "--store",
            "s1",
            "source",
            "add",
            fixture_a.to_str().unwrap(),
        ])
        .assert()
        .success();
    cmd_with_dir(&dir)
        .args([
            "--store",
            "s1",
            "source",
            "add",
            fixture_b.to_str().unwrap(),
        ])
        .assert()
        .success();

    // Sanity: both documents are visible unfiltered.
    let all = document_list_json(&dir, &["--store", "s1"]);
    assert_eq!(all["documents"].as_array().unwrap().len(), 2);

    // Find source A's id via `source list --json`.
    let output = cmd_with_dir(&dir)
        .args(["--json", "--store", "s1", "source", "list"])
        .output()
        .unwrap();
    let sources: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let source_a_id = sources["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["root"].as_str().unwrap().contains("docs-a"))
        .expect("source for docs-a must exist")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let filtered = document_list_json(&dir, &["--store", "s1", "--source", &source_a_id]);
    let docs = filtered["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert!(docs[0]["uri"].as_str().unwrap().contains("a.md"));
}

#[test]
fn document_list_store_scoping_narrows_to_named_store() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let fixture1 = write_fixture_file(&dir, "docs1", "one.md", "# One\n\nContent one.\n");
    let fixture2 = write_fixture_file(&dir, "docs2", "two.md", "# Two\n\nContent two.\n");
    store_with_indexed_dir(&dir, "store-one", &fixture1);
    store_with_indexed_dir(&dir, "store-two", &fixture2);

    // Unscoped spans both stores; human output gets a store-name column.
    let output = cmd_with_dir(&dir)
        .args(["document", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("one.md"));
    assert!(stdout.contains("two.md"));

    let scoped = document_list_json(&dir, &["--store", "store-one"]);
    let docs = scoped["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["store"]["name"], "store-one");
}

// ---------------------------------------------------------------------------
// document get
// ---------------------------------------------------------------------------

/// Index one document into `store_name` and return its id (via `document
/// list --json`).
fn indexed_document_id(dir: &TempDir, store_name: &str, fixture: &std::path::Path) -> String {
    store_with_indexed_dir(dir, store_name, fixture);
    let v = document_list_json(dir, &["--store", store_name]);
    v["documents"][0]["id"].as_str().unwrap().to_string()
}

#[test]
fn document_get_by_id_returns_expected_fields() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let fixture = write_fixture_file(&dir, "docs", "hello.md", "# Hello\n\nSome content.\n");
    let id = indexed_document_id(&dir, "s1", &fixture);

    let output = cmd_with_dir(&dir)
        .args(["document", "get", &id])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "document get should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&format!("id: {id}")));
    assert!(stdout.contains("uri:"));
    assert!(stdout.contains("store_id:"));
    assert!(stdout.contains("source_id:"));
    assert!(stdout.contains("content_hash:"));
    assert!(stdout.contains("fetched_at:"));
    // No --text: the reconstructed body must not appear.
    assert!(!stdout.contains("Some content."));
}

#[test]
fn document_get_with_text_flag_appends_reconstructed_body() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let fixture = write_fixture_file(
        &dir,
        "docs",
        "hello.md",
        "# Hello\n\nSome distinctive body text.\n",
    );
    let id = indexed_document_id(&dir, "s1", &fixture);

    cmd_with_dir(&dir)
        .args(["document", "get", &id, "--text"])
        .assert()
        .success()
        .stdout(predicate::str::contains("distinctive body text"));
}

#[test]
fn document_get_json_always_includes_text_field_regardless_of_text_flag() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let fixture = write_fixture_file(
        &dir,
        "docs",
        "hello.md",
        "# Hello\n\nJson body marker text.\n",
    );
    let id = indexed_document_id(&dir, "s1", &fixture);

    for args in [
        vec!["--json", "document", "get", id.as_str()],
        vec!["--json", "document", "get", id.as_str(), "--text"],
    ] {
        let output = cmd_with_dir(&dir).args(&args).output().unwrap();
        assert!(output.status.success());
        let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        // Unwrapped single object, not `{"document": {...}}`.
        assert_eq!(v["id"], id);
        assert!(
            v["text"]
                .as_str()
                .unwrap()
                .contains("Json body marker text"),
            "the 'text' field must always be present in --json output: {v}"
        );
    }
}

#[test]
fn document_get_unknown_id_exits_3() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    cmd_with_dir(&dir)
        .args(["store", "add", "s1"])
        .assert()
        .success();

    let output = cmd_with_dir(&dir)
        .args([
            "document",
            "get",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code().unwrap(), 3);
}

/// The same file indexed into two different stores gets the *same*
/// document id — `resource_id` (`core/src/ids.rs`) is derived from the
/// canonical source URI plus content hash, and both stores here see the
/// same absolute path and content. A bare `document get <id>` then hits
/// `get_document_detail_scoped`'s cross-store ambiguity path
/// (`Error::InvalidRequest`, exit 2); scoping with `--store` disambiguates.
#[test]
fn document_get_cross_store_ambiguity_requires_store_scope() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let fixture = write_fixture_file(
        &dir,
        "shared-docs",
        "shared.md",
        "# Shared\n\nShared content.\n",
    );

    store_with_indexed_dir(&dir, "store-a", &fixture);
    store_with_indexed_dir(&dir, "store-b", &fixture);

    let id_a = document_list_json(&dir, &["--store", "store-a"])["documents"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let id_b = document_list_json(&dir, &["--store", "store-b"])["documents"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        id_a, id_b,
        "same path + content must yield the same document id in both stores"
    );

    let ambiguous = cmd_with_dir(&dir)
        .args(["document", "get", &id_a])
        .output()
        .unwrap();
    assert_eq!(
        ambiguous.status.code().unwrap(),
        2,
        "an unscoped get of an id present in two stores must exit 2 (ambiguous); stderr: {}",
        String::from_utf8_lossy(&ambiguous.stderr)
    );

    cmd_with_dir(&dir)
        .args(["--store", "store-a", "document", "get", &id_a])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("id: {id_a}")));
}
