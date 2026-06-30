use crate::error::Error;
use crate::types::SourceKind;
use std::path::Path;

/// Default include patterns for path sources (#7).
///
/// Generated from `extract::supported_extensions()`: plain extension tokens
/// (no `.`) become `**/*.ext`; basename tokens (contain `.`) become
/// `**/<basename>`.
pub const DEFAULT_PATH_INCLUDES: &[&str] = &[
    // Markdown
    "**/*.md",
    "**/*.markdown",
    // HTML
    "**/*.html",
    "**/*.htm",
    // PDF
    "**/*.pdf",
    // EPUB / ebook
    "**/*.epub",
    // Office formats
    "**/*.docx",
    "**/*.xlsx",
    "**/*.pptx",
    "**/*.odt",
    "**/*.ods",
    "**/*.odp",
    // Plaintext prose
    "**/*.txt",
    "**/*.text",
    // Code / data
    "**/*.rs",
    "**/*.py",
    "**/*.js",
    "**/*.mjs",
    "**/*.ts",
    "**/*.tsx",
    "**/*.json",
    "**/*.yaml",
    "**/*.yml",
    "**/*.toml",
    "**/*.lock",
    "**/*.c",
    "**/*.h",
    "**/*.cpp",
    "**/*.hpp",
    "**/*.go",
    "**/*.java",
    "**/*.rb",
    "**/*.php",
    "**/*.sh",
    "**/*.css",
    "**/*.scss",
    "**/*.sql",
    "**/*.csv",
    "**/*.xml",
    "**/*.ini",
    "**/*.cfg",
    // Lockfile basenames
    "**/Cargo.lock",
    "**/package-lock.json",
    "**/yarn.lock",
    "**/poetry.lock",
    "**/Gemfile.lock",
];

/// Default exclude patterns for path sources (#4).
///
/// These patterns are matched against both the root-relative path and the bare
/// basename of each entry (see `enumerate_dir` in `core`), so a pattern like
/// `**/.git` prunes a `.git` directory at any depth before recursing into it.
/// Using `**/X` (without a trailing `/**`) matches the entry itself; the subtree
/// is never walked.  For single-file junk (`.DS_Store`) the same form works as a
/// file-pattern.
///
/// **Include** globs are still anchored to the source root and NOT affected by
/// this floating-basename rule.
pub const DEFAULT_PATH_EXCLUDES: &[&str] = &[
    "**/.git",
    "**/node_modules",
    "**/.DS_Store",
    "**/target",
    "**/__pycache__",
    "**/.venv",
];

pub type ParsedSourceSpec = (
    SourceKind,
    Option<String>,
    Option<String>,
    Vec<String>,
    Vec<String>,
);

/// Normalize a path source into root/include/exclude fields.
///
/// # Errors
/// Returns `Error::InvalidRequest` if `raw_path` does not exist.
pub fn normalize_path_source(raw_path: &str) -> Result<(String, Vec<String>, Vec<String>), Error> {
    let p = Path::new(raw_path);

    if !p.exists() {
        return Err(Error::InvalidRequest {
            message: format!("path '{}' does not exist", raw_path),
        });
    }

    let (root, include_globs) = if p.is_file() {
        // #7: single-file source — use parent dir as root, include only this file.
        let parent = p
            .parent()
            .map(|par| {
                if par == Path::new("") {
                    Path::new(".")
                } else {
                    par
                }
            })
            .unwrap_or(Path::new("."));
        let filename = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (parent.to_string_lossy().to_string(), vec![filename])
    } else {
        // Directory source: apply the default include allowlist so that only
        // files with supported extensions are visited.  Callers that need to
        // override this can set explicit include globs after construction.
        let includes = DEFAULT_PATH_INCLUDES
            .iter()
            .map(|s| s.to_string())
            .collect();
        (raw_path.to_string(), includes)
    };

    // #4: apply default excludes for path sources.
    let exclude_globs: Vec<String> = DEFAULT_PATH_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .collect();

    Ok((root, include_globs, exclude_globs))
}

/// Parse a JSON source spec by kind.
///
/// # Errors
/// Returns `Error::InvalidRequest` if required fields are missing or malformed.
pub fn parse_source_spec(kind: &str, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
    match kind {
        "path" => {
            let root = spec
                .get("root")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: "path source requires 'root'".to_string(),
                })?;
            let include = string_array_field(spec, "include")?;
            let exclude = string_array_field(spec, "exclude")?;
            Ok((SourceKind::Path, Some(root), None, include, exclude))
        }
        "url" => {
            let url = spec
                .get("url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: "url source requires 'url'".to_string(),
                })?;
            Ok((SourceKind::Url, None, Some(url), Vec::new(), Vec::new()))
        }
        other => Err(Error::InvalidRequest {
            message: format!("unknown source kind '{other}'"),
        }),
    }
}

pub(crate) fn string_array_field(
    spec: &serde_json::Value,
    field: &str,
) -> Result<Vec<String>, Error> {
    let Some(raw) = spec.get(field) else {
        return Ok(Vec::new());
    };
    let arr = raw.as_array().ok_or_else(|| Error::InvalidRequest {
        message: format!("source spec field '{field}' must be a JSON array of strings"),
    })?;
    arr.iter()
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: format!("source spec field '{field}' contains a non-string value"),
                })
        })
        .collect()
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_source_returns_file_parent_and_filename_when_path_is_file() { let temp_dir = tempfile::tempdir().unwrap(); let file_path = temp_dir.path().join("note.md"); std::fs::write(&file_path, "hello").unwrap(); let (root, include, exclude) = normalize_path_source(&file_path.to_string_lossy()).unwrap(); assert_eq!(root, temp_dir.path().to_string_lossy()); assert_eq!(include, vec!["note.md".to_string()]); assert_eq!(exclude, DEFAULT_PATH_EXCLUDES.iter().map(|value| value.to_string()).collect::<Vec<_>>()); }

    #[test]
    fn normalize_path_source_returns_error_when_path_is_missing() { let temp_dir = tempfile::tempdir().unwrap(); let missing_path = temp_dir.path().join("missing.md"); let err = normalize_path_source(&missing_path.to_string_lossy()).unwrap_err(); assert_eq!(err, invalid_request(&format!("path '{}' does not exist", missing_path.to_string_lossy()))); }

    #[test]
    fn normalize_path_source_returns_directory_defaults_when_path_is_directory() { let temp_dir = tempfile::tempdir().unwrap(); let (root, include, exclude) = normalize_path_source(&temp_dir.path().to_string_lossy()).unwrap(); assert_eq!(root, temp_dir.path().to_string_lossy()); assert_eq!(include, DEFAULT_PATH_INCLUDES.iter().map(|value| value.to_string()).collect::<Vec<_>>()); assert_eq!(exclude, DEFAULT_PATH_EXCLUDES.iter().map(|value| value.to_string()).collect::<Vec<_>>()); }

    #[test]
    fn parse_source_spec_returns_path_fields_when_path_spec_is_valid() { let spec = serde_json::json!({"root": "/tmp/docs", "include": ["**/*.md"], "exclude": ["**/.git"]}); let parsed = parse_source_spec("path", &spec).unwrap(); assert_eq!(parsed, (SourceKind::Path, Some("/tmp/docs".to_string()), None, vec!["**/*.md".to_string()], vec!["**/.git".to_string()])); }

    #[test]
    fn parse_source_spec_returns_error_when_array_field_contains_non_string() { let spec = serde_json::json!({"root": "/tmp/docs", "include": [42]}); let err = parse_source_spec("path", &spec).unwrap_err(); assert_eq!(err, invalid_request("source spec field 'include' contains a non-string value")); }

    #[test]
    fn parse_source_spec_handles_url_and_rejects_missing_and_unknown_specs() { let url_spec = serde_json::json!({"url": "https://example.com/page"}); let missing_root_spec = serde_json::json!({"include": ["**/*.md"]}); let missing_url_spec = serde_json::json!({}); let string_field_spec = serde_json::json!({"root": "/tmp/docs", "include": "**/*.md"}); let parsed_url = parse_source_spec("url", &url_spec).unwrap(); let missing_root_err = parse_source_spec("path", &missing_root_spec).unwrap_err(); let missing_url_err = parse_source_spec("url", &missing_url_spec).unwrap_err(); let unknown_kind_err = parse_source_spec("rss", &missing_url_spec).unwrap_err(); let string_field_err = parse_source_spec("path", &string_field_spec).unwrap_err(); assert_eq!(parsed_url, (SourceKind::Url, None, Some("https://example.com/page".to_string()), Vec::new(), Vec::new())); assert_eq!(missing_root_err, invalid_request("path source requires 'root'")); assert_eq!(missing_url_err, invalid_request("url source requires 'url'")); assert_eq!(unknown_kind_err, invalid_request("unknown source kind 'rss'")); assert_eq!(string_field_err, invalid_request("source spec field 'include' must be a JSON array of strings")); }

    fn invalid_request(message: &str) -> Error { Error::InvalidRequest { message: message.to_string() } }
}
