use localdb_core::dates::widen_date_upper_bound;
use localdb_core::{DateAxis, MetadataFilter};

pub(crate) fn escape_fts5_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            let escaped = token.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a SQL `WHERE` clause fragment for metadata filters, with values
/// bound as `?` placeholders rather than interpolated into the SQL text
/// (issue #255).
///
/// Returns `(clause, values)`: `clause` is a string of ` AND ...` fragments
/// meant to be spliced verbatim after the tenant predicate in each query,
/// and `values` are the corresponding bind values in the same left-to-right
/// order their `?` placeholders appear in `clause`. Callers are responsible
/// for prepending any params that precede the filter clauses in the query
/// (e.g. the tenant `store_id`, or an FTS `MATCH` term) to `values` in the
/// matching order.
///
/// The JOIN in every chunk query aliases `resources` as `r`, so filter
/// columns use the `r.` prefix. Chunk-level columns use `c.`.
pub(crate) fn build_filter_clauses(filters: &[MetadataFilter]) -> (String, Vec<String>) {
    let mut clauses = String::new();
    let mut values = Vec::new();
    for filter in filters {
        match filter {
            MetadataFilter::Mime(v) => push_filter(&mut clauses, &mut values, "r.mime =", v),
            MetadataFilter::UriPrefix(v) => {
                // A literal `%` or `_` in a URI-prefix filter is treated as a SQL
                // LIKE wildcard: binding the value doesn't escape wildcard
                // characters within the prefix itself, only the trailing `%`
                // this arm appends is intentional. Deliberately not fixed — see
                // specs/04-search-pipeline.md §5.
                clauses.push_str(" AND r.uri LIKE ?");
                values.push(format!("{v}%"));
            }
            MetadataFilter::DateAfter { axis, value } => {
                // `DateAfter` needs no widening: in fixed-width ISO 8601 a
                // proper prefix always sorts less than the string it
                // prefixes, so plain `>=` is already correct for every
                // combination of short/long stored value against
                // short/long bound. See `core::dates::widen_date_upper_bound`'s
                // doc comment for the full argument.
                let column_op = format!("r.{} >=", axis.column());
                push_filter(&mut clauses, &mut values, &column_op, value)
            }
            MetadataFilter::DateBefore { axis, value } => {
                // The two operands widen under DIFFERENT rules — mirroring
                // `MetadataFilter::matches` in `core`, which MUST stay in
                // lockstep with this arm (same lengths 4 / 7 / 10, same
                // widened suffixes).
                //
                // The BOUND is caller-supplied, so it can be partial on ANY
                // axis and is always widened here in Rust before binding.
                // Without it, an inclusive `added_before: "2026"` excludes
                // every resource added during 2026, since a longer timestamp
                // sorts after its own prefix.
                let bound = widen_date_upper_bound(value);

                // The COLUMN is only partial-width on `Document`
                // (`date_parsed`, normalized by
                // `core::dates::parse_partial_iso8601` to exactly 4, 7, or 10
                // chars); `Added`/`Updated`/`Modified` always hold full RFC
                // 3339. So only `Document` pays for the `CASE`, which no
                // index can cover. `length(NULL)` is `NULL` in SQLite, so no
                // `WHEN` arm matches for a NULL column and the `CASE` falls
                // through to `ELSE col` (still `NULL`), preserving the
                // "NULL fails every bound" property.
                let column = axis.column();
                let column_op = if matches!(axis, DateAxis::Document) {
                    format!(
                        "CASE length(r.{column})
                             WHEN 4 THEN r.{column} || '-12-31T23:59:59Z'
                             WHEN 7 THEN r.{column} || '-31T23:59:59Z'
                             WHEN 10 THEN r.{column} || 'T23:59:59Z'
                             ELSE r.{column}
                         END <="
                    )
                } else {
                    format!("r.{column} <=")
                };
                push_filter(&mut clauses, &mut values, &column_op, &bound)
            }
            MetadataFilter::SourceId(v) => {
                push_filter(&mut clauses, &mut values, "r.source_id =", v)
            }
            MetadataFilter::ResourceId(v) => {
                push_filter(&mut clauses, &mut values, "c.resource_id =", v)
            }
            MetadataFilter::PolicyVersion(v) => {
                push_filter(&mut clauses, &mut values, "r.policy_version =", v)
            }
        }
    }
    (clauses, values)
}

fn push_filter(clauses: &mut String, values: &mut Vec<String>, column_op: &str, value: &str) {
    clauses.push_str(&format!(" AND {column_op} ?"));
    values.push(value.to_string());
}
