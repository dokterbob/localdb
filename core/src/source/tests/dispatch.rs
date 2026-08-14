//! `parse_source_spec` dispatch tests: url-kind parsing plus missing-field
//! and unknown-kind rejection, exercised across source kinds.

use crate::source::kinds;
use crate::source::kinds::tests::common::invalid_request;
use crate::source::{parse_source_spec, ParsedSourceSpec};
use crate::types::SourceKind;

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

    // kind_def is a compile-time-exhaustive match: a new SourceKind variant added without a
    // matching arm fails to compile, not silently falls through at runtime.
    for kind in [SourceKind::Path, SourceKind::Url, SourceKind::Feed] {
        assert_eq!(kinds::kind_def(&kind).kind(), kind);
    }
}
