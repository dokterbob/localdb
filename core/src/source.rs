use crate::config::validate_max_entries;
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

/// Result of [`parse_source_spec`]: the kind-specific fields needed to build
/// a `SourceRow`, in one named struct (issue #116 — previously an unlabeled
/// 5-tuple, which grew a 6th field awkwardly as `config_json` was added).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSourceSpec {
    pub kind: SourceKind,
    pub root: Option<String>,
    pub url: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Kind-specific JSON config blob for `SourceRow.config_json`. Populated
    /// for feed sources (see [`build_feed_config_json`]); `None` for path
    /// and url sources.
    pub config_json: Option<String>,
}

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
            Ok(ParsedSourceSpec {
                kind: SourceKind::Path,
                root: Some(root),
                url: None,
                include,
                exclude,
                config_json: None,
            })
        }
        "url" => {
            let url = spec
                .get("url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: "url source requires 'url'".to_string(),
                })?;
            Ok(ParsedSourceSpec {
                kind: SourceKind::Url,
                root: None,
                url: Some(url),
                include: Vec::new(),
                exclude: Vec::new(),
                config_json: None,
            })
        }
        "feed" => {
            let url = spec
                .get("url")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| Error::InvalidRequest {
                    message: "feed source requires 'url'".to_string(),
                })?;
            // Full parse, not a prefix check: `https://[` and bare `https://`
            // start with the right prefix but fail `url::Url::parse`, and a
            // prefix-validated row would persist a source whose every index
            // run fails whole-source at the ingestor's fail-fast Uri::parse.
            let scheme_ok = crate::uri::Uri::parse(&url)
                .is_some_and(|u| matches!(u.scheme(), "http" | "https"));
            if !scheme_ok {
                return Err(Error::InvalidRequest {
                    message: format!("feed source 'url' must be a valid http(s) URL: '{url}'"),
                });
            }
            // Strict decode: a present, non-null `max_entries` must be an
            // integer that fits u32. `as_u64()` alone would silently treat
            // negatives/floats as absent and `as u32` would truncate huge
            // values (e.g. 4294967297 -> 1), mutating the caller's stated
            // intent instead of rejecting it — this arm is the single
            // validation authority for both CLI and HTTP surfaces.
            let max_entries = match spec.get("max_entries") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => match v.as_u64().filter(|&n| n <= u64::from(u32::MAX)) {
                    Some(n) => Some(n as u32),
                    None => {
                        return Err(Error::InvalidRequest {
                            message: format!(
                                "feed source 'max_entries' must be a positive integer no \
                                 greater than {}: {v}",
                                u32::MAX
                            ),
                        })
                    }
                },
            };
            let max_entries = validate_max_entries(max_entries)?;
            // Strict decode, mirroring `max_entries`: `as_bool()` alone would
            // treat a mistyped value (e.g. the string "false") as absent and
            // silently default discovery mode ON against the caller's stated
            // intent.
            let fetch_full_content = match spec.get("fetch_full_content") {
                None | Some(serde_json::Value::Null) => true,
                Some(serde_json::Value::Bool(b)) => *b,
                Some(v) => {
                    return Err(Error::InvalidRequest {
                        message: format!("feed source 'fetch_full_content' must be a boolean: {v}"),
                    })
                }
            };
            let config_json = build_feed_config_json(max_entries, fetch_full_content);
            Ok(ParsedSourceSpec {
                kind: SourceKind::Feed,
                root: None,
                url: Some(url),
                include: Vec::new(),
                exclude: Vec::new(),
                config_json: Some(config_json),
            })
        }
        other => Err(Error::InvalidRequest {
            message: format!("unknown source kind '{other}'"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Feed config_json — tolerant parse + inverse builder
// ---------------------------------------------------------------------------

/// Feed-source config decoded from `SourceRow.config_json`.
///
/// `Default` matches [`parse_feed_config_json`]'s fallback: unbounded entries,
/// full-content fetch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedConfig {
    pub max_entries: Option<u32>,
    pub fetch_full_content: bool,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            max_entries: None,
            fetch_full_content: true,
        }
    }
}

/// Tolerantly parse a feed source's `config_json` column.
///
/// `NULL` (`None`), an empty/whitespace-only string, syntactically invalid
/// JSON, or validly-parsed JSON of the wrong shape (not a JSON object, or
/// missing/mistyped fields) all fall back to [`FeedConfig::default`] rather
/// than erroring — a corrupt or stale config_json must never fail a source
/// read. Shared by `cli::normalize` and `server::state` so this tolerance
/// lives in exactly one place (issue #116).
pub fn parse_feed_config_json(config_json: Option<&str>) -> FeedConfig {
    let Some(raw) = config_json else {
        return FeedConfig::default();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return FeedConfig::default();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return FeedConfig::default();
    };
    let Some(obj) = value.as_object() else {
        return FeedConfig::default();
    };
    let max_entries = obj
        .get("max_entries")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let fetch_full_content = obj
        .get("fetch_full_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    FeedConfig {
        max_entries,
        fetch_full_content,
    }
}

/// Build the `config_json` string for a feed source's `SourceRow`.
///
/// Inverse of [`parse_feed_config_json`]. `refresh_interval_secs` is
/// deliberately NOT a parameter — it is persisted in `SourceRow.refresh`
/// instead (see `SourceRow::config_json` doc comment).
pub fn build_feed_config_json(max_entries: Option<u32>, fetch_full_content: bool) -> String {
    serde_json::json!({
        "max_entries": max_entries,
        "fetch_full_content": fetch_full_content,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// SourceRow -> Source (read path)
// ---------------------------------------------------------------------------

/// Reconstruct a domain [`crate::types::Source`] from its persisted
/// [`crate::backend::SourceRow`] form.
///
/// Pure, zero I/O — the mirror image of [`parse_source_spec`], which goes the
/// other way (request JSON -> `ParsedSourceSpec` -> `SourceRow`). Shared by
/// every surface that reads sources back out of a `StoreBackend` (currently
/// `cli::normalize::source_row_to_core_source`, which re-exports this
/// unchanged; `server` builds its own JSON shape via `source_row_to_record`
/// instead, since the HTTP wire format differs from the domain `Source`
/// type).
pub fn source_row_to_source(row: &crate::backend::SourceRow) -> crate::types::Source {
    use crate::types::{Source, SourceSpec};

    // C5: `refresh` is stored as the raw human-readable string the user gave
    // `localdb source add --refresh` (e.g. "24h"), validated at write time
    // but never converted to seconds for storage — the seconds value must be
    // recomputed here on every read. Tolerant: a row that somehow holds an
    // invalid string (should never happen post-validation, but this is a
    // read path and must not panic/error on stale data) falls back to `None`
    // rather than failing the whole reconstruction.
    let refresh_interval_secs = row
        .refresh
        .as_deref()
        .and_then(|s| crate::config::validate_refresh_interval(s).ok())
        .flatten();

    let spec = match row.kind {
        SourceKind::Url => SourceSpec::Url {
            url: row.url.clone().unwrap_or_default(),
            refresh_interval_secs,
        },
        SourceKind::Path => SourceSpec::Path {
            root: row.root.clone().unwrap_or_default(),
            include: row.include.clone(),
            exclude: row.exclude.clone(),
        },
        SourceKind::Feed => {
            let feed_config = parse_feed_config_json(row.config_json.as_deref());
            SourceSpec::Feed {
                url: row.url.clone().unwrap_or_default(),
                max_entries: feed_config.max_entries,
                fetch_full_content: feed_config.fetch_full_content,
                refresh_interval_secs,
            }
        }
    };

    Source {
        id: row.id.clone(),
        store_id: row.store_id.clone(),
        kind: row.kind.clone(),
        spec,
        source_preset: row.preset.clone(),
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
mod tests {
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
}
