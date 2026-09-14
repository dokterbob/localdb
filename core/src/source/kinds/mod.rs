//! The [`SourceKindDef`] trait and [`KINDS`] registry: per-kind (path/url/feed) parse and
//! row-reconstruction behavior, dispatched from `parse_source_spec` and `source_row_to_source`.
//! Unlike chunker's `FormatChunker`, which dispatches at runtime via a claim predicate over its
//! format registry, [`kind_def`] dispatches on the read path with a `match` over `SourceKind` —
//! deliberately keeping compile-time exhaustiveness so a new `SourceKind` variant is a compiler
//! error here, not a silent runtime fallback.

pub(in crate::source) mod feed;
pub(in crate::source) mod path;
pub(in crate::source) mod url;

#[cfg(test)]
pub(in crate::source) mod tests;

use crate::backend::SourceRow;
use crate::error::Error;
use crate::source::spec::ParsedSourceSpec;
use crate::types::{SourceKind, SourceSpec};

/// Per-source-kind behavior, dispatched from `parse_source_spec` (write path) and
/// `source_row_to_source` (read path) via the [`KINDS`] registry / [`kind_def`].
pub(in crate::source) trait SourceKindDef {
    /// Wire name for parse_source_spec dispatch ("path" / "url" / "feed").
    fn kind_str(&self) -> &'static str;
    /// The `SourceKind` this entry represents. Only reachable from
    /// `#[cfg(test)]` code today: the registry tests in
    /// `source::tests::dispatch` use it to prove every `SourceKind` variant
    /// has a [`KINDS`] entry, so the non-test build sees it as dead code
    /// without this allow.
    #[allow(dead_code)]
    fn kind(&self) -> SourceKind;
    /// Write path: request JSON -> ParsedSourceSpec.
    fn parse_spec(&self, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error>;
    /// Read path: SourceRow -> SourceSpec. Kinds with a refresh interval
    /// recompute it from `row.refresh` via [`refresh_interval_from`].
    fn row_to_spec(&self, row: &SourceRow) -> SourceSpec;
}

/// Kind registry, in `parse_source_spec`'s historical dispatch order (path, url, feed).
pub(in crate::source) const KINDS: [&dyn SourceKindDef; 3] =
    [&path::PathKind, &url::UrlKind, &feed::FeedKind];

/// Recompute a row's refresh interval in seconds, tolerantly.
///
/// C5: `refresh` is stored as the raw human-readable string the user gave
/// `localdb source add --refresh` (e.g. "24h"), validated at write time but
/// never converted to seconds for storage — the seconds value must be
/// recomputed on every read. Tolerant: a row that somehow holds an invalid
/// string (should never happen post-validation, but this is a read path and
/// must not panic/error on stale data) falls back to `None` rather than
/// failing the whole reconstruction.
pub(in crate::source) fn refresh_interval_from(row: &SourceRow) -> Option<u64> {
    row.refresh
        .as_deref()
        .and_then(|s| crate::config::validate_refresh_interval(s).ok())
        .flatten()
}

/// Read-path dispatch keeps COMPILE-TIME exhaustiveness: source_row_to_source reads persisted
/// rows, so a new SourceKind variant must be a compile error here, not a runtime fallback.
pub(in crate::source) fn kind_def(kind: &SourceKind) -> &'static dyn SourceKindDef {
    match kind {
        SourceKind::Path => &path::PathKind,
        SourceKind::Url => &url::UrlKind,
        SourceKind::Feed => &feed::FeedKind,
    }
}
