//! `SearchFilters` — the single arg → `Vec<MetadataFilter>` conversion point
//! shared verbatim by the CLI, HTTP, and MCP surfaces (issue #247).
//!
//! specs/01-architecture.md §1 forbids domain logic in surface crates, so the
//! translation from raw string flags/fields to [`MetadataFilter`] lives here
//! in `core`, not duplicated three times. Every surface builds a
//! `SearchFilters` from its own argument shape (clap flags, an HTTP JSON
//! body, an MCP tool call) and then calls [`SearchFilters::into_metadata_filters`].
//!
//! # Value grammar (path/mime excepted — see below)
//!
//! Each of the eight date fields accepts one of three forms, tried in this
//! order (no disambiguation heuristic needed: `humantime::parse_duration`
//! requires a numeral immediately followed by a unit token, so a bare
//! partial date like `"2026"` always fails duration parsing outright):
//!
//! 1. A full RFC 3339 datetime, normalized to canonical UTC via
//!    [`crate::dates::parse_date_or_datetime`].
//! 2. A partial date (`"2026"`, `"2026-06"`, `"2026-06-10"`), passed through
//!    unchanged by the same function — load-bearing for
//!    [`crate::store::DateAxis::Document`]'s asymmetric bound comparison,
//!    which depends on partial widths surviving to `MetadataFilter`.
//! 3. A relative duration (`"7d"`, `"30m"`, `"2w"`) via
//!    `humantime::parse_duration`, resolved to `now − duration` — **always**,
//!    regardless of whether it fills an `_after` or `_before` bound. So
//!    `--modified-after 7d` means "modified within the last 7 days" and
//!    `--modified-before 7d` means "modified more than 7 days ago"; never
//!    `now + duration` in either direction.
//!
//! `path` and `mime` take neither form: they become
//! [`MetadataFilter::UriPrefix`]/[`MetadataFilter::Mime`] string filters
//! directly, with zero date/duration parsing. `--mime 7d` filters on the
//! literal string `"7d"` — it is not an error.

use serde::{Deserialize, Serialize};

use crate::dates::{is_canonical_timestamp, parse_date_or_datetime};
use crate::error::Error;
use crate::store::{DateAxis, MetadataFilter};

#[cfg(test)]
mod tests;

/// User/agent-supplied search-scoping filters (issue #247), shared verbatim
/// by the CLI (`localdb search`), HTTP (`POST /v1/search`), and MCP
/// (`search` tool) surfaces.
///
/// Every field is `Option<String>`, deliberately never `Vec`: a repeated
/// `--mime` flag is then a clap parse error at the CLI layer, rather than
/// silently compiling to "matches nothing" over HTTP/MCP, where a JSON array
/// would otherwise be accepted with no equivalent guard.
///
/// Field names match [`DateAxis::name`] exactly (`added`, `updated`,
/// `modified`, `document`), suffixed `_after`/`_before` — the same names the
/// CLI flags (`--added-after`, …) and the HTTP/MCP JSON fields (`added_after`,
/// …) use, so there is exactly one vocabulary across all three surfaces.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchFilters {
    /// URI-prefix filter. Matched literally, with no date/duration parsing —
    /// see this module's doc comment.
    #[serde(default)]
    #[schemars(
        description = "Restrict to resources whose URI starts with this prefix (e.g. \
        \"file:///docs/\"). Matched literally — no date or duration parsing."
    )]
    pub path: Option<String>,

    /// Exact MIME-type filter. Matched literally, with no date/duration
    /// parsing — see this module's doc comment.
    #[serde(default)]
    #[schemars(description = "Restrict to resources with this exact MIME type (e.g. \
        \"text/markdown\"). Matched literally — no date or duration parsing.")]
    pub mime: Option<String>,

    /// Inclusive lower bound on [`DateAxis::Added`].
    #[serde(default)]
    #[schemars(
        description = "Lower bound (inclusive) on the added date — when this resource \
        was first indexed. Accepts a full RFC 3339 datetime, a partial date (YYYY, YYYY-MM, \
        YYYY-MM-DD), or a relative duration such as \"7d\" or \"30m\", which always resolves \
        to now minus the duration regardless of which bound it fills. Note: \"M\" means \
        months and \"m\" means minutes in the duration grammar — both parse successfully, so \
        a mistaken capital silently produces a bound about 60 times further out. NULL rule: a \
        resource with no value on this axis is excluded, regardless of the bound."
    )]
    pub added_after: Option<String>,
    /// Inclusive upper bound on [`DateAxis::Added`].
    #[serde(default)]
    #[schemars(
        description = "Upper bound (inclusive) on the added date — when this resource \
        was first indexed. Same value grammar as added_after. NULL rule: a resource with no \
        value on this axis is excluded, regardless of the bound."
    )]
    pub added_before: Option<String>,

    /// Inclusive lower bound on [`DateAxis::Updated`].
    #[serde(default)]
    #[schemars(
        description = "Lower bound (inclusive) on the updated date — when the store \
        last wrote this resource's stored state. Same value grammar as added_after. NULL \
        rule: a resource with no value on this axis is excluded, regardless of the bound."
    )]
    pub updated_after: Option<String>,
    /// Inclusive upper bound on [`DateAxis::Updated`].
    #[serde(default)]
    #[schemars(
        description = "Upper bound (inclusive) on the updated date — when the store \
        last wrote this resource's stored state. Same value grammar as added_after. NULL \
        rule: a resource with no value on this axis is excluded, regardless of the bound."
    )]
    pub updated_before: Option<String>,

    /// Inclusive lower bound on [`DateAxis::Modified`].
    #[serde(default)]
    #[schemars(
        description = "Lower bound (inclusive) on the modified date — the source's \
        own claim of when this resource was last changed. Same value grammar as added_after. \
        NULL rule: a resource with no claimed modification time is excluded, regardless of \
        the bound."
    )]
    pub modified_after: Option<String>,
    /// Inclusive upper bound on [`DateAxis::Modified`].
    #[serde(default)]
    #[schemars(
        description = "Upper bound (inclusive) on the modified date — the source's \
        own claim of when this resource was last changed. Same value grammar as added_after. \
        NULL rule: a resource with no claimed modification time is excluded, regardless of \
        the bound."
    )]
    pub modified_before: Option<String>,

    /// Inclusive lower bound on [`DateAxis::Document`].
    #[serde(default)]
    #[schemars(
        description = "Lower bound (inclusive) on the document date — the document's \
        own claimed date (Dublin Core dc:date). Same value grammar as added_after. NULL \
        rule: a resource with no claimed document date is excluded, regardless of the bound. \
        Coverage: this date is populated today only for HTML, Markdown front matter, and \
        Office documents — PDFs and feed entries have none yet (issue #251), so this \
        currently excludes every PDF in a corpus."
    )]
    pub document_after: Option<String>,
    /// Inclusive upper bound on [`DateAxis::Document`].
    #[serde(default)]
    #[schemars(
        description = "Upper bound (inclusive) on the document date — the document's \
        own claimed date (Dublin Core dc:date). Same value grammar as added_after. NULL \
        rule: a resource with no claimed document date is excluded, regardless of the bound. \
        Coverage: this date is populated today only for HTML, Markdown front matter, and \
        Office documents — PDFs and feed entries have none yet (issue #251), so this \
        currently excludes every PDF in a corpus."
    )]
    pub document_before: Option<String>,
}

impl SearchFilters {
    /// Convert to the `Vec<MetadataFilter>` the search orchestrator pushes
    /// down to each backend. `Ok(vec![])` for an all-`None` `SearchFilters`
    /// (the default, unfiltered case).
    ///
    /// # Errors
    /// `Error::InvalidRequest` naming the offending field, for any date field
    /// whose value matches none of the three accepted forms (see this
    /// module's doc comment).
    pub fn into_metadata_filters(self) -> Result<Vec<MetadataFilter>, Error> {
        let mut filters = Vec::new();

        if let Some(path) = self.path {
            filters.push(MetadataFilter::UriPrefix(path));
        }
        if let Some(mime) = self.mime {
            filters.push(MetadataFilter::Mime(mime));
        }

        // (axis, after-field-name, after-value, before-field-name, before-value)
        let axes = [
            (
                DateAxis::Added,
                "added_after",
                self.added_after,
                "added_before",
                self.added_before,
            ),
            (
                DateAxis::Updated,
                "updated_after",
                self.updated_after,
                "updated_before",
                self.updated_before,
            ),
            (
                DateAxis::Modified,
                "modified_after",
                self.modified_after,
                "modified_before",
                self.modified_before,
            ),
            (
                DateAxis::Document,
                "document_after",
                self.document_after,
                "document_before",
                self.document_before,
            ),
        ];

        for (axis, after_field, after, before_field, before) in axes {
            if let Some(raw) = after {
                let value = parse_filter_date_value(after_field, &raw)?;
                filters.push(MetadataFilter::DateAfter { axis, value });
            }
            if let Some(raw) = before {
                // Every axis's bound is carried through exactly as parsed —
                // no upper-bound widening happens here. Widening the
                // `document` axis is the store layer's job, because it has
                // to apply to every `MetadataFilter`, however constructed,
                // not only to those built from a `SearchFilters`. Both
                // backends already do it: `MetadataFilter::matches` widens
                // both operands, and `store-libsql`'s `build_filter_clauses`
                // widens the bound and mirrors it with a `CASE` over the
                // column. Widening here as well would give one rule two
                // owners that must stay in lockstep for no gain.
                let value = parse_filter_date_value(before_field, &raw)?;
                filters.push(MetadataFilter::DateBefore { axis, value });
            }
        }

        Ok(filters)
    }
}

/// Parse one date-filter value into a canonical bound string, trying the
/// date grammar before the duration grammar (see this module's doc comment
/// for why no disambiguation heuristic is needed).
fn parse_filter_date_value(field: &str, raw: &str) -> Result<String, Error> {
    if let Some(value) = parse_date_or_datetime(raw) {
        return Ok(value);
    }

    if let Ok(duration) = humantime::parse_duration(raw) {
        let duration = chrono::Duration::from_std(duration).map_err(|_| Error::InvalidRequest {
            message: format!("{field}: duration out of range: {raw:?}"),
        })?;
        // Sign convention (issue #247, deliberately specified): a duration
        // ALWAYS resolves to `now - duration`, used identically for either
        // bound direction. `--modified-after 7d` -> "within the last 7
        // days"; `--modified-before 7d` -> "more than 7 days ago". Never
        // `now + duration`.
        //
        // `checked_sub_signed`, not `-`: a duration can be syntactically
        // valid and fit both `std::time::Duration` and `chrono::Duration`
        // while still carrying `now` outside `DateTime<Utc>`'s representable
        // range (a span on the order of a million years does it). Plain
        // subtraction panics there, and these values arrive unfiltered from
        // the HTTP and MCP surfaces, so it has to be an `invalid_request`
        // like any other unusable bound.
        let bound = chrono::Utc::now()
            .checked_sub_signed(duration)
            .ok_or_else(|| Error::InvalidRequest {
                message: format!("{field}: duration out of range: {raw:?}"),
            })?;
        let bound = bound.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // The result must land in the canonical `YYYY-MM-DDTHH:MM:SSZ` form
        // (specs/02-domain-model.md), because bounds are compared
        // lexicographically against stored values in exactly that form. A
        // duration large enough to reach a year outside `0000..=9999` still
        // subtracts successfully, but chrono then renders a signed,
        // wider-than-four-digit year — and `-` sorts below every digit, so
        // such a bound would compare as an extreme against every row rather
        // than as the date it names. Reject it as unusable instead.
        if !is_canonical_timestamp(&bound) {
            return Err(Error::InvalidRequest {
                message: format!("{field}: duration out of range: {raw:?}"),
            });
        }
        return Ok(bound);
    }

    Err(Error::InvalidRequest {
        message: format!(
            "{field}: not a valid date (YYYY, YYYY-MM, YYYY-MM-DD), datetime (RFC 3339), or \
             relative duration (e.g. \"7d\"): {raw:?}"
        ),
    })
}
