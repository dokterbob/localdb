//! `ParsedSourceSpec` and the small JSON-field helpers shared across source kinds.

use crate::error::Error;
use crate::types::SourceKind;

/// Result of [`crate::source::parse_source_spec`]: the kind-specific fields
/// needed to build a `SourceRow`, in one named struct (issue #116 —
/// previously an unlabeled 5-tuple, which grew a 6th field awkwardly as
/// `config_json` was added).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSourceSpec {
    pub kind: SourceKind,
    pub root: Option<String>,
    pub url: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Kind-specific JSON config blob for `SourceRow.config_json`. Populated
    /// for feed sources (see [`crate::source::build_feed_config_json`]);
    /// `None` for path and url sources.
    pub config_json: Option<String>,
}

/// Extract a required string field from a JSON source spec, or fail with
/// `missing_message` if it is absent or not a JSON string. Shared shape
/// behind the `"root"` (path), `"url"` (url), and `"url"` (feed) required
/// fields (issue #213).
pub(in crate::source) fn required_string_field(
    spec: &serde_json::Value,
    field: &str,
    missing_message: &str,
) -> Result<String, Error> {
    spec.get(field)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| Error::InvalidRequest {
            message: missing_message.to_string(),
        })
}

pub(in crate::source) fn string_array_field(
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
