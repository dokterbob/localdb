use super::*;
use localdb_core::ingestion::now_rfc3339;
use tempfile::TempDir;

#[test]
fn format_snippet_collapses_whitespace() {
    assert_eq!(format_snippet("a\n\n  b   c", 500), "a b c");
}

#[test]
fn format_snippet_truncates_long_input_at_boundary() {
    let base: String = "a".repeat(498);
    let input = format!("{base}é extra text that should be cut");
    let result = format_snippet(&input, 500);
    assert!(result.ends_with('…'));
    // Boundary-aware: no longer an exact 501-char hard cut. The result
    // (minus the appended ellipsis) must respect the soft-cap overshoot
    // bound from `localdb_core::truncate_snippet`.
    assert!(result.chars().count() <= 500 + 500 / 5 + 1);
    assert!(!result.is_empty());
}

#[test]
fn format_snippet_snaps_to_sentence_boundary() {
    let input =
        "This is sentence one. This is sentence two that keeps going and going and going further.";
    let result = format_snippet(input, 25);
    assert!(result.ends_with('.') || result.ends_with("…"));
    assert!(result.starts_with("This is sentence one."));
}

#[test]
fn format_snippet_snaps_to_word_boundary() {
    let input = "word ".repeat(100);
    let result = format_snippet(&input, 50);
    assert!(result.ends_with('…'));
    // No mid-word cut: strip the ellipsis and confirm the remainder ends
    // on a full "word" token, not a partial fragment like "wor".
    let body = result.trim_end_matches('…');
    assert!(
        body.ends_with("word") || body.is_empty(),
        "expected a full-word ending, got: {body}"
    );
}

#[test]
fn classify_sources() {
    assert_eq!(
        classify_source("/home/user/docs"),
        ("path", Some("/home/user/docs"), None)
    );
    assert_eq!(
        classify_source("https://example.com/page"),
        ("url", None, Some("https://example.com/page"))
    );
    assert_eq!(
        classify_source("http://localhost/doc"),
        ("url", None, Some("http://localhost/doc"))
    );
}

#[test]
fn convert_path_source() {
    use localdb_core::types::SourceSpec;
    let src = SourceRow {
        id: "src-1".into(),
        store_id: "store-id".into(),
        kind: SourceKind::Path,
        root: Some("/tmp/docs".into()),
        url: None,
        include: vec!["**/*.md".into()],
        exclude: vec![],
        preset: "prose".into(),
        refresh: None,
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    };
    let core = source_row_to_core_source(&src);
    assert_eq!(core.id, "src-1");
    match &core.spec {
        SourceSpec::Path { root, include, .. } => {
            assert_eq!(root, "/tmp/docs");
            assert_eq!(include, &vec!["**/*.md".to_string()]);
        }
        _ => panic!("expected path spec"),
    }
}

#[test]
fn convert_url_source() {
    use localdb_core::types::SourceSpec;
    let src = SourceRow {
        id: "src-2".into(),
        store_id: "store-id".into(),
        kind: SourceKind::Url,
        root: None,
        url: Some("https://example.com".into()),
        include: vec![],
        exclude: vec![],
        preset: "prose".into(),
        refresh: None,
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    };
    let core = source_row_to_core_source(&src);
    match &core.spec {
        SourceSpec::Url { url, .. } => assert_eq!(url, "https://example.com"),
        _ => panic!("expected url spec"),
    }
}

#[test]
fn convert_url_source_parses_refresh_column_into_interval_secs() {
    use localdb_core::types::SourceSpec;
    let src = SourceRow {
        id: "src-2b".into(),
        store_id: "store-id".into(),
        kind: SourceKind::Url,
        root: None,
        url: Some("https://example.com".into()),
        include: vec![],
        exclude: vec![],
        preset: "prose".into(),
        refresh: Some("24h".into()),
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    };
    let core = source_row_to_core_source(&src);
    match &core.spec {
        SourceSpec::Url {
            refresh_interval_secs,
            ..
        } => assert_eq!(*refresh_interval_secs, Some(86400)),
        _ => panic!("expected url spec"),
    }
}

#[test]
fn convert_url_source_tolerates_invalid_refresh_string() {
    // Defensive: a row that somehow holds an invalid refresh string
    // (should never happen post-validation) must not panic on read —
    // it falls back to `None` rather than erroring reconstruction.
    use localdb_core::types::SourceSpec;
    let src = SourceRow {
        id: "src-2c".into(),
        store_id: "store-id".into(),
        kind: SourceKind::Url,
        root: None,
        url: Some("https://example.com".into()),
        include: vec![],
        exclude: vec![],
        preset: "prose".into(),
        refresh: Some("not-a-duration".into()),
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    };
    let core = source_row_to_core_source(&src);
    match &core.spec {
        SourceSpec::Url {
            refresh_interval_secs,
            ..
        } => assert_eq!(*refresh_interval_secs, None),
        _ => panic!("expected url spec"),
    }
}

#[test]
fn convert_feed_source_reconstructs_spec_from_config_json_and_refresh() {
    use localdb_core::types::SourceSpec;
    let src = SourceRow {
        id: "src-3".into(),
        store_id: "store-id".into(),
        kind: SourceKind::Feed,
        root: None,
        url: Some("https://example.com/feed.xml".into()),
        include: vec![],
        exclude: vec![],
        preset: "prose".into(),
        refresh: Some("1h".into()),
        created_at: now_rfc3339(),
        config_json: Some(r#"{"max_entries":25,"fetch_full_content":false}"#.into()),
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    };
    let core = source_row_to_core_source(&src);
    assert_eq!(core.kind, SourceKind::Feed);
    match &core.spec {
        SourceSpec::Feed {
            url,
            max_entries,
            fetch_full_content,
            refresh_interval_secs,
        } => {
            assert_eq!(url, "https://example.com/feed.xml");
            assert_eq!(*max_entries, Some(25));
            assert!(!fetch_full_content);
            assert_eq!(*refresh_interval_secs, Some(3600));
        }
        _ => panic!("expected feed spec"),
    }
}

#[test]
fn convert_feed_source_tolerates_null_config_json() {
    use localdb_core::types::SourceSpec;
    let src = SourceRow {
        id: "src-4".into(),
        store_id: "store-id".into(),
        kind: SourceKind::Feed,
        root: None,
        url: Some("https://example.com/feed.xml".into()),
        include: vec![],
        exclude: vec![],
        preset: "prose".into(),
        refresh: None,
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    };
    let core = source_row_to_core_source(&src);
    match &core.spec {
        SourceSpec::Feed {
            max_entries,
            fetch_full_content,
            refresh_interval_secs,
            ..
        } => {
            assert_eq!(*max_entries, None);
            assert!(fetch_full_content, "must default to true");
            assert_eq!(*refresh_interval_secs, None);
        }
        _ => panic!("expected feed spec"),
    }
}

#[test]
fn convert_feed_source_tolerates_malformed_config_json() {
    use localdb_core::types::SourceSpec;
    let src = SourceRow {
        id: "src-5".into(),
        store_id: "store-id".into(),
        kind: SourceKind::Feed,
        root: None,
        url: Some("https://example.com/feed.xml".into()),
        include: vec![],
        exclude: vec![],
        preset: "prose".into(),
        refresh: None,
        created_at: now_rfc3339(),
        config_json: Some("{not valid json".into()),
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    };
    let core = source_row_to_core_source(&src);
    match &core.spec {
        SourceSpec::Feed {
            max_entries,
            fetch_full_content,
            ..
        } => {
            assert_eq!(*max_entries, None);
            assert!(
                fetch_full_content,
                "malformed config_json must fall back to true"
            );
        }
        _ => panic!("expected feed spec"),
    }
}

#[test]
fn kind_to_string_maps_all_kinds() {
    assert_eq!(kind_to_string(&SourceKind::Path), "path");
    assert_eq!(kind_to_string(&SourceKind::Url), "url");
    assert_eq!(kind_to_string(&SourceKind::Feed), "feed");
}

#[test]
fn validate_store_name_rejects_invalid_and_accepts_valid_names() {
    assert_eq!(validate_store_name("").unwrap_err().exit_code(), 2);
    assert_eq!(validate_store_name(".").unwrap_err().exit_code(), 2);
    assert_eq!(validate_store_name("..").unwrap_err().exit_code(), 2);
    assert_eq!(validate_store_name("a/b").unwrap_err().exit_code(), 2);
    assert_eq!(validate_store_name("a\\b").unwrap_err().exit_code(), 2);
    assert!(validate_store_name("my_store_123").is_ok());
}

#[test]
fn looks_like_id_recognizes_ulid_and_rejects_paths() {
    assert!(looks_like_id("01HRQHB7FN3WMX4AZDV3S9VCTZ"));
    assert!(!looks_like_id("/home/user/docs"));
    assert!(!looks_like_id("https://example.com"));
    assert!(!looks_like_id("some/path"));
}

#[test]
fn confirm_destructive_yes_flag_skips_prompt() {
    let ctx = CliContext {
        config: None,
        json: false,
        stores: vec![],
        yes: true,
        daemon_url: None,
        config_env: None,
        api_key: None,
    };
    assert!(confirm_destructive(&ctx, "Are you sure?"));
}

#[test]
fn normalize_path_source_directory_has_default_includes() {
    let dir = TempDir::new().unwrap();
    let (root, include, exclude) =
        localdb_core::source::normalize_path_source(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(root, dir.path().to_str().unwrap());
    assert!(include.iter().any(|s| s == "**/*.md"));
    assert!(exclude.iter().any(|s| s == "**/.git"));
}
