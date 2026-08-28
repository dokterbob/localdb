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
    /// URI-prefix filter. No date/duration parsing — see this module's doc
    /// comment. Pushed down as a SQL `LIKE` prefix match, so a literal `%` or
    /// `_` in the value is a wildcard (specs/04-search-pipeline.md §5).
    #[serde(default)]
    #[schemars(
        description = "Restrict to resources whose URI starts with this prefix (e.g. \
        \"file:///docs/\"). Not a date: no date or duration parsing is applied. \
        Matched with SQL LIKE, so a literal `%` or `_` in the prefix acts as a \
        wildcard."
    )]
    pub path: Option<String>,

    /// Exact MIME-type filter, compared as a whole string. No date/duration
    /// parsing — see this module's doc comment.
    #[serde(default)]
    #[schemars(description = "Restrict to resources with this exact MIME type (e.g. \
        \"text/markdown\"). Matched as an exact string: no date or duration parsing \
        is applied.")]
    pub mime: Option<String>,

    /// Inclusive lower bound on [`DateAxis::Added`].
    #[serde(default)]
    #[schemars(
        description = "Lower bound (inclusive) on the added date — when this resource \
        was first indexed. Accepts a full RFC 3339 datetime, a partial date (YYYY, YYYY-MM, \
        YYYY-MM-DD), or a relative duration such as \"7d\" or \"30m\", which always resolves \
        to now minus the duration regardless of which bound it fills. Note: \"M\" means \
        months and \"m\" means minutes in the duration grammar — both parse successfully, so \
        a mistaken capital silently produces a bound roughly 44,000 times further out. NULL rule: a \
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
        Coverage: a resource has one only when its source carried one — HTML \
        (JSON-LD or a `dcterms.date`/`date` meta), Markdown front matter, Office \
        (`dcterms:created`), PDF (`/CreationDate` or XMP `xmp:CreateDate`), and feed \
        entries (`published`/`updated`). Plain text carries none, and any format's \
        metadata may simply omit it."
    )]
    pub document_after: Option<String>,
    /// Inclusive upper bound on [`DateAxis::Document`].
    #[serde(default)]
    #[schemars(
        description = "Upper bound (inclusive) on the document date — the document's \
        own claimed date (Dublin Core dc:date). Same value grammar as added_after. NULL \
        rule: a resource with no claimed document date is excluded, regardless of the bound. \
        Coverage: a resource has one only when its source carried one — HTML \
        (JSON-LD or a `dcterms.date`/`date` meta), Markdown front matter, Office \
        (`dcterms:created`), PDF (`/CreationDate` or XMP `xmp:CreateDate`), and feed \
        entries (`published`/`updated`). Plain text carries none, and any format's \
        metadata may simply omit it."
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
    /// Is any filter field set?
    ///
    /// Lets a caller skip work that only matters when filtering — notably the
    /// CLI's daemon-capability probe, which should not cost an extra request
    /// on every unfiltered search.
    pub fn is_any_set(&self) -> bool {
        let Self {
            path,
            mime,
            added_after,
            added_before,
            updated_after,
            updated_before,
            modified_after,
            modified_before,
            document_after,
            document_before,
        } = self;
        // Destructured rather than field-by-field so adding a field without
        // updating this is a compile error, not a silently-missed case.
        [
            path,
            mime,
            added_after,
            added_before,
            updated_after,
            updated_before,
            modified_after,
            modified_before,
            document_after,
            document_before,
        ]
        .iter()
        .any(|f| f.is_some())
    }

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
                let value = parse_filter_date_value(after_field, &raw, Bound::Lower)?;
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
                let value = parse_filter_date_value(before_field, &raw, Bound::Upper)?;
                filters.push(MetadataFilter::DateBefore { axis, value });
            }
        }

        Ok(filters)
    }
}

/// Parse one date-filter value into a canonical bound string, trying the
/// date grammar before the duration grammar (see this module's doc comment
/// for why no disambiguation heuristic is needed).
/// If `raw` is a full RFC 3339 datetime carrying a non-zero sub-second
/// component, return it rounded **up** to the next whole second in canonical
/// form. Returns `None` for anything else — a partial date, a datetime with
/// no fraction (or a zero one), or a value chrono cannot parse — leaving the
/// caller's existing result untouched.
fn ceil_to_next_second(raw: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw.trim()).ok()?;
    let utc = parsed.with_timezone(&chrono::Utc);
    if utc.timestamp_subsec_nanos() == 0 {
        return None;
    }
    let ceiled = utc.checked_add_signed(chrono::Duration::seconds(1))?;
    let formatted = ceiled.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    is_canonical_timestamp(&formatted).then_some(formatted)
}

/// Which end of a range a value bounds. Only matters for one thing — how a
/// sub-second component is rounded — but it matters in opposite directions,
/// so the two cannot share a single rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    /// Inclusive lower bound (`DateAfter`).
    Lower,
    /// Inclusive upper bound (`DateBefore`).
    Upper,
}

fn parse_filter_date_value(field: &str, raw: &str, bound: Bound) -> Result<String, Error> {
    if let Some(value) = parse_date_or_datetime(raw) {
        // Stored timestamps have second precision, and canonical form has no
        // fractional part, so a sub-second input has to be rounded to a whole
        // second. Truncating — which is what rendering at second precision
        // does — is right for an upper bound and wrong for a lower one:
        //
        //   added_after: "…14:30:00.9Z"  truncated to "…14:30:00Z" would admit
        //   a resource stored at exactly "…14:30:00Z", which precedes the
        //   bound the caller actually asked for.
        //
        // So a lower bound rounds up to the next whole second instead. Any
        // stored second-precision value at or after that is genuinely at or
        // after the requested instant. An upper bound keeps the truncation,
        // which is already correct for the same reason in reverse.
        if bound == Bound::Lower {
            if let Some(ceiled) = ceil_to_next_second(raw) {
                return Ok(ceiled);
            }
        }
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
