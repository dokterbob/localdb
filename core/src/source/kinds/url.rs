//! `"url"`-kind sources: spec parsing and the `SourceKindDef` impl.

use crate::backend::SourceRow;
use crate::error::Error;
use crate::source::spec::{required_string_field, ParsedSourceSpec};
use crate::types::{SourceKind, SourceSpec};

/// Parse a `"url"`-kind JSON source spec. Body of the `"url"` arm of
/// [`crate::source::parse_source_spec`].
///
/// # Errors
/// Returns `Error::InvalidRequest` if required fields are missing or malformed.
pub fn parse_url_spec(spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
    let url = required_string_field(spec, "url", "url source requires 'url'")?;
    Ok(ParsedSourceSpec {
        kind: SourceKind::Url,
        root: None,
        url: Some(url),
        include: Vec::new(),
        exclude: Vec::new(),
        config_json: None,
    })
}

/// Reconstruct a `SourceSpec::Url` from its persisted `SourceRow` form. Body
/// of the `SourceKind::Url` arm of [`crate::source::source_row_to_source`].
pub fn url_row_to_spec(row: &SourceRow, refresh_interval_secs: Option<u64>) -> SourceSpec {
    SourceSpec::Url {
        url: row.url.clone().unwrap_or_default(),
        refresh_interval_secs,
    }
}

/// [`crate::source::kinds::SourceKindDef`] for `"url"` sources: one-line delegations to
/// [`parse_url_spec`] / [`url_row_to_spec`].
pub(in crate::source) struct UrlKind;

crate::source::kinds::impl_source_kind_def!(
    UrlKind,
    "url",
    SourceKind::Url,
    parse_url_spec,
    url_row_to_spec
);
