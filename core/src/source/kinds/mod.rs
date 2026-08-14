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
    /// `#[cfg(test)]` code today (the registry-consistency and registry-snapshot
    /// tests in `source::tests::dispatch`), so the non-test build sees it as
    /// dead code without this allow.
    #[allow(dead_code)]
    fn kind(&self) -> SourceKind;
    /// Write path: request JSON -> ParsedSourceSpec (delegates to the per-kind parse fn).
    fn parse_spec(&self, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error>;
    /// Read path: SourceRow -> SourceSpec. The dispatcher computes refresh_interval_secs
    /// once (tolerant logic) and passes it in.
    fn row_to_spec(&self, row: &SourceRow, refresh_interval_secs: Option<u64>) -> SourceSpec;
}

/// Kind registry, in `parse_source_spec`'s historical dispatch order (path, url, feed).
pub(in crate::source) const KINDS: [&dyn SourceKindDef; 3] =
    [&path::PathKind, &url::UrlKind, &feed::FeedKind];

/// Generates a [`SourceKindDef`] impl that delegates each method to the kind's wire string,
/// `SourceKind` variant, and per-kind parse/reconstruct functions. The three kinds'
/// `SourceKindDef` impls are otherwise identical one-line delegations — this macro is the
/// single place that shape lives, so `kinds::{path,url,feed}` each contribute only their own
/// name/variant/function names.
macro_rules! impl_source_kind_def {
    ($ty:ty, $kind_str:literal, $kind:expr, $parse_fn:path, $row_fn:path) => {
        impl $crate::source::kinds::SourceKindDef for $ty {
            fn kind_str(&self) -> &'static str {
                $kind_str
            }

            fn kind(&self) -> $crate::types::SourceKind {
                $kind
            }

            fn parse_spec(
                &self,
                spec: &serde_json::Value,
            ) -> Result<$crate::source::spec::ParsedSourceSpec, $crate::error::Error> {
                $parse_fn(spec)
            }

            fn row_to_spec(
                &self,
                row: &$crate::backend::SourceRow,
                refresh_interval_secs: Option<u64>,
            ) -> $crate::types::SourceSpec {
                $row_fn(row, refresh_interval_secs)
            }
        }
    };
}

pub(in crate::source) use impl_source_kind_def;

/// Read-path dispatch keeps COMPILE-TIME exhaustiveness: source_row_to_source reads persisted
/// rows, so a new SourceKind variant must be a compile error here, not a runtime fallback.
pub(in crate::source) fn kind_def(kind: &SourceKind) -> &'static dyn SourceKindDef {
    match kind {
        SourceKind::Path => &path::PathKind,
        SourceKind::Url => &url::UrlKind,
        SourceKind::Feed => &feed::FeedKind,
    }
}
