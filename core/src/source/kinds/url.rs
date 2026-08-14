use crate::backend::SourceRow;
use crate::error::Error;
use crate::source::spec::ParsedSourceSpec;
use crate::types::{SourceKind, SourceSpec};

/// Parse a `"url"`-kind JSON source spec. Body of the `"url"` arm of
/// [`crate::source::parse_source_spec`].
///
/// # Errors
/// Returns `Error::InvalidRequest` if required fields are missing or malformed.
pub fn parse_url_spec(spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
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

/// Reconstruct a `SourceSpec::Url` from its persisted `SourceRow` form. Body
/// of the `SourceKind::Url` arm of [`crate::source::source_row_to_source`].
pub fn url_row_to_spec(row: &SourceRow, refresh_interval_secs: Option<u64>) -> SourceSpec {
    SourceSpec::Url {
        url: row.url.clone().unwrap_or_default(),
        refresh_interval_secs,
    }
}
