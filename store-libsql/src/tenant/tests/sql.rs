//! `tenant::sql::build_filter_clauses` unit tests.
//!
//! Pure-function coverage for the `?`-placeholder clause builder (issue
//! #255): no DB needed. Integration coverage that filter values actually
//! survive as literal data through the real libsql backend lives in
//! `store-libsql/tests/conformance.rs` via
//! `core::store::conformance::test_metadata_filter_values_are_bound_not_interpolated`.

use localdb_core::{DateAxis, MetadataFilter};

use crate::tenant::sql::build_filter_clauses;

#[test]
fn empty_filters_produce_empty_clause_and_no_values() {
    let (clause, values) = build_filter_clauses(&[]);
    assert_eq!(clause, "");
    assert!(values.is_empty());
}

#[test]
fn each_filter_variant_produces_exactly_one_placeholder_and_value() {
    let cases: Vec<(MetadataFilter, &str)> = vec![
        (
            MetadataFilter::Mime("text/markdown".to_string()),
            "text/markdown",
        ),
        (
            MetadataFilter::DateAfter {
                axis: DateAxis::Added,
                value: "2026-01-01T00:00:00Z".to_string(),
            },
            "2026-01-01T00:00:00Z",
        ),
        (
            MetadataFilter::DateBefore {
                axis: DateAxis::Added,
                value: "2026-01-01T00:00:00Z".to_string(),
            },
            "2026-01-01T00:00:00Z",
        ),
        (MetadataFilter::SourceId("src-1".to_string()), "src-1"),
        (MetadataFilter::ResourceId("doc-1".to_string()), "doc-1"),
        (MetadataFilter::PolicyVersion("v1".to_string()), "v1"),
    ];
    for (filter, expected_value) in &cases {
        let (clause, values) = build_filter_clauses(std::slice::from_ref(filter));
        assert_eq!(
            clause.matches('?').count(),
            1,
            "{filter:?} should produce exactly one placeholder, got clause {clause:?}"
        );
        assert_eq!(
            values,
            vec![expected_value.to_string()],
            "{filter:?} should bind its own value unchanged"
        );
    }
}

#[test]
fn uri_prefix_appends_wildcard_suffix_to_the_bound_value_not_the_sql() {
    let filter = MetadataFilter::UriPrefix("file:///docs/".to_string());
    let (clause, values) = build_filter_clauses(std::slice::from_ref(&filter));

    assert!(
        clause.contains("LIKE ?") && !clause.contains('%'),
        "the trailing wildcard must be in the bound value, not the SQL text: {clause:?}"
    );
    assert!(
        !clause.to_uppercase().contains("ESCAPE"),
        "no ESCAPE clause was decided against — %/_ stay LIKE wildcards: {clause:?}"
    );
    assert_eq!(values, vec!["file:///docs/%".to_string()]);
}

#[test]
fn multiple_filters_produce_placeholders_and_values_in_matching_order() {
    let filters = vec![
        MetadataFilter::Mime("text/markdown".to_string()),
        MetadataFilter::UriPrefix("file:///docs/".to_string()),
        MetadataFilter::SourceId("src-1".to_string()),
    ];
    let (clause, values) = build_filter_clauses(&filters);

    assert_eq!(clause.matches('?').count(), 3);
    assert_eq!(
        values,
        vec![
            "text/markdown".to_string(),
            "file:///docs/%".to_string(),
            "src-1".to_string(),
        ],
        "values must appear in the same left-to-right order as their ? placeholders"
    );
}

/// `DateBefore { axis: Document, .. }` is the one arm that emits a `CASE`
/// over the column (to widen a partial-precision `date_parsed` value) and
/// binds the *widened* bound, not the raw one — every other axis/direction
/// combination binds its value unchanged (covered above).
#[test]
fn date_before_document_axis_widens_the_bound_and_wraps_the_column_in_case() {
    let filter = MetadataFilter::DateBefore {
        axis: DateAxis::Document,
        value: "2024-06".to_string(),
    };
    let (clause, values) = build_filter_clauses(std::slice::from_ref(&filter));

    assert!(
        clause.contains("CASE length(r.date_parsed)"),
        "Document-axis DateBefore must wrap the column in a length-keyed CASE: {clause:?}"
    );
    assert!(
        clause.trim_end().ends_with("END <= ?"),
        "the CASE must be compared with <=, matching Before's upper-bound semantics: {clause:?}"
    );
    assert_eq!(
        values,
        vec!["2024-06-31T23:59:59Z".to_string()],
        "the bound itself must be widened before binding, not left as the raw input"
    );
}

/// `DateAfter { axis: Document, .. }` needs no widening (see
/// `core::dates::widen_date_upper_bound`'s doc comment) — the bound is bound
/// unchanged and the column is referenced plainly, not wrapped in `CASE`.
#[test]
fn date_after_document_axis_binds_the_bound_unwidened_with_no_case() {
    let filter = MetadataFilter::DateAfter {
        axis: DateAxis::Document,
        value: "2024-06".to_string(),
    };
    let (clause, values) = build_filter_clauses(std::slice::from_ref(&filter));

    assert!(
        !clause.contains("CASE"),
        "Document-axis DateAfter must not need a CASE: {clause:?}"
    );
    assert!(clause.contains("r.date_parsed >="));
    assert_eq!(values, vec!["2024-06".to_string()]);
}

/// Every axis maps to its own `resources` column, and non-`Document` axes
/// never trigger the `CASE` widening even on `DateBefore`.
#[test]
fn every_axis_maps_to_its_own_column() {
    let cases = [
        (DateAxis::Added, "r.added_at"),
        (DateAxis::Updated, "r.index_updated_at"),
        (DateAxis::Modified, "r.modified_at"),
    ];
    for (axis, expected_column) in cases {
        let filter = MetadataFilter::DateBefore {
            axis,
            value: "2026-01-01T00:00:00Z".to_string(),
        };
        let (clause, values) = build_filter_clauses(std::slice::from_ref(&filter));
        assert!(
            clause.contains(&format!("{expected_column} <=")),
            "{axis:?} should filter on {expected_column}: {clause:?}"
        );
        assert!(
            !clause.contains("CASE"),
            "{axis:?} should not need CASE: {clause:?}"
        );
        assert_eq!(values, vec!["2026-01-01T00:00:00Z".to_string()]);
    }
}
