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

/// Build SQL WHERE clause fragments for metadata filters.
///
/// The JOIN in every chunk query aliases `resources` as `r`, so filter
/// columns use the `r.` prefix. Chunk-level columns use `c.`.
pub(crate) fn build_filter_clauses(filters: &[MetadataFilter]) -> String {
    let mut clauses = String::new();
    for filter in filters {
        match filter {
            MetadataFilter::Mime(v) => push_filter(&mut clauses, "r.mime =", v),
            MetadataFilter::UriPrefix(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND r.uri LIKE '{escaped}%'"));
            }
            MetadataFilter::FetchedAfter(v) => push_filter(&mut clauses, "r.added_at >=", v),
            MetadataFilter::FetchedBefore(v) => push_filter(&mut clauses, "r.added_at <=", v),
            MetadataFilter::SourceId(v) => push_filter(&mut clauses, "r.source_id =", v),
            // `document_id` in core maps to `resource_id` in the schema.
            MetadataFilter::DocumentId(v) => push_filter(&mut clauses, "c.resource_id =", v),
            MetadataFilter::PolicyVersion(v) => push_filter(&mut clauses, "r.policy_version =", v),
        }
    }
    clauses
}

fn push_filter(clauses: &mut String, column_op: &str, value: &str) {
    let escaped = value.replace('\'', "''");
    clauses.push_str(&format!(" AND {column_op} '{escaped}'"));
}
