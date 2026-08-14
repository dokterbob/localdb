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
