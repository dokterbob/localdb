//! Source spec parsing and validation: `parse_source_spec` (write path, request JSON ->
//! `ParsedSourceSpec`) and `source_row_to_source` (read path, persisted `SourceRow` -> domain
//! `Source`), both dispatched per-kind through the [`kinds`] registry — the seam future
//! connectors (Notion, Telegram, …) plug into.

use crate::error::Error;

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
    match kinds::KINDS.iter().find(|def| def.kind_str() == kind) {
        Some(def) => def.parse_spec(spec),
        None => Err(Error::InvalidRequest {
            message: format!("unknown source kind '{kind}'"),
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

    let spec = kinds::kind_def(&row.kind).row_to_spec(row);

    Source {
        id: row.id.clone(),
        store_id: row.store_id.clone(),
        kind: row.kind.clone(),
        spec,
        source_preset: row.preset.clone(),
    }
}
