use crate::backend::SourceRow;
use crate::config::validate_max_entries;
use crate::error::Error;
use crate::source::kinds::SourceKindDef;
use crate::source::spec::{required_string_field, ParsedSourceSpec};
use crate::types::{SourceKind, SourceSpec};

// ---------------------------------------------------------------------------
// Feed config_json — tolerant parse + inverse builder
// ---------------------------------------------------------------------------

/// Feed-source config decoded from `SourceRow.config_json`.
///
/// `Default` matches [`parse_feed_config_json`]'s fallback: unbounded entries,
/// full-content fetch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedConfig {
    pub max_entries: Option<u32>,
    pub fetch_full_content: bool,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            max_entries: None,
            fetch_full_content: true,
        }
    }
}

/// Tolerantly parse a feed source's `config_json` column.
///
/// `NULL` (`None`), an empty/whitespace-only string, syntactically invalid
/// JSON, or validly-parsed JSON of the wrong shape (not a JSON object, or
/// missing/mistyped fields) all fall back to [`FeedConfig::default`] rather
/// than erroring — a corrupt or stale config_json must never fail a source
/// read. Shared by `cli::normalize` and `server::state` so this tolerance
/// lives in exactly one place (issue #116).
pub fn parse_feed_config_json(config_json: Option<&str>) -> FeedConfig {
    let Some(raw) = config_json else {
        return FeedConfig::default();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return FeedConfig::default();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return FeedConfig::default();
    };
    let Some(obj) = value.as_object() else {
        return FeedConfig::default();
    };
    let max_entries = obj
        .get("max_entries")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let fetch_full_content = obj
        .get("fetch_full_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    FeedConfig {
        max_entries,
        fetch_full_content,
    }
}

/// Build the `config_json` string for a feed source's `SourceRow`.
///
/// Inverse of [`parse_feed_config_json`]. `refresh_interval_secs` is
/// deliberately NOT a parameter — it is persisted in `SourceRow.refresh`
/// instead (see `SourceRow::config_json` doc comment).
pub fn build_feed_config_json(max_entries: Option<u32>, fetch_full_content: bool) -> String {
    serde_json::json!({
        "max_entries": max_entries,
        "fetch_full_content": fetch_full_content,
    })
    .to_string()
}

/// Parse a `"feed"`-kind JSON source spec. Body of the `"feed"` arm of
/// [`crate::source::parse_source_spec`].
///
/// # Errors
/// Returns `Error::InvalidRequest` if required fields are missing or malformed.
pub fn parse_feed_spec(spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
    let url = required_string_field(spec, "url", "feed source requires 'url'")?;
    // Full parse, not a prefix check: `https://[` and bare `https://`
    // start with the right prefix but fail `url::Url::parse`, and a
    // prefix-validated row would persist a source whose every index
    // run fails whole-source at the ingestor's fail-fast Uri::parse.
    let scheme_ok =
        crate::uri::Uri::parse(&url).is_some_and(|u| matches!(u.scheme(), "http" | "https"));
    if !scheme_ok {
        return Err(Error::InvalidRequest {
            message: format!("feed source 'url' must be a valid http(s) URL: '{url}'"),
        });
    }
    // Strict decode: a present, non-null `max_entries` must be an
    // integer that fits u32. `as_u64()` alone would silently treat
    // negatives/floats as absent and `as u32` would truncate huge
    // values (e.g. 4294967297 -> 1), mutating the caller's stated
    // intent instead of rejecting it — this arm is the single
    // validation authority for both CLI and HTTP surfaces.
    let max_entries = match spec.get("max_entries") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => match v.as_u64().filter(|&n| n <= u64::from(u32::MAX)) {
            Some(n) => Some(n as u32),
            None => {
                return Err(Error::InvalidRequest {
                    message: format!(
                        "feed source 'max_entries' must be a positive integer no \
                         greater than {}: {v}",
                        u32::MAX
                    ),
                })
            }
        },
    };
    let max_entries = validate_max_entries(max_entries)?;
    // Strict decode, mirroring `max_entries`: `as_bool()` alone would
    // treat a mistyped value (e.g. the string "false") as absent and
    // silently default discovery mode ON against the caller's stated
    // intent.
    let fetch_full_content = match spec.get("fetch_full_content") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(v) => {
            return Err(Error::InvalidRequest {
                message: format!("feed source 'fetch_full_content' must be a boolean: {v}"),
            })
        }
    };
    let config_json = build_feed_config_json(max_entries, fetch_full_content);
    Ok(ParsedSourceSpec {
        kind: SourceKind::Feed,
        root: None,
        url: Some(url),
        include: Vec::new(),
        exclude: Vec::new(),
        config_json: Some(config_json),
    })
}

/// Reconstruct a `SourceSpec::Feed` from its persisted `SourceRow` form. Body
/// of the `SourceKind::Feed` arm of [`crate::source::source_row_to_source`].
pub fn feed_row_to_spec(row: &SourceRow, refresh_interval_secs: Option<u64>) -> SourceSpec {
    let feed_config = parse_feed_config_json(row.config_json.as_deref());
    SourceSpec::Feed {
        url: row.url.clone().unwrap_or_default(),
        max_entries: feed_config.max_entries,
        fetch_full_content: feed_config.fetch_full_content,
        refresh_interval_secs,
    }
}

/// [`SourceKindDef`] for `"feed"` sources: one-line delegations to
/// [`parse_feed_spec`] / [`feed_row_to_spec`].
pub(in crate::source) struct FeedKind;

impl SourceKindDef for FeedKind {
    fn kind_str(&self) -> &'static str {
        "feed"
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Feed
    }

    fn parse_spec(&self, spec: &serde_json::Value) -> Result<ParsedSourceSpec, Error> {
        parse_feed_spec(spec)
    }

    fn row_to_spec(&self, row: &SourceRow, refresh_interval_secs: Option<u64>) -> SourceSpec {
        feed_row_to_spec(row, refresh_interval_secs)
    }
}
