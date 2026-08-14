use super::*;

#[test]
fn normalize_path_source_returns_file_parent_and_filename_when_path_is_file() {
    // Given
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("note.md");
    std::fs::write(&file_path, "hello").unwrap();

    // When
    let (root, include, exclude) = normalize_path_source(&file_path.to_string_lossy()).unwrap();

    // Then
    assert_eq!(root, temp_dir.path().to_string_lossy());
    assert_eq!(include, vec!["note.md".to_string()]);
    assert_eq!(exclude, default_path_excludes());
}

#[test]
fn normalize_path_source_returns_error_when_path_is_missing() {
    // Given
    let temp_dir = tempfile::tempdir().unwrap();
    let missing_path = temp_dir.path().join("missing.md");

    // When
    let err = normalize_path_source(&missing_path.to_string_lossy()).unwrap_err();

    // Then
    assert_eq!(
        err,
        invalid_request(&format!(
            "path '{}' does not exist",
            missing_path.to_string_lossy()
        ))
    );
}

#[test]
fn normalize_path_source_returns_directory_defaults_when_path_is_directory() {
    // Given
    let temp_dir = tempfile::tempdir().unwrap();

    // When
    let (root, include, exclude) =
        normalize_path_source(&temp_dir.path().to_string_lossy()).unwrap();

    // Then
    assert_eq!(root, temp_dir.path().to_string_lossy());
    assert_eq!(include, default_path_includes());
    assert_eq!(exclude, default_path_excludes());
}

#[test]
fn parse_source_spec_returns_path_fields_when_path_spec_is_valid() {
    // Given
    let spec = serde_json::json!({
        "root": "/tmp/docs",
        "include": ["**/*.md"],
        "exclude": ["**/.git"],
    });

    // When
    let parsed = parse_source_spec("path", &spec).unwrap();

    // Then
    assert_eq!(
        parsed,
        ParsedSourceSpec {
            kind: SourceKind::Path,
            root: Some("/tmp/docs".to_string()),
            url: None,
            include: vec!["**/*.md".to_string()],
            exclude: vec!["**/.git".to_string()],
            config_json: None,
        }
    );
}

#[test]
fn parse_source_spec_returns_error_when_array_field_contains_non_string() {
    // Given
    let spec = serde_json::json!({"root": "/tmp/docs", "include": [42]});

    // When
    let err = parse_source_spec("path", &spec).unwrap_err();

    // Then
    assert_eq!(
        err,
        invalid_request("source spec field 'include' contains a non-string value")
    );
}

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

// --- parse_source_spec: feed arm ---

#[test]
fn parse_source_spec_feed_valid_defaults() {
    let spec = serde_json::json!({"url": "https://example.com/feed.xml"});
    let parsed = parse_source_spec("feed", &spec).unwrap();
    assert_eq!(parsed.kind, SourceKind::Feed);
    assert_eq!(parsed.url, Some("https://example.com/feed.xml".to_string()));
    assert_eq!(parsed.root, None);
    assert!(parsed.include.is_empty());
    assert!(parsed.exclude.is_empty());
    let config = parse_feed_config_json(parsed.config_json.as_deref());
    assert_eq!(config.max_entries, None);
    assert!(config.fetch_full_content);
}

#[test]
fn parse_source_spec_feed_valid_with_explicit_fields() {
    let spec = serde_json::json!({
        "url": "http://example.com/feed.xml",
        "max_entries": 25,
        "fetch_full_content": false,
    });
    let parsed = parse_source_spec("feed", &spec).unwrap();
    let config = parse_feed_config_json(parsed.config_json.as_deref());
    assert_eq!(config.max_entries, Some(25));
    assert!(!config.fetch_full_content);
}

#[test]
fn parse_source_spec_feed_missing_url_rejected() {
    let spec = serde_json::json!({});
    let err = parse_source_spec("feed", &spec).unwrap_err();
    assert_eq!(err, invalid_request("feed source requires 'url'"));
}

#[test]
fn parse_source_spec_feed_non_http_url_rejected() {
    let spec = serde_json::json!({"url": "ftp://example.com/feed.xml"});
    let err = parse_source_spec("feed", &spec).unwrap_err();
    assert_eq!(
        err,
        invalid_request(
            "feed source 'url' must be a valid http(s) URL: 'ftp://example.com/feed.xml'"
        )
    );
}

/// Prefix-only validation (`starts_with("https://")`) would accept these:
/// they carry the right scheme prefix but fail a full `url::Url::parse`
/// (empty host, unclosed IPv6 bracket) — a persisted row would then fail
/// every index run whole-source at the ingestor's fail-fast parse.
#[test]
fn parse_source_spec_feed_unparseable_http_prefixed_url_rejected() {
    for bad in ["https://", "https://[", "http://"] {
        let spec = serde_json::json!({ "url": bad });
        let err = parse_source_spec("feed", &spec).unwrap_err();
        assert!(
            matches!(err, Error::InvalidRequest { .. }),
            "expected InvalidRequest for url={bad}"
        );
    }
}

#[test]
fn parse_source_spec_feed_mailto_url_rejected() {
    let spec = serde_json::json!({"url": "mailto:x@y"});
    let err = parse_source_spec("feed", &spec).unwrap_err();
    assert_eq!(
        err,
        invalid_request("feed source 'url' must be a valid http(s) URL: 'mailto:x@y'")
    );
}

/// A present, non-null `fetch_full_content` that is not a JSON boolean
/// must be rejected — `as_bool()` alone treats the string "false" as
/// absent and silently enables discovery mode against the caller's
/// stated intent (HTTP surface; clap guards the CLI).
#[test]
fn parse_source_spec_feed_non_bool_fetch_full_content_rejected() {
    for bad in [
        serde_json::json!("false"),
        serde_json::json!(0),
        serde_json::json!([true]),
    ] {
        let spec = serde_json::json!({
            "url": "https://example.com/feed.xml",
            "fetch_full_content": bad,
        });
        let err = parse_source_spec("feed", &spec).unwrap_err();
        assert!(
            matches!(err, Error::InvalidRequest { .. }),
            "expected InvalidRequest for fetch_full_content={bad}"
        );
    }
}

#[test]
fn parse_source_spec_feed_explicit_null_fetch_full_content_is_default_true() {
    let spec = serde_json::json!({
        "url": "https://example.com/feed.xml",
        "fetch_full_content": null,
    });
    let parsed = parse_source_spec("feed", &spec).unwrap();
    let config = parse_feed_config_json(parsed.config_json.as_deref());
    assert!(config.fetch_full_content);
}

#[test]
fn parse_source_spec_feed_max_entries_zero_rejected() {
    let spec = serde_json::json!({
        "url": "https://example.com/feed.xml",
        "max_entries": 0,
    });
    let err = parse_source_spec("feed", &spec).unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

/// A present, non-null `max_entries` that is not a u32-representable
/// integer must be rejected, never silently truncated (4294967297 -> 1)
/// or treated as absent (negative/float/string). Only reachable via the
/// HTTP surface — clap's u32 parser guards the CLI — but this arm is the
/// single validation authority for both.
#[test]
fn parse_source_spec_feed_max_entries_non_u32_rejected_not_truncated() {
    for bad in [
        serde_json::json!(u64::from(u32::MAX) + 2),
        serde_json::json!(-5),
        serde_json::json!(2.5),
        serde_json::json!("25"),
    ] {
        let spec = serde_json::json!({
            "url": "https://example.com/feed.xml",
            "max_entries": bad,
        });
        let err = parse_source_spec("feed", &spec).unwrap_err();
        assert!(
            matches!(err, Error::InvalidRequest { .. }),
            "expected InvalidRequest for max_entries={bad}"
        );
    }
}

#[test]
fn parse_source_spec_feed_max_entries_explicit_null_is_unbounded() {
    let spec = serde_json::json!({
        "url": "https://example.com/feed.xml",
        "max_entries": null,
    });
    let parsed = parse_source_spec("feed", &spec).unwrap();
    let config = parse_feed_config_json(parsed.config_json.as_deref());
    assert_eq!(config.max_entries, None);
    assert!(config.fetch_full_content);
}

// --- parse_feed_config_json / build_feed_config_json ---

#[test]
fn parse_feed_config_json_null_returns_defaults() {
    let config = parse_feed_config_json(None);
    assert_eq!(config, FeedConfig::default());
}

#[test]
fn parse_feed_config_json_empty_string_returns_defaults() {
    let config = parse_feed_config_json(Some(""));
    assert_eq!(config, FeedConfig::default());
    let config_ws = parse_feed_config_json(Some("   "));
    assert_eq!(config_ws, FeedConfig::default());
}

#[test]
fn parse_feed_config_json_malformed_json_returns_defaults() {
    let config = parse_feed_config_json(Some("{not valid json"));
    assert_eq!(config, FeedConfig::default());
}

#[test]
fn parse_feed_config_json_wrong_shape_returns_defaults() {
    assert_eq!(
        parse_feed_config_json(Some("[1,2,3]")),
        FeedConfig::default()
    );
    assert_eq!(parse_feed_config_json(Some("42")), FeedConfig::default());
    assert_eq!(
        parse_feed_config_json(Some("\"just a string\"")),
        FeedConfig::default()
    );
}

#[test]
fn parse_feed_config_json_valid_populated() {
    let config =
        parse_feed_config_json(Some(r#"{"max_entries": 10, "fetch_full_content": false}"#));
    assert_eq!(
        config,
        FeedConfig {
            max_entries: Some(10),
            fetch_full_content: false,
        }
    );
}

#[test]
fn parse_feed_config_json_valid_null_max_entries() {
    let config =
        parse_feed_config_json(Some(r#"{"max_entries": null, "fetch_full_content": true}"#));
    assert_eq!(
        config,
        FeedConfig {
            max_entries: None,
            fetch_full_content: true,
        }
    );
}

#[test]
fn build_feed_config_json_round_trips_through_parse() {
    let json = build_feed_config_json(Some(7), false);
    let config = parse_feed_config_json(Some(&json));
    assert_eq!(
        config,
        FeedConfig {
            max_entries: Some(7),
            fetch_full_content: false,
        }
    );
}

#[test]
fn build_feed_config_json_none_max_entries_round_trips() {
    let json = build_feed_config_json(None, true);
    let config = parse_feed_config_json(Some(&json));
    assert_eq!(config, FeedConfig::default());
}

fn default_path_includes() -> Vec<String> {
    DEFAULT_PATH_INCLUDES
        .iter()
        .map(|value| value.to_string())
        .collect()
}

fn default_path_excludes() -> Vec<String> {
    DEFAULT_PATH_EXCLUDES
        .iter()
        .map(|value| value.to_string())
        .collect()
}

fn invalid_request(message: &str) -> Error {
    Error::InvalidRequest {
        message: message.to_string(),
    }
}
