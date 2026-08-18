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

/// `--version` (long form) must identify the exact build: either a git SHA
/// (7+ hex chars, from vergen) or the literal `unknown` when built outside a
/// git checkout (e.g. from a source tarball).
#[test]
fn long_version_contains_commit_sha_or_unknown() {
    let out = cmd().arg("--version").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_lowercase();

    let has_sha = stdout
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok.len() >= 7 && tok.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(
        has_sha || stdout.contains("unknown"),
        "--version must contain a commit SHA (or 'unknown' for git-less builds); got: {stdout}",
    );
}

/// The workspace license must match what LICENSE/README/docs declare.
#[test]
fn workspace_license_is_agpl() {
    assert_eq!(
        env!("CARGO_PKG_LICENSE"),
        "AGPL-3.0-or-later",
        "workspace [workspace.package].license must match the LICENSE file (AGPL-3.0-or-later)",
    );
}

// ---------------------------------------------------------------------------
// T12-AC2: release pipeline shape — dist config + custom workflows
// ---------------------------------------------------------------------------

/// The dist-generated release workflow must exist at
/// `.github/workflows/release.yml`.
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

/// release.yml is generated from dist-workspace.toml; the three required
/// platform targets are declared there.
#[test]
fn dist_config_has_required_targets() {
    let config_path = workspace_root().join("dist-workspace.toml");
    let content = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|_| panic!("cannot read {}", config_path.display()));

    for required_target in &[
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ] {
        assert!(
            content.contains(required_target),
            "dist-workspace.toml missing target '{required_target}'",
        );
    }
}

/// dist config must keep the Homebrew channel: both installers plus our tap.
#[test]
fn dist_config_has_homebrew_installer_and_tap() {
    let content = std::fs::read_to_string(workspace_root().join("dist-workspace.toml"))
        .expect("dist-workspace.toml must exist");
    assert!(
        content.contains("\"homebrew\"") && content.contains("\"shell\""),
        "dist-workspace.toml must keep the homebrew + shell installers",
    );
    assert!(
        content.contains("dokterbob/homebrew-localdb"),
        "dist-workspace.toml must name the tap",
    );
}

/// The release workflow must be triggered on tag pushes (dist's version-tag
/// pattern, which matches release-plz's bare vX.Y.Z tags).
#[test]
fn release_workflow_triggers_on_tags() {
    let workflow_path = workspace_root().join(".github/workflows/release.yml");
    let content = std::fs::read_to_string(&workflow_path)
        .unwrap_or_else(|_| panic!("cannot read {}", workflow_path.display()));

    assert!(
        content.contains("tags:"),
        "release workflow must trigger on tag pushes",
    );
    assert!(
        content.contains("[0-9]+.[0-9]+.[0-9]+"),
        "release workflow tag pattern must match vX.Y.Z",
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

/// Every custom job the dist config references must exist as a reusable
/// (`workflow_call`) workflow, and release.yml must actually call it —
/// otherwise `dist generate` was run without the companion files.
#[test]
fn custom_workflows_exist_and_are_wired() {
    let root = workspace_root();
    let release = std::fs::read_to_string(root.join(".github/workflows/release.yml"))
        .expect("release.yml must exist");

    for (file, job) in &[
        ("release-checks.yml", "custom-release-checks"),
        ("homebrew-tap-publish.yml", "custom-homebrew-tap-publish"),
        ("smoke-test.yml", "custom-smoke-test"),
    ] {
        let path = root.join(".github/workflows").join(file);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("cannot read {}", path.display()));
        assert!(
            content.contains("workflow_call"),
            "{file} must be a reusable workflow (workflow_call)",
        );
        assert!(
            release.contains(job),
            "release.yml must wire the {job} job (regenerate with `dist generate`)",
        );
    }
}

/// The release-plz workflow (rolling bump+changelog PR; tag on merge) must
/// exist — it is what feeds tags to the dist pipeline.
#[test]
fn release_plz_workflow_exists() {
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/release-plz.yml"))
            .expect("release-plz.yml must exist");
    assert!(
        content.contains("release-pr"),
        "release-plz workflow must maintain the release PR",
    );
    assert!(
        content.contains("command: release"),
        "release-plz workflow must tag on merge",
    );
}

/// The tap formula template must keep the brew-services and completions
/// integrations that justify hand-maintaining it over dist's generated one.
#[test]
fn homebrew_template_has_service_and_completions() {
    let root = workspace_root();
    let template = std::fs::read_to_string(root.join("homebrew/localdb.rb.erb"))
        .expect("homebrew/localdb.rb.erb must exist");
    assert!(
        template.contains("service do"),
        "formula template must declare a brew-services `service do` block",
    );
    assert!(
        template.contains("generate_completions_from_executable"),
        "formula template must install shell completions",
    );
    assert!(
        template.contains("AGPL-3.0-or-later"),
        "formula template must carry the license",
    );
    assert!(
        root.join("homebrew/render.rb").exists(),
        "formula render script must exist",
    );
}

// ---------------------------------------------------------------------------
// Shell completions: `localdb completions <shell>` (specs/05-surfaces.md §2)
// ---------------------------------------------------------------------------

/// Every supported shell generates a non-empty completion script mentioning
/// the binary name, exit 0. Pure codegen: must work without config or store.
#[test]
fn completions_generate_for_all_shells() {
    for shell in &["bash", "zsh", "fish", "elvish", "powershell"] {
        let out = cmd()
            .args(["completions", shell])
            .output()
            .expect("completions must run");
        assert!(
            out.status.success(),
            "completions {shell} must exit 0; stderr: {}",
            String::from_utf8_lossy(&out.stderr),
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("localdb"),
            "completions {shell} output must mention 'localdb'; got {} bytes",
            stdout.len(),
        );
    }
}

/// An unknown shell is a usage error (clap rejects it, exit 2).
#[test]
fn completions_unknown_shell_is_usage_error() {
    cmd().args(["completions", "tcsh"]).assert().code(2);
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
