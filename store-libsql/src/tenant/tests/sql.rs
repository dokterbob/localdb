//! `tenant::sql::build_filter_clauses` unit tests.
//!
//! Pure-function coverage for the `?`-placeholder clause builder (issue
//! #255): no DB needed. Integration coverage that filter values actually
//! survive as literal data through the real libsql backend lives in
//! `store-libsql/tests/conformance.rs` via
//! `core::store::conformance::test_metadata_filter_values_are_bound_not_interpolated`.

use localdb_core::MetadataFilter;

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
            MetadataFilter::FetchedAfter("2026-01-01T00:00:00Z".to_string()),
            "2026-01-01T00:00:00Z",
        ),
        (
            MetadataFilter::FetchedBefore("2026-01-01T00:00:00Z".to_string()),
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
