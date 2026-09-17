use super::*;
use crate::cmds::listing::store_column_width;

fn test_source_row(root: Option<&str>, url: Option<&str>) -> SourceRow {
    SourceRow {
        id: "01HRQHB7FN3WMX4AZDV3S9VCTZ".to_string(),
        store_id: "store-1".to_string(),
        kind: if root.is_some() {
            SourceKind::Path
        } else {
            SourceKind::Url
        },
        root: root.map(str::to_string),
        url: url.map(str::to_string),
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    }
}

#[test]
fn format_source_line_single_store_matches_legacy_format() {
    let src = test_source_row(Some("/Volumes/Archive/books"), None);
    let line = source_to_human_line(&src);
    assert_eq!(
        line,
        "01HRQHB7FN3WMX4AZDV3S9VCTZ [path] /Volumes/Archive/books"
    );
}

#[test]
fn format_source_line_multi_store_prefixes_padded_name() {
    let src = test_source_row(Some("/Volumes/Archive/books"), None);
    let width = store_column_width(["books", "default"].into_iter());
    assert_eq!(width, 9); // "default" (7) + 2
    let item = source_row_to_list_item(&src, "books");
    let line = item.human_line(true, width);
    assert_eq!(
        line,
        "books    01HRQHB7FN3WMX4AZDV3S9VCTZ [path] /Volumes/Archive/books"
    );
}

#[test]
fn format_source_line_falls_back_to_url_when_no_root() {
    let src = test_source_row(None, Some("https://example.com"));
    let line = source_to_human_line(&src);
    assert_eq!(line, "01HRQHB7FN3WMX4AZDV3S9VCTZ [url] https://example.com");
}

fn feed_row(id: &str, url: &str, config_json: Option<&str>, refresh: Option<&str>) -> SourceRow {
    SourceRow {
        id: id.to_string(),
        store_id: "store-1".to_string(),
        kind: SourceKind::Feed,
        root: None,
        url: Some(url.to_string()),
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: refresh.map(str::to_string),
        created_at: now_rfc3339(),
        config_json: config_json.map(str::to_string),
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    }
}

fn url_row(id: &str, url: &str, refresh: Option<&str>) -> SourceRow {
    SourceRow {
        id: id.to_string(),
        store_id: "store-1".to_string(),
        kind: SourceKind::Url,
        root: None,
        url: Some(url.to_string()),
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: refresh.map(str::to_string),
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    }
}

fn path_row(id: &str, root: &str) -> SourceRow {
    SourceRow {
        id: id.to_string(),
        store_id: "store-1".to_string(),
        kind: SourceKind::Path,
        root: Some(root.to_string()),
        url: None,
        include: vec!["**/*.md".to_string()],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    }
}

// --- resolve_source_add_kind: flag-matrix rejections (exit 2) ---

#[test]
fn resolve_source_add_kind_rejects_max_entries_with_path_kind() {
    let err = resolve_source_add_kind("/tmp/docs", Some("path"), Some(10), false).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[test]
fn resolve_source_add_kind_rejects_max_entries_with_url_kind() {
    let err = resolve_source_add_kind("https://example.com/page", Some("url"), Some(10), false)
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn resolve_source_add_kind_rejects_max_entries_without_override_on_inferred_path() {
    // No --kind at all: classify_source infers "path" from a non-URL arg.
    let err = resolve_source_add_kind("/tmp/docs", None, Some(5), false).unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn resolve_source_add_kind_rejects_no_fetch_full_content_with_non_feed() {
    let err = resolve_source_add_kind("/tmp/docs", Some("path"), None, true).unwrap_err();
    assert_eq!(err.exit_code(), 2);

    let err =
        resolve_source_add_kind("https://example.com/page", Some("url"), None, true).unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn resolve_source_add_kind_rejects_kind_feed_non_http_url() {
    let err = resolve_source_add_kind("ftp://example.com/feed.xml", Some("feed"), None, false)
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[test]
fn resolve_source_add_kind_rejects_max_entries_zero() {
    let err = resolve_source_add_kind("https://example.com/feed.xml", Some("feed"), Some(0), false)
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

// --- resolve_source_add_kind: acceptance paths ---

#[test]
fn resolve_source_add_kind_accepts_feed_defaults() {
    let (kind, parsed) =
        resolve_source_add_kind("https://example.com/feed.xml", Some("feed"), None, false).unwrap();
    assert_eq!(kind, "feed");
    let parsed = parsed.expect("feed kind yields a parsed spec");
    assert_eq!(parsed.kind, SourceKind::Feed);
    assert_eq!(parsed.url, Some("https://example.com/feed.xml".to_string()));
    let config = localdb_core::source::parse_feed_config_json(parsed.config_json.as_deref());
    assert_eq!(config.max_entries, None);
    assert!(config.fetch_full_content);
}

#[test]
fn resolve_source_add_kind_accepts_feed_with_explicit_fields() {
    let (kind, parsed) =
        resolve_source_add_kind("https://example.com/feed.xml", Some("feed"), Some(25), true)
            .unwrap();
    assert_eq!(kind, "feed");
    let parsed = parsed.unwrap();
    let config = localdb_core::source::parse_feed_config_json(parsed.config_json.as_deref());
    assert_eq!(config.max_entries, Some(25));
    assert!(
        !config.fetch_full_content,
        "--no-fetch-full-content flips the default"
    );
}

#[test]
fn resolve_source_add_kind_infers_path_and_url_without_override() {
    let (kind, parsed) = resolve_source_add_kind("/tmp/docs", None, None, false).unwrap();
    assert_eq!(kind, "path");
    assert!(parsed.is_none());

    let (kind, parsed) =
        resolve_source_add_kind("https://example.com/page", None, None, false).unwrap();
    assert_eq!(kind, "url");
    assert!(parsed.is_none());
}

#[test]
fn resolve_source_add_kind_override_bypasses_classification() {
    // A URL-shaped string can be forced to "path": #116 says `--kind`
    // overrides classification uniformly. (The reverse — forcing a
    // non-URL string to "url" — is rejected; see the scheme-check tests
    // below.)
    let (kind, _) =
        resolve_source_add_kind("https://example.com/page", Some("path"), None, false).unwrap();
    assert_eq!(kind, "path");
}

#[test]
fn resolve_source_add_kind_rejects_kind_url_non_http_arg() {
    // Explicit `--kind url` bypasses classify_source's http(s) shape
    // guarantee; without a scheme check it would persist a url source
    // that can never be indexed (auto-index only warns, exit 0).
    let err = resolve_source_add_kind("/tmp/docs", Some("url"), None, false).unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
    assert!(err.to_string().contains("must be a valid http(s) URL"));
}

#[test]
fn resolve_source_add_kind_rejects_kind_url_unparseable_http_prefixed_arg() {
    // Right prefix, but not a parseable URL (unclosed IPv6 bracket /
    // empty host) — a prefix-only check would persist these.
    for bad in ["https://[", "https://", "http://"] {
        let err = resolve_source_add_kind(bad, Some("url"), None, false).unwrap_err();
        assert!(
            matches!(err, Error::InvalidRequest { .. }),
            "expected InvalidRequest for arg={bad}"
        );
    }
}

#[test]
fn resolve_source_add_kind_accepts_kind_url_http_arg() {
    let (kind, parsed) =
        resolve_source_add_kind("https://example.com/page", Some("url"), None, false).unwrap();
    assert_eq!(kind, "url");
    assert!(parsed.is_none());
}

// --- source list formatting ---

#[test]
fn source_to_human_line_feed_with_max_entries() {
    let row = feed_row(
        "src-1",
        "https://example.com/feed.xml",
        Some(r#"{"max_entries":25,"fetch_full_content":false}"#),
        None,
    );
    let line = source_to_human_line(&row);
    assert_eq!(
        line,
        "src-1 [feed] https://example.com/feed.xml (max_entries=25, full_content=off)"
    );
}

#[test]
fn source_to_human_line_feed_unbounded_defaults() {
    let row = feed_row("src-2", "https://example.com/feed.xml", None, None);
    let line = source_to_human_line(&row);
    assert_eq!(
        line,
        "src-2 [feed] https://example.com/feed.xml (max_entries=unbounded, full_content=on)"
    );
}

#[test]
fn source_to_human_line_path_and_url_unchanged() {
    let row = path_row("src-3", "/tmp/docs");
    assert_eq!(source_to_human_line(&row), "src-3 [path] /tmp/docs");

    let row = url_row("src-4", "https://example.com/page", None);
    assert_eq!(
        source_to_human_line(&row),
        "src-4 [url] https://example.com/page"
    );
}

#[test]
fn source_to_json_value_feed_includes_parsed_fields_and_refresh_not_raw_config_json() {
    let row = feed_row(
        "src-5",
        "https://example.com/feed.xml",
        Some(r#"{"max_entries":10,"fetch_full_content":false}"#),
        Some("1h"),
    );
    let v = source_to_json_value(&row, "notes");
    assert_eq!(v["kind"], "feed");
    assert_eq!(v["max_entries"], 10);
    assert_eq!(v["fetch_full_content"], false);
    assert_eq!(v["refresh"], "1h");
    // Never expose the raw config_json blob.
    assert!(v.get("config_json").is_none());
}

#[test]
fn source_to_json_value_url_surfaces_refresh_but_no_feed_fields() {
    let row = url_row("src-6", "https://example.com/page", Some("30m"));
    let v = source_to_json_value(&row, "notes");
    assert_eq!(v["kind"], "url");
    assert_eq!(v["refresh"], "30m");
    assert!(v.get("max_entries").is_none());
    assert!(v.get("fetch_full_content").is_none());
}

#[test]
fn source_to_json_value_path_has_no_refresh_field() {
    let row = path_row("src-7", "/tmp/docs");
    let v = source_to_json_value(&row, "notes");
    assert_eq!(v["kind"], "path");
    assert!(v.get("refresh").is_none());
}

// -- auto-index embedder reuse (Codex review round 2, finding 6) --------

/// `source add` scoped to two stores must build the (potentially ~706 MB
/// local) embedder once for the whole request, not once per store.
/// Holds `EMBEDDER_BUILD_COUNT_TEST_LOCK` for its whole body — see that
/// lock's doc comment.
///
/// Drives `run_source_add_async` end to end against a real temp DB/config
/// (provider `fake`, so it's fully offline and cheap) and asserts on
/// `crate::cmds::index::EMBEDDER_BUILD_COUNT`, a test-only counter
/// incremented exactly where `run_embedded_index_with` calls
/// `embed::create_embedder`. Before the fix, `source add`'s auto-index
/// loop called the single-store `run_embedded_index` wrapper once per
/// store, rebuilding the embedder each time; this test fails red against
/// that code (count == 2 for two stores) and green once the loop threads
/// one `Arc<dyn Embedder>` across stores via `run_embedded_index_with`,
/// exactly as `run_index_async` already does for `localdb index`.
#[tokio::test]
async fn source_add_across_two_stores_builds_embedder_once() {
    use crate::cmds::index::{EMBEDDER_BUILD_COUNT, EMBEDDER_BUILD_COUNT_TEST_LOCK};
    use crate::cmds::store::run_store_add_async;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    // Held for the rest of this test:
    // `job_attach::tests::run_embedded_store_job_warns_and_continues_on_an_invalid_chunker_preset`
    // also drives a real embedder build and shares this same
    // process-wide counter — without this lock, `cargo test`'s default
    // parallel execution can interleave that test's increment into
    // this one's measurement window (observed: count == 2 instead of
    // 1, indistinguishable from the real per-store-rebuild regression
    // this test exists to catch). See `EMBEDDER_BUILD_COUNT_TEST_LOCK`'s
    // doc comment.
    let _embedder_count_guard = EMBEDDER_BUILD_COUNT_TEST_LOCK.lock().await;

    let dir = TempDir::new().unwrap();
    let note_path = dir.path().join("note.md");
    std::fs::write(&note_path, "# Hello\n\nSome content to index.\n").unwrap();

    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
            dir.path().display()
        ),
    )
    .unwrap();

    let base_ctx = CliContext {
        config: Some(config_path.clone()),
        json: false,
        stores: vec![],
        yes: false,
        daemon_url: None,
        config_env: None,
        api_key: None,
    };
    // Pre-create both stores: `source add`'s explicit `--store` scope
    // requires them to already exist (`resolve_store_scope_inner`).
    run_store_add_async(&base_ctx, "a").await;
    run_store_add_async(&base_ctx, "b").await;

    // Reset just before the call under test: safe against `cargo
    // test`'s parallel test threads only because `_embedder_count_guard`
    // above excludes the one other counter-touching test.
    EMBEDDER_BUILD_COUNT.store(0, Ordering::SeqCst);

    let add_ctx = CliContext {
        config: Some(config_path),
        json: false,
        stores: vec!["a".to_string(), "b".to_string()],
        yes: false,
        daemon_url: None,
        config_env: None,
        api_key: None,
    };
    run_source_add_async(
        &add_ctx,
        note_path.to_str().unwrap(),
        None,
        None,
        None,
        false,
    )
    .await;

    assert_eq!(
        EMBEDDER_BUILD_COUNT.load(Ordering::SeqCst),
        1,
        "auto-indexing 2 stores in one `source add` must build the embedder once, not once per store"
    );
}
