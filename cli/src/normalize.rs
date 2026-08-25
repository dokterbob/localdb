use localdb_core::{
    types::{SourceKind, StoreVisibility},
    Error, SourceRow,
};
use serde_json::json;

use crate::daemon_client::CliContext;

/// Validate a store name, returning an error for unsafe or invalid names.
///
/// Rejects: empty string, names containing `/`, and names that are exactly `.` or `..`.
/// Returns `Error::InvalidRequest` (exit code 2) on rejection.
pub fn validate_store_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::InvalidRequest {
            message: "store name must not be empty".to_string(),
        });
    }
    if name == "." || name == ".." {
        return Err(Error::InvalidRequest {
            message: format!("store name '{}' is not allowed", name),
        });
    }
    if name.contains('/') || name.contains('\\') {
        return Err(Error::InvalidRequest {
            message: format!("store name '{}' must not contain '/' or '\\'", name),
        });
    }
    Ok(())
}

pub(crate) fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// Format a chunk snippet for terminal display: collapse internal runs of
/// whitespace into single spaces, then apply a boundary-aware soft cap at
/// `max_chars` (see `localdb_core::truncate_snippet`), appending `…` if cut.
///
/// Note: collapsing whitespace first destroys `\n\n` paragraph breaks, so on
/// this path only sentence- and word-boundary snapping can ever fire —
/// paragraph snapping is effectively MCP-only (its text rendering truncates
/// before whitespace collapse would apply, since it has none).
pub(crate) fn format_snippet(snippet: &str, max_chars: usize) -> String {
    let normalized = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    let (body, truncated) = localdb_core::truncate_snippet(&normalized, max_chars);
    if truncated {
        format!("{body}…")
    } else {
        body.to_string()
    }
}

/// Print an error and exit with the correct exit code.
///
/// In `--json` mode the error envelope goes to **stdout**, not stderr —
/// stdout is the only channel a `--json` caller is ever guaranteed to be
/// pure JSON, success or failure (specs/05-surfaces.md §2, issue #260).
/// stderr in `--json` mode carries diagnostics only (progress, warnings) and
/// is never meant to be parsed, so nothing that lands there — including a
/// local-model download's progress output — can corrupt a machine-readable
/// result.
pub fn exit_err(err: &Error, json_mode: bool) -> ! {
    let code = err.exit_code();
    if json_mode {
        print_json(&json!({
            "error": err.code(),
            "message": err.to_string(),
        }));
    } else {
        eprintln!("error: {}", err);
    }
    std::process::exit(code);
}

/// Exit with a partial-batch `--json` result document, preserving whatever
/// per-item results were already buffered.
///
/// A multi-`--store` `--json` loop (`source add`/`add`'s local and
/// daemon-routed branches alike) that fails partway through — after at least
/// one earlier item already succeeded — must not silently discard the
/// buffered results. (The fuller validate-then-persist restructuring across
/// the multi-argument axis is tracked separately as #174.) Mirrors
/// `cmds::index::report_index_outcomes`'s existing pattern: print a
/// `"status"`-tagged JSON document to stdout, then exit explicitly. Both this
/// and plain `exit_err` write to stdout; this helper exists because `results`
/// is additional output data a caller may need to preserve, and `exit_err`'s
/// bare `{"error", "message"}` shape has no field for it.
///
/// Only meaningful in `--json` mode: non-JSON output already prints each
/// success as it happens, so callers should keep using `exit_err` directly
/// when `!ctx.json` — there is nothing buffered to lose.
pub(crate) fn exit_err_with_partial_results(err: &Error, results: Vec<serde_json::Value>) -> ! {
    print_json(&json!({
        "status": "error",
        "error": { "code": err.code(), "message": err.to_string() },
        "results": results,
    }));
    std::process::exit(err.exit_code());
}

pub(crate) fn visibility_to_string(visibility: &StoreVisibility) -> &'static str {
    match visibility {
        StoreVisibility::Private => "private",
        StoreVisibility::Shared => "shared",
    }
}

pub(crate) fn kind_to_string(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::Path => "path",
        SourceKind::Url => "url",
        SourceKind::Feed => "feed",
    }
}

/// Classify a source argument as "path" or "url".
///
/// Returns `(kind, root, url)`.
pub fn classify_source(source: &str) -> (&str, Option<&str>, Option<&str>) {
    if source.starts_with("http://") || source.starts_with("https://") {
        ("url", None, Some(source))
    } else {
        ("path", Some(source), None)
    }
}

/// Determine whether a string looks like a ULID/UUID (not a path or URL).
///
/// ULIDs are 26 uppercase alphanumeric characters. We use this to distinguish
/// bare IDs from path/URL arguments in source remove.
pub(crate) fn looks_like_id(s: &str) -> bool {
    // ULID: exactly 26 chars, all uppercase alphanumeric.
    // UUID: 36 chars with hyphens.
    // Anything containing `/`, `\`, `.` or `://` is a path or URL, not an ID.
    if s.contains('/') || s.contains('\\') || s.contains("://") {
        return false;
    }
    // ULID pattern: 26 uppercase alphanumeric.
    if s.len() == 26
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_lowercase())
    {
        return true;
    }
    // UUID pattern: 32 hex + 4 hyphens = 36 chars.
    if s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return true;
    }
    // Shorter opaque IDs (no path indicators) are also treated as IDs.
    // E.g. numeric IDs or short hex. If it has no path separator or dot, treat
    // as ID only if it's clearly not a filename/relative path.
    false
}

/// Thin delegation to `core::source::source_row_to_source` — kept under its
/// original name so no CLI call site needs to change. The conversion itself
/// is pure (zero I/O) and needs nothing CLI-specific, so it now lives in
/// `core` where `server` can share it too (issue #187).
pub fn source_row_to_core_source(src: &SourceRow) -> localdb_core::types::Source {
    localdb_core::source::source_row_to_source(src)
}

/// Prompt the user for confirmation of a destructive action.
///
/// Returns `true` if confirmed (proceed), `false` if aborted.
/// Exits with code 2 if non-interactive and `--yes` was not given.
pub fn confirm_destructive(ctx: &CliContext, prompt: &str) -> bool {
    use std::io::IsTerminal as _;

    if ctx.yes {
        return true;
    }
    if ctx.json || !std::io::stdin().is_terminal() {
        exit_err(
            &Error::InvalidRequest {
                message: "this command is destructive; re-run with --yes to confirm".to_string(),
            },
            ctx.json,
        );
    }
    eprint!("{} [y/N] ", prompt);
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        eprintln!("Aborted.");
        return false;
    }
    let answer = line.trim().to_lowercase();
    if answer == "y" || answer == "yes" {
        true
    } else {
        eprintln!("Aborted.");
        false
    }
}

#[cfg(test)]
mod tests {
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
        let input = "This is sentence one. This is sentence two that keeps going and going and going further.";
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
}
