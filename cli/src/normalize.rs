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
/// whitespace into single spaces, then cap at `max_chars`, appending `…` if cut.
pub(crate) fn format_snippet(snippet: &str, max_chars: usize) -> String {
    let normalized = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() > max_chars {
        let truncated: String = normalized.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        normalized
    }
}

/// Print an error and exit with the correct exit code.
pub fn exit_err(err: &Error, json_mode: bool) -> ! {
    let code = err.exit_code();
    if json_mode {
        let v = json!({
            "error": err.code(),
            "message": err.to_string(),
        });
        eprintln!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        eprintln!("error: {}", err);
    }
    std::process::exit(code);
}

pub(crate) fn visibility_to_string(visibility: &StoreVisibility) -> &'static str {
    match visibility {
        StoreVisibility::Private => "private",
        StoreVisibility::Shared => "shared",
    }
}

pub(crate) fn source_kind_to_string(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::Path => "path",
        SourceKind::Url => "url",
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

pub fn source_row_to_core_source(src: &SourceRow) -> localdb_core::types::Source {
    use localdb_core::types::{Source, SourceSpec};

    let spec = match src.kind {
        SourceKind::Url => SourceSpec::Url {
            url: src.url.clone().unwrap_or_default(),
            refresh_interval_secs: None,
        },
        SourceKind::Path => SourceSpec::Path {
            root: src.root.clone().unwrap_or_default(),
            include: src.include.clone(),
            exclude: src.exclude.clone(),
        },
    };

    Source {
        id: src.id.clone(),
        store_id: src.store_id.clone(),
        kind: src.kind.clone(),
        spec,
        source_kind_preset: src.preset.clone(),
    }
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
    fn format_snippet_truncates_long_input() {
        let base: String = "a".repeat(498);
        let input = format!("{base}é extra text that should be cut");
        let result = format_snippet(&input, 500);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 501);
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
        };
        let core = source_row_to_core_source(&src);
        match &core.spec {
            SourceSpec::Url { url, .. } => assert_eq!(url, "https://example.com"),
            _ => panic!("expected url spec"),
        }
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
