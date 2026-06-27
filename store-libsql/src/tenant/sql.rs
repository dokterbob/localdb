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

pub(crate) fn build_filter_clauses(filters: &[MetadataFilter]) -> String {
    let mut clauses = String::new();
    for filter in filters {
        match filter {
            MetadataFilter::Mime(v) => push_filter(&mut clauses, "d.mime =", v),
            MetadataFilter::UriPrefix(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND d.uri LIKE '{escaped}%'"));
            }
            MetadataFilter::FetchedAfter(v) => push_filter(&mut clauses, "d.fetched_at >=", v),
            MetadataFilter::FetchedBefore(v) => push_filter(&mut clauses, "d.fetched_at <=", v),
            MetadataFilter::SourceId(v) => push_filter(&mut clauses, "d.source_id =", v),
            MetadataFilter::DocumentId(v) => push_filter(&mut clauses, "c.document_id =", v),
            MetadataFilter::PolicyVersion(v) => push_filter(&mut clauses, "d.policy_version =", v),
        }
    }
    clauses
}

fn push_filter(clauses: &mut String, column_op: &str, value: &str) {
    let escaped = value.replace('\'', "''");
    clauses.push_str(&format!(" AND {column_op} '{escaped}'"));
}
