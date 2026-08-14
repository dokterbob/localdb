//! `parse_source_spec` dispatch tests: url-kind parsing plus missing-field
//! and unknown-kind rejection, exercised across source kinds.

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
