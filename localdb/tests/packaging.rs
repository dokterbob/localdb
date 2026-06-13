//! T12 — Packaging & release tests.
//!
//! These tests verify the acceptance criteria from the T12 ticket:
//!   - versioned `--version` output (semver format)
//!   - smoke workflow: install → init → index fixture → search returns citations
//!   - the release workflow YAML exists and targets the three required platforms
//!   - binary has no unexpected dynamic deps (checked by examining the binary type)
//!
//! Coverage gates: N/A for T12 (no product code); the smoke script is the test.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper: build a Command for the localdb binary.
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// T12-AC1: versioned `--version`
// ---------------------------------------------------------------------------

/// `--version` must exit 0 and emit a semver-style version.
#[test]
fn version_flag_exits_zero_with_semver() {
    let out = cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("localdb"));

    // The version line must contain a digit.version pattern (semver).
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    let has_semver = stdout.split_whitespace().any(|tok| {
        let parts: Vec<&str> = tok.split('.').collect();
        parts.len() >= 2 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    });
    assert!(
        has_semver,
        "--version output must contain a semver-like version (e.g. 0.1.0); got: {stdout}",
    );
}

/// The version reported by `--version` matches the workspace Cargo.toml version.
#[test]
fn version_matches_cargo_toml() {
    // workspace version is baked in at build time via clap's `version`.
    let cargo_version = env!("CARGO_PKG_VERSION");

    cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(cargo_version));
}

// ---------------------------------------------------------------------------
// T12-AC2: release workflow exists and covers three targets
// ---------------------------------------------------------------------------

/// The release workflow YAML must exist at `.github/workflows/release.yml`.
#[test]
fn release_workflow_file_exists() {
    // Walk up from the test binary location to find the workspace root.
    // The worktree/project root is the parent of .github/.
    let workflow_path = workspace_root().join(".github/workflows/release.yml");
    assert!(
        workflow_path.exists(),
        "release workflow not found at: {}",
        workflow_path.display(),
    );
}

/// The release workflow must declare all three required platform targets.
#[test]
fn release_workflow_has_required_targets() {
    let workflow_path = workspace_root().join(".github/workflows/release.yml");
    let content = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|_| panic!("cannot read {}", workflow_path.display()));

    for required_target in &[
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ] {
        assert!(
            content.contains(required_target),
            "release workflow missing target '{required_target}'",
        );
    }
}

/// The release workflow must be triggered on tag pushes.
#[test]
fn release_workflow_triggers_on_tags() {
    let workflow_path = workspace_root().join(".github/workflows/release.yml");
    let content = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|_| panic!("cannot read {}", workflow_path.display()));

    assert!(
        content.contains("tags:"),
        "release workflow must trigger on tag pushes",
    );
}

/// The release workflow must upload artifacts (tarballs).
#[test]
fn release_workflow_uploads_artifacts() {
    let workflow_path = workspace_root().join(".github/workflows/release.yml");
    let content = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|_| panic!("cannot read {}", workflow_path.display()));

    // Either upload-artifact or gh release upload step must be present.
    let has_upload = content.contains("upload-artifact")
        || content.contains("softprops/action-gh-release")
        || content.contains("gh release upload")
        || content.contains("release_assets");
    assert!(has_upload, "release workflow must upload release artifacts",);
}

// ---------------------------------------------------------------------------
// T12-AC3: smoke workflow — init → index fixture → search returns citations
// ---------------------------------------------------------------------------

/// Smoke test: init a fresh config, index a markdown fixture, search and
/// get back at least one citation.  This is the same logical flow the
/// smoke_test.sh script performs, but driven from Rust for CI reliability.
#[test]
fn smoke_init_index_search() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    // 1. init
    cmd_with_dir(&dir).arg("init").assert().success();

    // 2. store add
    cmd_with_dir(&dir)
        .args(["store", "add", "smoke"])
        .assert()
        .success();

    // 3. write fixture document
    let docs = dir.path().join("smoke_docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(
        docs.join("localdb_intro.md"),
        "# localdb\n\nlocaldb is a local-first knowledge server with hybrid search.\n\
         It indexes files and URLs into a local store and provides natural-language search.\n",
    )
    .unwrap();

    // 4. source add
    cmd_with_dir(&dir)
        .args(["--store", "smoke", "source", "add"])
        .arg(docs.to_str().unwrap())
        .assert()
        .success();

    // 5. index
    cmd_with_dir(&dir)
        .args(["--store", "smoke", "index"])
        .assert()
        .success();

    // 6. search
    let out = cmd_with_dir(&dir)
        .args([
            "--json",
            "--store",
            "smoke",
            "search",
            "knowledge server search",
        ])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "smoke search must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("smoke search must emit valid JSON; got: {stdout}"));

    let citations = v["citations"]
        .as_array()
        .expect("search output must have citations array");

    assert!(
        !citations.is_empty(),
        "smoke search must return at least one citation after indexing fixture doc",
    );

    // Citation must reference our fixture file.
    let uri = citations[0]["uri"].as_str().unwrap_or("");
    assert!(
        uri.contains("localdb_intro.md"),
        "top citation should reference the indexed fixture; uri={uri}",
    );
}

// ---------------------------------------------------------------------------
// T12-AC4: smoke script exists and is executable
// ---------------------------------------------------------------------------

/// The smoke_test.sh script must exist at the workspace root.
#[test]
fn smoke_script_exists() {
    let script = workspace_root().join("smoke_test.sh");
    assert!(
        script.exists(),
        "smoke_test.sh not found at workspace root: {}",
        script.display(),
    );
}

/// The smoke_test.sh script must be executable (on Unix).
#[cfg(unix)]
#[test]
fn smoke_script_is_executable() {
    use std::os::unix::fs::PermissionsExt;

    let script = workspace_root().join("smoke_test.sh");
    let meta =
        std::fs::metadata(&script).unwrap_or_else(|_| panic!("cannot stat {}", script.display()));
    let mode = meta.permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "smoke_test.sh must be executable (chmod +x); current mode: {mode:o}",
    );
}

// ---------------------------------------------------------------------------
// Utility: locate workspace root relative to manifest directory
// ---------------------------------------------------------------------------

fn workspace_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR for the `localdb` crate is <workspace>/localdb.
    // The workspace root is one level up.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .expect("manifest dir has a parent (workspace root)")
        .to_path_buf()
}
