//! Unit tests for [`super::SearchFilters::into_metadata_filters`]: the three
//! accepted value forms, the duration sign convention, the document-axis-only
//! widening rule, and the error cases.

use super::SearchFilters;
use crate::error::Error;
use crate::store::{ChunkRecord, DateAxis, MetadataFilter};
use crate::types::Span;

fn filters() -> SearchFilters {
    SearchFilters::default()
}

// ---------------------------------------------------------------------------
// Default / path / mime — no date parsing at all
// ---------------------------------------------------------------------------

#[test]
fn default_filters_yield_no_metadata_filters() {
    let result = filters().into_metadata_filters().unwrap();
    assert!(result.is_empty());
}

#[test]
fn path_becomes_uri_prefix_filter() {
    let f = SearchFilters {
        path: Some("file:///docs/".to_string()),
        ..filters()
    };
    let result = f.into_metadata_filters().unwrap();
    assert_eq!(
        result,
        vec![MetadataFilter::UriPrefix("file:///docs/".to_string())]
    );
}

/// `--mime 7d` must filter on the literal string `"7d"`, not be misparsed as
/// a duration — `path`/`mime` never run through date/duration parsing.
#[test]
fn mime_matches_literal_string_that_looks_like_a_duration() {
    let f = SearchFilters {
        mime: Some("7d".to_string()),
        ..filters()
    };
    let result = f.into_metadata_filters().unwrap();
    assert_eq!(result, vec![MetadataFilter::Mime("7d".to_string())]);
}

// ---------------------------------------------------------------------------
// Value grammar: full datetime / partial date / relative duration
// ---------------------------------------------------------------------------

#[test]
fn full_datetime_is_normalized_to_canonical_utc() {
    let f = SearchFilters {
        added_after: Some("2026-06-10T14:30:00+02:00".to_string()),
        ..filters()
    };
    let result = f.into_metadata_filters().unwrap();
    assert_eq!(
        result,
        vec![MetadataFilter::DateAfter {
            axis: DateAxis::Added,
            value: "2026-06-10T12:30:00Z".to_string(),
        }]
    );
}

#[test]
fn partial_dates_pass_through_unchanged_for_after_bound() {
    for raw in ["2026", "2026-06", "2026-06-10"] {
        let f = SearchFilters {
            document_after: Some(raw.to_string()),
            ..filters()
        };
        let result = f.into_metadata_filters().unwrap();
        assert_eq!(
            result,
            vec![MetadataFilter::DateAfter {
                axis: DateAxis::Document,
                value: raw.to_string(),
            }],
            "partial date {raw:?} should pass through unchanged on the after bound"
        );
    }
}

/// A duration can parse cleanly and still carry `now` outside
/// `DateTime<Utc>`'s representable range. That must be an `invalid_request`,
/// not a panic — these values arrive unfiltered from the HTTP and MCP
/// surfaces, where a panic would take down the request rather than answer
/// it.
#[test]
fn duration_that_underflows_the_representable_range_is_invalid_request() {
    for huge in ["1000000years", "500000years", "9999999weeks"] {
        let f = SearchFilters {
            added_after: Some(huge.to_string()),
            ..filters()
        };
        match f.into_metadata_filters() {
            Err(Error::InvalidRequest { message }) => {
                assert!(
                    message.contains("added_after"),
                    "error must name the offending field, got: {message}"
                );
            }
            Err(other) => panic!("{huge:?} must be InvalidRequest, got {other:?}"),
            Ok(v) => panic!("{huge:?} must be rejected, got {v:?}"),
        }
    }
}

#[test]
fn relative_duration_resolves_to_now_minus_duration() {
    let before = chrono::Utc::now();
    let f = SearchFilters {
        modified_after: Some("7d".to_string()),
        ..filters()
    };
    let result = f.into_metadata_filters().unwrap();
    let value = match &result[0] {
        MetadataFilter::DateAfter { value, .. } => value.clone(),
        other => panic!("expected DateAfter, got {other:?}"),
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(&value)
        .expect("duration must resolve to a valid RFC 3339 datetime")
        .with_timezone(&chrono::Utc);
    let expected = before - chrono::Duration::days(7);
    let delta = (parsed - expected).num_seconds().abs();
    assert!(
        delta < 5,
        "expected ~now-7d ({expected}), got {parsed} (delta {delta}s)"
    );
}

// ---------------------------------------------------------------------------
// Duration sign convention — the load-bearing test the plan calls out by
// name: a same-day fixture would look correct under either interpretation,
// so this pins both directions with fixtures 1 day and 8 days old against a
// 7-day bound.
// ---------------------------------------------------------------------------

fn chunk_record_with_modified_at(modified_at: &str) -> ChunkRecord {
    ChunkRecord {
        id: "chunk-1".to_string(),
        resource_id: "doc-1".to_string(),
        store_id: "store-1".to_string(),
        text: "text".to_string(),
        span: Span::new(0, 4),
        heading_path: vec![],
        embedding: vec![],
        policy_version: "v1".to_string(),
        fetched_at: "2020-01-01T00:00:00Z".to_string(),
        modified_at: Some(modified_at.to_string()),
        content_hash: "abc".to_string(),
        origin_store: "store-1".to_string(),
        source_id: "source-1".to_string(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: "file:///doc.txt".to_string(),
        metadata: Default::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    }
}

#[test]
fn modified_before_7d_excludes_yesterday_and_includes_8_days_ago() {
    let now = chrono::Utc::now();
    let yesterday =
        (now - chrono::Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let eight_days_ago =
        (now - chrono::Duration::days(8)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let f = SearchFilters {
        modified_before: Some("7d".to_string()),
        ..filters()
    };
    let result = f.into_metadata_filters().unwrap();
    assert_eq!(result.len(), 1);
    let filter = &result[0];

    let modified_yesterday = chunk_record_with_modified_at(&yesterday);
    let modified_eight_days_ago = chunk_record_with_modified_at(&eight_days_ago);

    assert!(
        !filter.matches(&modified_yesterday),
        "--modified-before 7d must EXCLUDE a document modified yesterday"
    );
    assert!(
        filter.matches(&modified_eight_days_ago),
        "--modified-before 7d must INCLUDE a document modified 8 days ago"
    );
}

// ---------------------------------------------------------------------------
// Widening ownership: no axis's bound is widened here.
// ---------------------------------------------------------------------------

/// Upper-bound widening for the `document` axis belongs to the store layer,
/// which applies it to every `MetadataFilter` however constructed —
/// `MetadataFilter::matches` widens both operands, and `store-libsql`'s
/// `build_filter_clauses` widens the bound and mirrors it with a `CASE` over
/// the column. This conversion therefore carries every bound through exactly
/// as parsed, so the filter reflects what the caller actually asked for.
///
/// Pinned because widening here as well would be invisible — it is
/// idempotent, so the duplication would pass every behavioral test while
/// giving one rule two owners that must stay in lockstep.
#[test]
fn no_axis_before_bound_is_widened_at_the_conversion_boundary() {
    let f = SearchFilters {
        added_before: Some("2026".to_string()),
        updated_before: Some("2026".to_string()),
        modified_before: Some("2026".to_string()),
        document_before: Some("2026".to_string()),
        ..filters()
    };
    let result = f.into_metadata_filters().unwrap();

    let value_for = |axis: DateAxis| {
        result
            .iter()
            .find_map(|f| match f {
                MetadataFilter::DateBefore { axis: a, value } if *a == axis => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a DateBefore filter for {axis:?}"))
    };

    assert_eq!(
        value_for(DateAxis::Added),
        "2026",
        "added must not be widened"
    );
    assert_eq!(
        value_for(DateAxis::Updated),
        "2026",
        "updated must not be widened"
    );
    assert_eq!(
        value_for(DateAxis::Modified),
        "2026",
        "modified must not be widened"
    );
    assert_eq!(
        value_for(DateAxis::Document),
        "2026",
        "document must not be widened here either — the store layer owns widening"
    );
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn invalid_date_value_is_invalid_request_naming_the_field() {
    let f = SearchFilters {
        added_after: Some("not-a-date".to_string()),
        ..filters()
    };
    let err = f.into_metadata_filters().unwrap_err();
    assert_eq!(err.code(), "invalid_request");
    let message = err.to_string();
    assert!(
        message.contains("added_after"),
        "error message should name the offending field: {message}"
    );
}

#[test]
fn invalid_before_bound_value_names_the_before_field() {
    let f = SearchFilters {
        document_before: Some("not-a-date".to_string()),
        ..filters()
    };
    let err = f.into_metadata_filters().unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("document_before"),
        "error message should name the offending field: {message}"
    );
}
