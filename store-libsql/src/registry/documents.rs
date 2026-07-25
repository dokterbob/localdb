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
) -> Result<Option<DocumentInfo>, Error> {
    let conn = db.conn().await;
    // Column mapping from resources → DocumentInfo:
    //   resources.id           → DocumentInfo.id
    //   resources.added_at     → DocumentInfo.fetched_at
    //   resources.metadata_json → DocumentInfo.metadata
    let mut rows = conn
        .query(
            "SELECT store_id, id, source_id, ingestor_kind, uri, title, mime,
                        content_hash, added_at, origin_store, policy_version, metadata_json
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
    })
}
