use crate::backend::SourceRow;
use crate::error::Error;
use crate::source::kinds::SourceKindDef;
use crate::source::spec::{required_string_field, string_array_field, ParsedSourceSpec};
use crate::types::{SourceKind, SourceSpec};
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

/// Parse a `"path"`-kind JSON source spec. Body of the `"path"` arm of
/// [`crate::source::parse_source_spec`].
///
/// # Errors
/// Returns `Error::InvalidRequest` if required fields are missing or malformed.
pub fn parse_path_spec(spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
    let root = required_string_field(spec, "root", "path source requires 'root'")?;
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

/// Reconstruct a `SourceSpec::Path` from its persisted `SourceRow` form. Body
/// of the `SourceKind::Path` arm of [`crate::source::source_row_to_source`].
pub fn path_row_to_spec(row: &SourceRow, _refresh_interval_secs: Option<u64>) -> SourceSpec {
    SourceSpec::Path {
        root: row.root.clone().unwrap_or_default(),
        include: row.include.clone(),
        exclude: row.exclude.clone(),
    }
}

/// [`SourceKindDef`] for `"path"` sources: one-line delegations to
/// [`parse_path_spec`] / [`path_row_to_spec`].
pub(in crate::source) struct PathKind;

impl SourceKindDef for PathKind {
    fn kind_str(&self) -> &'static str {
        "path"
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Path
    }

    fn parse_spec(&self, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
        parse_path_spec(spec)
    }

    fn row_to_spec(&self, row: &SourceRow, refresh_interval_secs: Option<u64>) -> SourceSpec {
        path_row_to_spec(row, refresh_interval_secs)
    }
}
