//! Integration tests for `localdb search`'s ten metadata-filter flags
//! (issue #247): `--path`, `--mime`, and `--{axis}-after`/`--{axis}-before`
//! for each of the four `DateAxis` values (`added`, `updated`, `modified`,
//! `document`).
//!
//! Own crate, separate from `cli_integration.rs` (already close to the
//! file-size ceiling) — setup helpers below are copied from that file's
//! patterns rather than shared, per the same module-boundary convention
//! `cli_document.rs` follows.

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::BufRead;
use std::time::{Duration, SystemTime};
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
/// pointing inside the temp dir. Pins `provider: fake` so these tests run
/// offline without any API key or the local-model download `provider:
/// local` would otherwise trigger.
fn write_default_config(dir: &TempDir) {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();
}

/// Query words shared by every fixture document and every search below —
/// chosen to avoid the literal word "search" (the subcommand name) purely
/// for readability of the test bodies, not because it would actually be
/// ambiguous to clap.
const QUERY_WORDS: [&str; 4] = ["needle", "filters", "rust", "programming"];

/// Build the shared fixture set (one shared set, table-driven over all ten
/// flags below — not eight separate fixture sets): two documents
/// distinguishable on every axis a filter can bound.
///
/// - `keep/alpha.md`: Markdown (mime `text/markdown`), under `keep/` (for
///   `--path`), with front-matter `date: 2020-01-01` (for `--document-*`),
///   mtime set 60 days in the past (for `--modified-*`), indexed FIRST (for
///   `--added-*`/`--updated-*`).
/// - `skip/beta.txt`: plain text (mime `text/plain`), under `skip/`, no
///   front matter (so its document date is NULL — excluded by the NULL rule
///   from both `--document-after` and `--document-before`), fresh mtime,
///   indexed SECOND, after the returned cutoff.
///
/// Returns the store name and the RFC 3339 cutoff between the two `source
/// add` calls, for `--added-after`/`--added-before`/`--updated-after`/
/// `--updated-before`.
fn setup_filter_fixtures(dir: &TempDir) -> (&'static str, String) {
    let store = "filter-store";
    let docs_dir = dir.path().join("docs");
    let keep_dir = docs_dir.join("keep");
    let skip_dir = docs_dir.join("skip");
    std::fs::create_dir_all(&keep_dir).unwrap();
    std::fs::create_dir_all(&skip_dir).unwrap();

    let alpha_path = keep_dir.join("alpha.md");
    std::fs::write(
        &alpha_path,
        "---\ndate: 2020-01-01\n---\n# Alpha\n\nneedle filters test rust programming content.\n",
    )
    .unwrap();
    let old_mtime = SystemTime::now() - Duration::from_secs(60 * 86_400);
    std::fs::File::open(&alpha_path)
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();

    let beta_path = skip_dir.join("beta.txt");
    std::fs::write(
        &beta_path,
        "needle filters test rust programming content.\n",
    )
    .unwrap();
    // beta's mtime stays fresh (file just created) — no explicit set_modified.

    cmd_with_dir(dir)
        .args(["store", "add", store])
        .assert()
        .success();

    cmd_with_dir(dir)
        .args([
            "--store",
            store,
            "source",
            "add",
            keep_dir.to_str().unwrap(),
        ])
        .assert()
        .success();

    // `added_at`/`index_updated_at` are stamped at write time with
    // second-granularity RFC 3339 (`core::ingestion::now_rfc3339`); bracket
    // the cutoff capture with sleeps comfortably longer than one second so
    // alpha's and beta's timestamps can never land in the same second as
    // the cutoff itself.
    std::thread::sleep(Duration::from_millis(1200));
    let cutoff = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    std::thread::sleep(Duration::from_millis(1200));

    cmd_with_dir(dir)
        .args([
            "--store",
            store,
            "source",
            "add",
            skip_dir.to_str().unwrap(),
        ])
        .assert()
        .success();

    (store, cutoff)
}

/// Run `localdb --json --store <store> search [extra_flags...] <QUERY_WORDS>`
/// and parse the JSON response.
fn run_search_json(dir: &TempDir, store: &str, extra_flags: &[&str]) -> serde_json::Value {
    let mut args: Vec<&str> = vec!["--json", "--store", store, "search"];
    args.extend_from_slice(extra_flags);
    args.extend_from_slice(&QUERY_WORDS);

    let output = cmd_with_dir(dir).args(&args).output().unwrap();
    assert!(
        output.status.success(),
        "search {extra_flags:?} should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("search --json must emit valid JSON; got: {stdout}"))
}

fn citation_uris(v: &serde_json::Value) -> Vec<String> {
    v["citations"]
        .as_array()
        .expect("citations must be an array")
        .iter()
        .map(|c| c["uri"].as_str().unwrap().to_string())
        .collect()
}

/// The single non-negotiable table-driven test: one shared fixture set,
/// exercised across all ten filter flags, each asserted to narrow the
/// unfiltered baseline correctly.
#[test]
fn search_filters_narrow_results_across_all_ten_flags() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (store, cutoff) = setup_filter_fixtures(&dir);

    // Baseline: unfiltered, both documents must match.
    let baseline = run_search_json(&dir, store, &[]);
    let baseline_uris = citation_uris(&baseline);
    let alpha_uri = baseline_uris
        .iter()
        .find(|u| u.contains("alpha.md"))
        .unwrap_or_else(|| panic!("baseline missing alpha.md: {baseline_uris:?}"))
        .clone();
    assert!(
        baseline_uris.iter().any(|u| u.contains("beta.txt")),
        "baseline missing beta.txt: {baseline_uris:?}"
    );

    // Derive the exact `--path` prefix from the real indexed URI rather
    // than reconstructing a `file://` URI by hand (percent-encoding /
    // canonicalization are the store's concern, not this test's).
    let path_prefix = alpha_uri[..alpha_uri.rfind("alpha.md").unwrap()].to_string();

    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["--path", &path_prefix], "alpha.md"),
        (vec!["--mime", "text/markdown"], "alpha.md"),
        (vec!["--added-before", &cutoff], "alpha.md"),
        (vec!["--added-after", &cutoff], "beta.txt"),
        (vec!["--updated-before", &cutoff], "alpha.md"),
        (vec!["--updated-after", &cutoff], "beta.txt"),
        (vec!["--modified-before", "7d"], "alpha.md"),
        (vec!["--modified-after", "7d"], "beta.txt"),
        (vec!["--document-after", "2019-01-01"], "alpha.md"),
        (vec!["--document-before", "2021-01-01"], "alpha.md"),
    ];

    for (flags, expected_substring) in cases {
        let result = run_search_json(&dir, store, &flags);
        let uris = citation_uris(&result);
        assert!(
            !uris.is_empty(),
            "flags {flags:?} returned no citations; expected only {expected_substring}"
        );
        assert!(
            uris.iter().all(|u| u.contains(expected_substring)),
            "flags {flags:?} should narrow to only {expected_substring}, got {uris:?}"
        );
    }
}

/// A malformed date-filter value is `invalid_request` — exit 2 — not a
/// panic or a silently-empty result.
#[test]
fn search_bad_date_filter_exits_2() {
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
            "search",
            "--added-after",
            "not-a-date",
            "hello",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code().unwrap(),
        2,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `--mime` never runs through date/duration parsing: `"7d"` would be a
/// valid duration for a date flag, but here it is only ever a literal MIME
/// string. No real document has that MIME type, so the request must still
/// succeed (exit 0, no `invalid_request`) with zero results — the contrast
/// with `search_bad_date_filter_exits_2` above.
#[test]
fn mime_filter_treats_duration_shaped_value_as_a_literal_string() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);
    let (store, _cutoff) = setup_filter_fixtures(&dir);

    let result = run_search_json(&dir, store, &["--mime", "7d"]);
    let citations = result["citations"]
        .as_array()
        .expect("citations must be an array");
    assert!(
        citations.is_empty(),
        "no fixture document has mime \"7d\"; got: {result:?}"
    );
}

/// `search --help` must list all ten filter flags — there is no existing
/// `search --help` test today, following the `store_subcommand_help` /
/// `source_subcommand_help` pattern in `cli_integration.rs`.
#[test]
fn search_subcommand_help_lists_all_filter_flags() {
    cmd()
        .args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--path"))
        .stdout(predicate::str::contains("--mime"))
        .stdout(predicate::str::contains("--added-after"))
        .stdout(predicate::str::contains("--added-before"))
        .stdout(predicate::str::contains("--updated-after"))
        .stdout(predicate::str::contains("--updated-before"))
        .stdout(predicate::str::contains("--modified-after"))
        .stdout(predicate::str::contains("--modified-before"))
        .stdout(predicate::str::contains("--document-after"))
        .stdout(predicate::str::contains("--document-before"));
}

/// Consistency guard (issue #247): the clap `--help` output for `localdb
/// search` and the MCP `search` tool schema must both describe each
/// `DateAxis` using its `describe()` text verbatim. This is the entire
/// justification for hand-writing the flag/schema description text in
/// three places instead of deriving it at macro-expansion time, which is
/// not possible for `#[arg(long = ...)]`/`#[schemars(description = ...)]`
/// (see `core::store::DateAxis::describe`'s doc comment).
#[test]
fn date_axis_describe_text_matches_cli_help_and_mcp_schema() {
    let help_output = cmd()
        .args(["search", "--help"])
        .output()
        .expect("run localdb search --help");
    assert!(help_output.status.success());
    let help_text = String::from_utf8_lossy(&help_output.stdout).to_string();

    let schema =
        schemars::SchemaGenerator::default().into_root_schema_for::<mcp::args::SearchArgs>();
    let schema_json = schema.to_value();
    let properties = schema_json["properties"]
        .as_object()
        .expect("SearchArgs schema must have an object 'properties'");
    let schema_text: String = properties
        .values()
        .filter_map(|p| p.get("description").and_then(|d| d.as_str()))
        .collect::<Vec<_>>()
        .join("\n");

    for axis in localdb_core::DateAxis::ALL {
        let describe = axis.describe();
        assert!(
            help_text.contains(describe),
            "`localdb search --help` must contain {axis:?}'s describe() text {describe:?}; \
             got:\n{help_text}"
        );
        assert!(
            schema_text.contains(describe),
            "the MCP search tool schema must contain {axis:?}'s describe() text {describe:?}; \
             got:\n{schema_text}"
        );
    }
}

/// Parity (issue #247): the same filter value must produce the same
/// citations whether `search` runs embedded or daemon-attached. The
/// mechanism is a single shared `SearchFilters` struct serialized into the
/// daemon POST body, so per-flag parity would be redundant — one
/// representative filter (`--path`) is enough to prove the wiring.
#[test]
fn search_path_filter_parity_embedded_vs_daemon() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = format!(
        "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\nserver:\n  port: 0\n",
        data_dir.to_string_lossy()
    );
    std::fs::write(dir.path().join("config.yaml"), &config).unwrap();

    let (store, _cutoff) = setup_filter_fixtures(&dir);

    // Derive the exact `--path` prefix from a real indexed URI, same as
    // `search_filters_narrow_results_across_all_ten_flags` above.
    let baseline = run_search_json(&dir, store, &[]);
    let baseline_uris = citation_uris(&baseline);
    let alpha_uri = baseline_uris
        .iter()
        .find(|u| u.contains("alpha.md"))
        .unwrap_or_else(|| panic!("baseline missing alpha.md: {baseline_uris:?}"))
        .clone();
    let path_prefix = alpha_uri[..alpha_uri.rfind("alpha.md").unwrap()].to_string();

    // Embedded: no daemon running yet.
    let embedded = run_search_json(&dir, store, &["--path", &path_prefix]);
    let mut embedded_uris = citation_uris(&embedded);
    embedded_uris.sort();
    assert!(
        !embedded_uris.is_empty(),
        "embedded search returned no results"
    );

    // Now start a real daemon pointed at the same config/data dir.
    let bin = assert_cmd::cargo::cargo_bin("localdb");
    let mut daemon = std::process::Command::new(&bin)
        .arg("serve")
        .env("LOCALDB_CONFIG", dir.path().join("config.yaml"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn localdb serve");

    let daemon_stdout = daemon.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(daemon_stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read announce line");
    assert!(
        line.contains("http://"),
        "announce line must contain http:// URL, got: {line}"
    );

    // Daemon-attached: `daemon.sock` now exists, so `search` auto-routes to it.
    let daemon_result = run_search_json(&dir, store, &["--path", &path_prefix]);
    let mut daemon_uris = citation_uris(&daemon_result);
    daemon_uris.sort();

    daemon.kill().ok();
    daemon.wait().ok();

    assert_eq!(
        embedded_uris, daemon_uris,
        "the same --path filter must produce the same citations embedded and daemon-attached"
    );
}
