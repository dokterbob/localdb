//! `"url"`-kind sources: spec parsing and the `SourceKindDef` impl.

use super::SourceKindDef;
use crate::backend::SourceRow;
use crate::error::Error;
use crate::source::spec::{required_string_field, ParsedSourceSpec};
use crate::types::{SourceKind, SourceSpec};

/// [`SourceKindDef`] for `"url"` sources.
pub(in crate::source) struct UrlKind;

impl SourceKindDef for UrlKind {
    fn kind_str(&self) -> &'static str {
        "url"
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Url
    }

    /// Body of the historical `"url"` arm of [`crate::source::parse_source_spec`].
    fn parse_spec(&self, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
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

    fn row_to_spec(&self, row: &SourceRow) -> SourceSpec {
        SourceSpec::Url {
            url: row.url.clone().unwrap_or_default(),
            refresh_interval_secs: super::refresh_interval_from(row),
        }
    }
}
