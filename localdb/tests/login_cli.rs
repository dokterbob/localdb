//! `localdb login --json` stdout-purity regression test (specs/05-surfaces.md
//! §2 / §3.1: `--json` guarantees stdout is pure JSON, even on failure).
//!
//! Before this fix, `perform_login`'s no-browser instructions ("Open this
//! URL...") and paste prompt ("Paste the code shown after authorizing: ")
//! were written with `println!`/`print!`, landing on stdout ahead of the
//! final `--json` result and breaking any pipe into `jq` or similar. They
//! now go to stderr unconditionally.
//!
//! `--url` is passed explicitly so this never needs a real daemon:
//! `resolve_login_base_url` only probes for one when `--url` is absent.
//! Stdin is closed immediately, so the paste-prompt `read_line` sees EOF
//! and the pasted code is empty, failing fast with `invalid_request` (exit
//! 2) — a deterministic outcome that still exercises the no-browser
//! instructions/prompt path this regression is about.

use assert_cmd::Command;
use tempfile::TempDir;

fn cmd() -> Command {
    Command::cargo_bin("localdb").expect("localdb binary must exist")
}

fn cmd_with_dir(dir: &TempDir) -> Command {
    let mut c = cmd();
    c.env("LOCALDB_CONFIG", dir.path().join("config.yaml"));
    c
}

#[test]
fn login_json_no_browser_stdout_is_pure_json_even_on_failure() {
    let dir = TempDir::new().unwrap();

    let assert = cmd_with_dir(&dir)
        .arg("login")
        .arg("--url")
        .arg("http://127.0.0.1:1")
        .arg("--no-browser")
        .arg("--json")
        .write_stdin("")
        .timeout(std::time::Duration::from_secs(10))
        .assert();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("stdout must be pure JSON under --json, got parse error {e}: {stdout:?}")
    });
    assert!(
        parsed.get("error").is_some(),
        "an empty pasted code should fail with a JSON error body: {parsed}"
    );

    // The instructions/prompt must have gone to stderr, not stdout.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Open this URL") || stderr.contains("Paste the code"),
        "the no-browser instructions must still appear, on stderr: {stderr}"
    );
}

/// `--url` is omitted here specifically so `resolve_login_base_url` takes
/// its daemon-probing path: against a fresh, empty data dir with no daemon
/// running, that path fails with `Error::DaemonUnreachable` (exit 5). A
/// malformed `credentials.json` must still win over that — the credentials
/// preflight runs before the base URL is ever resolved, so this exits 2.
#[test]
fn login_fails_on_malformed_credentials_before_probing_for_a_daemon() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("config.yaml"), "version: 1\n").unwrap();
    std::fs::write(dir.path().join("credentials.json"), "not json at all").unwrap();

    let assert = cmd_with_dir(&dir)
        .arg("login")
        .timeout(std::time::Duration::from_secs(10))
        .assert();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a malformed credentials.json must fail preflight (exit 2) before the daemon is \
         ever probed, not surface as daemon-unreachable (exit 5): {stderr}"
    );
    assert!(
        stderr.contains("credentials.json"),
        "the error should name the credentials file: {stderr}"
    );
}
