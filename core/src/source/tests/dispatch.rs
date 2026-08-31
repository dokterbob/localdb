//! `parse_source_spec` dispatch tests: url-kind parsing plus missing-field
//! and unknown-kind rejection, exercised across source kinds.

use crate::source::kinds;
use crate::source::kinds::tests::common::invalid_request;
use crate::source::{parse_source_spec, source_row_to_source, ParsedSourceSpec};
use crate::types::{SourceKind, SourceSpec};

#[test]
fn parse_source_spec_handles_url_and_rejects_missing_and_unknown_specs() {
    // Given
    let url_spec = serde_json::json!({"url": "https://example.com/page"});
    let missing_root_spec = serde_json::json!({"include": ["**/*.md"]});
    let missing_url_spec = serde_json::json!({});
    let string_field_spec = serde_json::json!({"root": "/tmp/docs", "include": "**/*.md"});

    // When
    let parsed_url = parse_source_spec("url", &url_spec).unwrap();
    let missing_root_err = parse_source_spec("path", &missing_root_spec).unwrap_err();
    let missing_url_err = parse_source_spec("url", &missing_url_spec).unwrap_err();
    let unknown_kind_err = parse_source_spec("rss", &missing_url_spec).unwrap_err();
    let string_field_err = parse_source_spec("path", &string_field_spec).unwrap_err();

    // Then
    assert_eq!(
        parsed_url,
        ParsedSourceSpec {
            kind: SourceKind::Url,
            root: None,
            url: Some("https://example.com/page".to_string()),
            include: Vec::new(),
            exclude: Vec::new(),
            config_json: None,
        }
    );
    assert_eq!(
        missing_root_err,
        invalid_request("path source requires 'root'")
    );
    assert_eq!(
        missing_url_err,
        invalid_request("url source requires 'url'")
    );
    assert_eq!(
        unknown_kind_err,
        invalid_request("unknown source kind 'rss'")
    );
    assert_eq!(
        string_field_err,
        invalid_request("source spec field 'include' must be a JSON array of strings")
    );
}

// ---------------------------------------------------------------------------
// KINDS / kind_def registry (#213 Stage 3): mirrors chunker::formats' FORMATS
// registry tests (order/name snapshot + per-entry consistency).
// ---------------------------------------------------------------------------

#[test]
fn kinds_registry_kind_str_round_trips_through_parse_source_spec() {
    // Given: minimal valid spec JSON per kind string (shapes lifted from the
    // per-kind unit tests in kinds::tests::{path,feed} and this file's own
    // url-kind coverage above).
    let minimal_specs: [(&str, serde_json::Value); 3] = [
        ("path", serde_json::json!({"root": "/tmp/x"})),
        ("url", serde_json::json!({"url": "https://example.com/"})),
        (
            "feed",
            serde_json::json!({"url": "https://example.com/feed.xml"}),
        ),
    ];

    // When / Then: every KINDS entry's kind_str() dispatches through the public
    // parse_source_spec to a ParsedSourceSpec whose kind matches that entry's kind().
    for def in kinds::KINDS {
        let (_, spec) = minimal_specs
            .iter()
            .find(|(kind_str, _)| *kind_str == def.kind_str())
            .unwrap_or_else(|| panic!("no minimal spec fixture for kind_str {:?}", def.kind_str()));
        let parsed = parse_source_spec(def.kind_str(), spec).unwrap();
        assert_eq!(parsed.kind, def.kind());
    }
}

#[test]
fn kinds_registry_has_three_entries_in_dispatch_order_and_kind_def_round_trips() {
    // KINDS order must match parse_source_spec's historical match-arm order (path, url, feed).
    let kind_strs: Vec<&str> = kinds::KINDS.iter().map(|def| def.kind_str()).collect();
    assert_eq!(kind_strs, vec!["path", "url", "feed"]);

    // Force ALL_KINDS to stay exhaustive: adding a SourceKind variant makes this
    // wildcard-free match a compile error, pointing here to extend the list.
    const ALL_KINDS: [SourceKind; 3] = [SourceKind::Path, SourceKind::Url, SourceKind::Feed];
    match ALL_KINDS[0] {
        SourceKind::Path | SourceKind::Url | SourceKind::Feed => {}
    }

    for kind in ALL_KINDS {
        // kind_def is a compile-time-exhaustive match: a new SourceKind variant added without
        // a matching arm fails to compile, not silently falls through at runtime.
        assert_eq!(kinds::kind_def(&kind).kind(), kind);
        // The write-path KINDS array has no such compiler link — a variant missing from it
        // parses as "unknown source kind" at runtime even though the read path supports it,
        // so registry completeness is pinned here instead.
        assert!(
            kinds::KINDS.iter().any(|def| def.kind() == kind),
            "KINDS registry is missing an entry for {kind:?} — \
             parse_source_spec would reject its wire name"
        );
    }
}

// ---------------------------------------------------------------------------
// source_row_to_source (read path): per-kind SourceRow -> Source reconstruction,
// including the tolerant refresh-interval recompute.
// ---------------------------------------------------------------------------

fn row(kind: SourceKind) -> crate::backend::SourceRow {
    crate::backend::SourceRow {
        id: "src-1".to_string(),
        store_id: "store-1".to_string(),
        kind,
        root: None,
        url: None,
        include: Vec::new(),
        exclude: Vec::new(),
        preset: "auto".to_string(),
        refresh: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
    }
}

#[test]
fn source_row_to_source_reconstructs_each_kind() {
    // Given
    let path_row = crate::backend::SourceRow {
        root: Some("/tmp/docs".to_string()),
        include: vec!["**/*.md".to_string()],
        exclude: vec!["**/.git".to_string()],
        ..row(SourceKind::Path)
    };
    let url_row = crate::backend::SourceRow {
        url: Some("https://example.com/page".to_string()),
        refresh: Some("24h".to_string()),
        ..row(SourceKind::Url)
    };
    let feed_row = crate::backend::SourceRow {
        url: Some("https://example.com/feed.xml".to_string()),
        refresh: Some("30m".to_string()),
        config_json: Some(r#"{"max_entries": 10, "fetch_full_content": false}"#.to_string()),
        ..row(SourceKind::Feed)
    };

    // When
    let path_source = source_row_to_source(&path_row);
    let url_source = source_row_to_source(&url_row);
    let feed_source = source_row_to_source(&feed_row);

    // Then
    assert_eq!(path_source.id, "src-1");
    assert_eq!(path_source.store_id, "store-1");
    assert_eq!(path_source.kind, SourceKind::Path);
    assert_eq!(path_source.source_preset, "auto");
    assert_eq!(
        path_source.spec,
        SourceSpec::Path {
            root: "/tmp/docs".to_string(),
            include: vec!["**/*.md".to_string()],
            exclude: vec!["**/.git".to_string()],
        }
    );
    assert_eq!(
        url_source.spec,
        SourceSpec::Url {
            url: "https://example.com/page".to_string(),
            refresh_interval_secs: Some(86_400),
        }
    );
    assert_eq!(
        feed_source.spec,
        SourceSpec::Feed {
            url: "https://example.com/feed.xml".to_string(),
            max_entries: Some(10),
            fetch_full_content: false,
            refresh_interval_secs: Some(1_800),
        }
    );
}

#[test]
fn source_row_to_source_tolerates_invalid_refresh_and_config_json() {
    // Given: stale rows whose refresh/config_json would fail write-time
    // validation — the read path must fall back, never error.
    let url_row = crate::backend::SourceRow {
        url: Some("https://example.com/page".to_string()),
        refresh: Some("soonish".to_string()),
        ..row(SourceKind::Url)
    };
    let feed_row = crate::backend::SourceRow {
        url: Some("https://example.com/feed.xml".to_string()),
        config_json: Some("{not json".to_string()),
        ..row(SourceKind::Feed)
    };

    // When
    let url_source = source_row_to_source(&url_row);
    let feed_source = source_row_to_source(&feed_row);

    // Then
    assert_eq!(
        url_source.spec,
        SourceSpec::Url {
            url: "https://example.com/page".to_string(),
            refresh_interval_secs: None,
        }
    );
    assert_eq!(
        feed_source.spec,
        SourceSpec::Feed {
            url: "https://example.com/feed.xml".to_string(),
            max_entries: None,
            fetch_full_content: true,
            refresh_interval_secs: None,
        }
    );
}
