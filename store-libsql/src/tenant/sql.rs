use localdb_core::MetadataFilter;

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
            MetadataFilter::FetchedAfter(v) => {
                push_filter(&mut clauses, &mut values, "r.added_at >=", v)
            }
            MetadataFilter::FetchedBefore(v) => {
                push_filter(&mut clauses, &mut values, "r.added_at <=", v)
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
