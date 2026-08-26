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

// ---------------------------------------------------------------------------
// Daemon-attached routing — mock HTTP server
//
// Copied from `cli_integration.rs`'s `start_routing_mock_server` pattern
// (own crate — `tests/*.rs` files don't share helpers) to pin the daemon-mode
// URL construction for `document list`/`document get`
// (`cli/src/cmds/document.rs`'s `DocumentListCmd`/`DocumentGetCmd`).
// ---------------------------------------------------------------------------

/// Requests recorded by [`start_routing_mock_server`]: one `(start_line,
/// json_body)` pair per request received, in arrival order.
type RecordedRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// A single `(method, path_prefix, status_line, body)` route for
/// [`start_routing_mock_server`] — see that function's doc comment for the
/// matching rules (method exact-or-any, path via `starts_with`,
/// first-match-wins).
type MockRoute = (&'static str, &'static str, &'static str, String);

const UNMATCHED_ROUTE_STATUS: &str = "HTTP/1.1 404 Not Found";
const UNMATCHED_ROUTE_BODY: &str =
    r#"{"code":"resource_not_found","message":"no mock route matched this request"}"#;

/// Spin up a minimal mock HTTP server on a random port that dispatches each
/// request to the first route in `routes` whose method matches (exactly, or
/// any method if `""`) and whose path (with its query string still attached)
/// starts with `path_prefix` — first-match-wins, so a cursor-specific route
/// must be listed before a broader prefix that would otherwise also match it.
/// Requests matching no route get a 404 rather than hanging. Every request's
/// start-line and raw JSON body (if any) is recorded for assertions.
fn start_routing_mock_server(routes: Vec<MockRoute>) -> (u16, RecordedRequests) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();
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
/// renders `null`) and `total` — for building one *page* of a larger list, to
/// drive the pagination tests below.
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

/// One `StoreRecord` (`server/src/state.rs`) JSON object, for stubbing `GET
/// /v1/stores` — the store-scope resolution every daemon-routed `document`
/// command performs before hitting its own endpoint.
fn store_record_json(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","id":"01STOREID000000000000000A","visibility":"private","backend":"libsql"}}"#
    )
}

/// A `DocumentInfo`-shaped (`core/src/backend.rs`) JSON object, for stubbing
/// `GET /v1/stores/{{name}}/documents` items. `daemon_item_to_document_list_item`
/// (`cli/src/cmds/document.rs`) reads `id`/`uri`/`title`/`store_id`/
/// `source_id`/`content_hash`/`fetched_at`/`date_original`/`date_parsed`/
/// `index_updated_at` from each item, so the remaining `DocumentInfo` fields
/// (`ingestor_kind`, `mime`, `origin_store`, `policy_version`, `metadata`)
/// are omitted here.
#[allow(clippy::too_many_arguments)]
fn daemon_document_list_item_json(
    id: &str,
    uri: &str,
    title: Option<&str>,
    store_id: &str,
    source_id: &str,
    content_hash: &str,
    fetched_at: &str,
    date_original: Option<&str>,
    date_parsed: Option<&str>,
    index_updated_at: Option<&str>,
) -> String {
    let opt_json = |v: Option<&str>| match v {
        Some(s) => format!("\"{s}\""),
        None => "null".to_string(),
    };
    let title_json = opt_json(title);
    let date_original_json = opt_json(date_original);
    let date_parsed_json = opt_json(date_parsed);
    let index_updated_at_json = opt_json(index_updated_at);
    format!(
        r#"{{"id":"{id}","uri":"{uri}","title":{title_json},"store_id":"{store_id}","source_id":"{source_id}","content_hash":"{content_hash}","fetched_at":"{fetched_at}","date_original":{date_original_json},"date_parsed":{date_parsed_json},"index_updated_at":{index_updated_at_json}}}"#
    )
}

/// A minimal but fully-populated `Metadata::Document(..)` JSON value
/// (`core/src/metadata.rs`) — every field is `#[serde(default)]`-free on at
/// least one variant, so a `document get` daemon fixture must set every key
/// explicitly rather than omitting the empty ones.
const EMPTY_DOCUMENT_METADATA_JSON: &str = r#"{"kind":"document","title":null,"creator":[],"subject":[],"description":null,"publisher":null,"contributor":[],"date":null,"type":null,"format":null,"identifier":null,"source":null,"language":null,"relation":[],"coverage":null,"rights":null,"page_count":null,"word_count":null}"#;

/// A `DocumentRecord`-shaped (`server/src/handlers/documents.rs`) JSON
/// object, for stubbing `GET /v1/documents/{id}` — note the wire field is
/// `normalized_text`, not `text` (`document_get_result_from_daemon_json` in
/// `cli/src/cmds/document.rs` reads it under that name).
fn daemon_document_record_json(
    id: &str,
    uri: &str,
    store_id: &str,
    source_id: &str,
    content_hash: &str,
    fetched_at: &str,
    normalized_text: &str,
) -> String {
    format!(
        r#"{{"id":"{id}","uri":"{uri}","title":null,"store_id":"{store_id}","source_id":"{source_id}","content_hash":"{content_hash}","fetched_at":"{fetched_at}","normalized_text":"{normalized_text}","metadata":{EMPTY_DOCUMENT_METADATA_JSON}}}"#
    )
}

/// `document list -s <store>` daemon-routing: derives the daemon-mock's
/// fixture from an embedded run's own persisted document (same id/uri/store),
/// then asserts `--json` and text output are byte-identical between the two
/// transports and that the request actually hit `GET
/// /v1/stores/mystore/documents` — mirroring
/// `source_list_daemon_routes_and_matches_embedded_shape`
/// (`cli_integration.rs`) for `document list`.
#[test]
fn document_list_daemon_routes_and_matches_embedded_shape() {
    let embedded_dir = TempDir::new().unwrap();
    write_default_config(&embedded_dir);
    let fixture = write_fixture_file(&embedded_dir, "docs", "hello.md", "# Hello\n\nBody.\n");
    store_with_indexed_dir(&embedded_dir, "mystore", &fixture);

    let embedded_v = document_list_json(&embedded_dir, &["--store", "mystore"]);
    let embedded_text = cmd_with_dir(&embedded_dir)
        .args(["--store", "mystore", "document", "list"])
        .output()
        .unwrap();
    let embedded_text_stdout = String::from_utf8_lossy(&embedded_text.stdout).to_string();

    let d = &embedded_v["documents"][0];
    let daemon_item = daemon_document_list_item_json(
        d["id"].as_str().unwrap(),
        d["uri"].as_str().unwrap(),
        d["title"].as_str(),
        d["store_id"].as_str().unwrap(),
        d["source_id"].as_str().unwrap(),
        d["content_hash"].as_str().unwrap(),
        d["fetched_at"].as_str().unwrap(),
        d["date_original"].as_str(),
        d["date_parsed"].as_str(),
        d["index_updated_at"].as_str(),
    );

    let daemon_dir = TempDir::new().unwrap();
    write_default_config(&daemon_dir);
    let stores_body = paginated_list_body(&[&store_record_json("mystore")]);
    let documents_body = paginated_list_body(&[&daemon_item]);
    let (port, received) = start_routing_mock_server(vec![
        // The more specific `/v1/stores/mystore/documents` route must be
        // listed before the bare `/v1/stores` one — every documents path
        // also starts with `/v1/stores`.
        (
            "GET",
            "/v1/stores/mystore/documents",
            "HTTP/1.1 200 OK",
            documents_body,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
    ]);
    let daemon_url = format!("http://127.0.0.1:{}", port);

    let daemon_json = cmd_with_dir(&daemon_dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args(["--json", "--store", "mystore", "document", "list"])
        .output()
        .unwrap();
    assert!(
        daemon_json.status.success(),
        "daemon-routed document list --json should succeed; stderr: {}",
        String::from_utf8_lossy(&daemon_json.stderr)
    );
    let daemon_v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&daemon_json.stdout)).unwrap();
    assert_eq!(
        embedded_v, daemon_v,
        "--json document list must be identical between embedded and daemon-mock"
    );

    let daemon_text = cmd_with_dir(&daemon_dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args(["--store", "mystore", "document", "list"])
        .output()
        .unwrap();
    let daemon_text_stdout = String::from_utf8_lossy(&daemon_text.stdout).to_string();
    assert_eq!(
        embedded_text_stdout, daemon_text_stdout,
        "text document list must be identical between embedded and daemon-mock"
    );

    let reqs = received.lock().unwrap();
    assert!(
        reqs.iter()
            .any(|(l, _)| l.starts_with("GET /v1/stores/mystore/documents")),
        "expected a GET /v1/stores/mystore/documents request; got {:?}",
        reqs
    );
}

/// `document list -s <store> --source <id>` daemon-routing: the `--source`
/// filter must reach the daemon as `?source=<id>` on the list path, and the
/// per-store document walk must itself paginate to exhaustion (a match
/// sitting on page 2 of `GET /v1/stores/{name}/documents` must still be
/// found) via `&cursor=`, not `?cursor=`, since the path already carries the
/// `?source=` query string.
#[test]
fn document_list_daemon_source_filter_and_pagination_walks_to_page_2() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let page1_item = daemon_document_list_item_json(
        "doc-a",
        "file:///docs/a.md",
        None,
        "store-id-1",
        "src-1",
        "hash-a",
        "2026-01-01T00:00:00Z",
        None,
        None,
        None,
    );
    let page2_item = daemon_document_list_item_json(
        "doc-b",
        "file:///docs/b.md",
        None,
        "store-id-1",
        "src-1",
        "hash-b",
        "2026-01-02T00:00:00Z",
        None,
        None,
        None,
    );
    let page1_body = paginated_list_page(&[page1_item], Some("1"), 2);
    let page2_body = paginated_list_page(&[page2_item], None, 2);
    let stores_body = paginated_list_body(&[&store_record_json("mystore")]);

    let (port, received) = start_routing_mock_server(vec![
        // Cursor-specific route listed before the bare `?source=` route —
        // first-match-wins on a path *prefix*, and the page-2 request also
        // starts with the page-1 route's prefix.
        (
            "GET",
            "/v1/stores/mystore/documents?source=src-1&cursor=1",
            "HTTP/1.1 200 OK",
            page2_body,
        ),
        (
            "GET",
            "/v1/stores/mystore/documents?source=src-1",
            "HTTP/1.1 200 OK",
            page1_body,
        ),
        ("GET", "/v1/stores", "HTTP/1.1 200 OK", stores_body),
    ]);
    let daemon_url = format!("http://127.0.0.1:{}", port);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args([
            "--json", "--store", "mystore", "document", "list", "--source", "src-1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "daemon-routed document list --source --json should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    let docs = v["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 2, "both pages' documents must be returned: {v}");
    let ids: Vec<&str> = docs.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["doc-a", "doc-b"]);

    let reqs = received.lock().unwrap();
    assert!(
        reqs.iter()
            .any(|(l, _)| l == "GET /v1/stores/mystore/documents?source=src-1 HTTP/1.1"),
        "expected the page-1 request to carry '?source=src-1'; got {:?}",
        reqs
    );
    assert!(
        reqs.iter()
            .any(|(l, _)| l.starts_with(
                "GET /v1/stores/mystore/documents?source=src-1&cursor=1"
            )),
        "expected the page-2 request to carry '&cursor=1' after the existing '?source=' query; got {:?}",
        reqs
    );
}

/// `document get <id> -s <a> -s <b>` daemon-routing: every `-s`/`--store`
/// value must reach the daemon as its own `store=` query parameter, in
/// order, percent-encoded (issue #207-style URL construction, see
/// `cli/src/daemon_client.rs::encode_path_segment`) — store names here
/// deliberately contain a space and a `#` to force encoding.
#[test]
fn document_get_daemon_sends_multiple_store_params_percent_encoded() {
    let dir = TempDir::new().unwrap();
    write_default_config(&dir);

    let id = "doc-shared-id";
    let body = daemon_document_record_json(
        id,
        "file:///docs/shared.md",
        "store-id-1",
        "src-1",
        "hash-shared",
        "2026-01-01T00:00:00Z",
        "the reconstructed body",
    );
    let (port, received) =
        start_routing_mock_server(vec![("GET", "/v1/documents/", "HTTP/1.1 200 OK", body)]);
    let daemon_url = format!("http://127.0.0.1:{}", port);

    let output = cmd_with_dir(&dir)
        .env("LOCALDB_DAEMON_URL", &daemon_url)
        .args([
            "--json",
            "-s",
            "store one",
            "-s",
            "store#two",
            "document",
            "get",
            id,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "daemon-routed document get --json should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap();
    // Same JSON shape `document_get_result_json` (embedded mode) produces.
    assert_eq!(v["id"], id);
    assert_eq!(v["uri"], "file:///docs/shared.md");
    assert_eq!(v["store_id"], "store-id-1");
    assert_eq!(v["source_id"], "src-1");
    assert_eq!(v["content_hash"], "hash-shared");
    assert_eq!(v["fetched_at"], "2026-01-01T00:00:00Z");
    assert_eq!(v["text"], "the reconstructed body");
    assert_eq!(v["metadata"]["kind"], "document");

    let reqs = received.lock().unwrap();
    let expected_prefix = format!("GET /v1/documents/{id}?store=store%20one&store=store%23two");
    assert!(
        reqs.iter().any(|(l, _)| l.starts_with(&expected_prefix)),
        "expected both -s values as separate, percent-encoded 'store=' params in request order; \
         got {:?}",
        reqs
    );
}
