use crate::error::Error;
use crate::types::SourceKind;

mod kinds;
mod spec;

#[cfg(test)]
mod tests;

pub use kinds::feed::{build_feed_config_json, parse_feed_config_json, FeedConfig};
pub use kinds::path::{normalize_path_source, DEFAULT_PATH_EXCLUDES, DEFAULT_PATH_INCLUDES};
pub use spec::ParsedSourceSpec;

/// Parse a JSON source spec by kind.
///
/// # Errors
/// Returns `Error::InvalidRequest` if required fields are missing or malformed.
pub fn parse_source_spec(kind: &str, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
    match kind {
        "path" => kinds::path::parse_path_spec(spec),
        "url" => kinds::url::parse_url_spec(spec),
        "feed" => kinds::feed::parse_feed_spec(spec),
        other => Err(Error::InvalidRequest {
            message: format!("unknown source kind '{other}'"),
        }),
    }
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
    use crate::types::Source;

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
        SourceKind::Url => kinds::url::url_row_to_spec(row, refresh_interval_secs),
        SourceKind::Path => kinds::path::path_row_to_spec(row, refresh_interval_secs),
        SourceKind::Feed => kinds::feed::feed_row_to_spec(row, refresh_interval_secs),
    };

    Source {
        id: row.id.clone(),
        store_id: row.store_id.clone(),
        kind: row.kind.clone(),
        spec,
        source_preset: row.preset.clone(),
    }
}
