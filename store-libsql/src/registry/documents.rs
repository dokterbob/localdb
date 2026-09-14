//! Cross-store resource lookup for the daemon's `GET /v1/documents/:id` path.
//!
//! The underlying table changed from `documents` to `resources` in schema v3.
//! The public API still speaks `DocumentInfo` (a core type) — the column
//! mapping is done here.
use localdb_core::{DocumentInfo, Error};

use crate::connection::{map_libsql_err, parse_metadata_json_lenient, LibsqlDb};

pub(crate) async fn find_document(
    db: &LibsqlDb,
    doc_id: &str,
    store_id: Option<&str>,
) -> Result<Option<DocumentInfo>, Error> {
    let conn = db.reader();
    // Column mapping from resources → DocumentInfo:
    //   resources.id           → DocumentInfo.id
    //   resources.added_at     → DocumentInfo.fetched_at
    //   resources.metadata_json → DocumentInfo.metadata
    if let Some(store_id) = store_id {
        // `UNIQUE(store_id, id)` guarantees at most one row here, so there is
        // no ambiguity path to handle.
        let mut rows = conn
            .query(
                "SELECT store_id, id, source_id, ingestor_kind, uri, title, mime,
                            content_hash, added_at, origin_store, policy_version, metadata_json,
                            date_original, date_parsed, index_updated_at
                     FROM resources WHERE id = ? AND store_id = ?",
                libsql::params![doc_id.to_string(), store_id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;
        return match rows.next().await.map_err(map_libsql_err)? {
            Some(row) => Ok(Some(row_to_document_info(&row)?)),
            None => Ok(None),
        };
    }

    let mut rows = conn
        .query(
            "SELECT store_id, id, source_id, ingestor_kind, uri, title, mime,
                        content_hash, added_at, origin_store, policy_version, metadata_json,
                        date_original, date_parsed, index_updated_at
                 FROM resources WHERE id = ?",
            libsql::params![doc_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut found = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        found.push(row_to_document_info(&row)?);
    }
    match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => Err(Error::InvalidRequest {
                message: format!(
                    "document '{doc_id}' exists in multiple stores; use store-scoped search to disambiguate"
                ),
            }),
        }
}

/// List documents in `store_id`, ordered by `uri`, optionally filtered to a
/// single `source_id`, and paginated by `limit`/`offset`.
///
/// `limit: None` binds SQLite's `LIMIT -1` — SQLite treats a negative `LIMIT`
/// as "no upper bound", so the query still applies `OFFSET` without capping
/// the row count, avoiding a second query shape for the unbounded case.
pub(crate) async fn list_documents(
    db: &LibsqlDb,
    store_id: &str,
    source_id: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<DocumentInfo>, Error> {
    let conn = db.reader();
    let limit_param: i64 = limit.map(|l| l as i64).unwrap_or(-1);
    let offset_param: i64 = offset as i64;
    let mut rows = match source_id {
        Some(source_id) => conn
            .query(
                "SELECT store_id, id, source_id, ingestor_kind, uri, title, mime,
                            content_hash, added_at, origin_store, policy_version, metadata_json,
                            date_original, date_parsed, index_updated_at
                     FROM resources WHERE store_id = ? AND source_id = ? ORDER BY uri
                     LIMIT ? OFFSET ?",
                libsql::params![
                    store_id.to_string(),
                    source_id.to_string(),
                    limit_param,
                    offset_param
                ],
            )
            .await
            .map_err(map_libsql_err)?,
        None => conn
            .query(
                "SELECT store_id, id, source_id, ingestor_kind, uri, title, mime,
                            content_hash, added_at, origin_store, policy_version, metadata_json,
                            date_original, date_parsed, index_updated_at
                     FROM resources WHERE store_id = ? ORDER BY uri
                     LIMIT ? OFFSET ?",
                libsql::params![store_id.to_string(), limit_param, offset_param],
            )
            .await
            .map_err(map_libsql_err)?,
    };
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_document_info(&row)?);
    }
    Ok(out)
}

/// Count documents in `store_id`, optionally filtered to a single
/// `source_id` — the un-paginated total behind a `list_documents` page.
pub(crate) async fn count_documents(
    db: &LibsqlDb,
    store_id: &str,
    source_id: Option<&str>,
) -> Result<u64, Error> {
    let conn = db.reader();
    let mut rows = match source_id {
        Some(source_id) => conn
            .query(
                "SELECT COUNT(*) FROM resources WHERE store_id = ? AND source_id = ?",
                libsql::params![store_id.to_string(), source_id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?,
        None => conn
            .query(
                "SELECT COUNT(*) FROM resources WHERE store_id = ?",
                libsql::params![store_id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?,
    };
    let row = rows
        .next()
        .await
        .map_err(map_libsql_err)?
        .ok_or_else(|| Error::Internal {
            message: "COUNT(*) query returned no rows".to_string(),
            correlation_id: "count_documents_no_rows".to_string(),
        })?;
    let count: i64 = row.get(0).map_err(map_libsql_err)?;
    // COUNT(*) is never negative; the max(0) is a defensive cast guard, not a
    // reachable branch.
    Ok(count.max(0) as u64)
}

fn row_to_document_info(row: &libsql::Row) -> Result<DocumentInfo, Error> {
    let store_id: String = row.get(0).map_err(map_libsql_err)?;
    let id: String = row.get(1).map_err(map_libsql_err)?;
    let source_id: String = row.get(2).map_err(map_libsql_err)?;
    let ingestor_kind: String = row.get(3).map_err(map_libsql_err)?;
    let uri: String = row.get(4).map_err(map_libsql_err)?;
    let title: Option<String> = row.get(5).map_err(map_libsql_err)?;
    let mime: Option<String> = row.get(6).map_err(map_libsql_err)?;
    let content_hash: String = row.get(7).map_err(map_libsql_err)?;
    let fetched_at: String = row.get(8).map_err(map_libsql_err)?; // added_at
    let origin_store: String = row.get(9).map_err(map_libsql_err)?;
    let policy_version: String = row.get(10).map_err(map_libsql_err)?;
    let metadata_str: String = row.get(11).map_err(map_libsql_err)?; // metadata_json
                                                                     // Read defensively: rows written before the tagged-`Metadata` migration
                                                                     // (#130) hold untagged, flat Dublin Core JSON — fall back to
                                                                     // `Metadata::default()` rather than erroring the whole lookup. A
                                                                     // genuine parse failure is logged (issue C4); see
                                                                     // `parse_metadata_json_lenient`.
    let metadata: localdb_core::metadata::Metadata =
        parse_metadata_json_lenient(&metadata_str, &id);
    let date_original: Option<String> = row.get(12).map_err(map_libsql_err)?;
    let date_parsed: Option<String> = row.get(13).map_err(map_libsql_err)?;
    let index_updated_at: Option<String> = row.get(14).map_err(map_libsql_err)?;

    Ok(DocumentInfo {
        store_id,
        id,
        source_id,
        ingestor_kind,
        uri,
        title,
        mime,
        content_hash,
        fetched_at,
        origin_store,
        policy_version,
        metadata,
        date_original,
        date_parsed,
        index_updated_at,
    })
}
