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
mod tests;
