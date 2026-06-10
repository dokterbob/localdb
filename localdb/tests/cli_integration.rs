//! Integration tests for the `localdb` binary.
//!
//! These tests use `assert_cmd` to drive the binary as a subprocess,
//! verifying the CLI surface from specs/05-surfaces.md §2.

use assert_cmd::Command;
use predicates::prelude::*;

fn cmd() -> Command {
    Command::cargo_bin("localdb").expect("localdb binary must exist")
}

/// `localdb --help` must list all subcommands from specs/05-surfaces.md §2.
#[test]
fn help_lists_all_subcommands() {
    let mut c = cmd();
    c.arg("--help");
    let output = c.output().expect("localdb --help should succeed");

    assert!(output.status.success(), "--help should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Help text may appear on stdout or stderr (clap sends it to stdout).
    let help_text = format!("{stdout}{stderr}");

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

/// `localdb init` stub must exit 1 (not yet implemented).
#[test]
fn init_stub_exits_nonzero() {
    cmd()
        .arg("init")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

/// `localdb search` requires a query argument.
#[test]
fn search_requires_query() {
    cmd().arg("search").assert().failure();
}

/// `localdb search <query>` stub exits non-zero (not yet implemented).
#[test]
fn search_stub_exits_nonzero() {
    cmd()
        .args(["search", "test query"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}
