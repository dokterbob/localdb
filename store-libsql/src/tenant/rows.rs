use localdb_core::parser::DocumentMetadata;
use localdb_core::types::Span;
use localdb_core::{ChunkRecord, Error};

use crate::connection::map_libsql_err;

pub(crate) fn row_to_chunk_record_strict(row: &libsql::Row) -> Result<ChunkRecord, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let document_id: String = row.get(1).map_err(map_libsql_err)?;
    let _seq: i64 = row.get(2).map_err(map_libsql_err)?;
    let text: String = row.get(3).map_err(map_libsql_err)?;
    let span_start: i64 = row.get(4).map_err(map_libsql_err)?;
    let span_end: i64 = row.get(5).map_err(map_libsql_err)?;
    let heading_path_str: String = row.get(6).map_err(map_libsql_err)?;
    let embedding_str: String = row.get(7).map_err(map_libsql_err)?;
    let store_id: String = row.get(8).map_err(map_libsql_err)?;
    let source_id: String = row.get(9).map_err(map_libsql_err)?;
    let source_kind: String = row.get(10).map_err(map_libsql_err)?;
    let uri: String = row.get(11).map_err(map_libsql_err)?;
    let _title: Option<String> = row.get(12).map_err(map_libsql_err)?;
    let mime: Option<String> = row.get(13).map_err(map_libsql_err)?;
    let policy_version: String = row.get(14).map_err(map_libsql_err)?;
    let fetched_at: String = row.get(15).map_err(map_libsql_err)?;
    let content_hash: String = row.get(16).map_err(map_libsql_err)?;
    let origin_store: String = row.get(17).map_err(map_libsql_err)?;
    let metadata_str: String = row.get(18).map_err(map_libsql_err)?;

    let heading_path: Vec<String> =
        serde_json::from_str(&heading_path_str).map_err(|e| Error::Internal {
            message: format!("invalid heading_path JSON: {e}"),
            correlation_id: "store_handle_row_heading".to_string(),
        })?;
    let embedding: Vec<f32> =
        serde_json::from_str(&embedding_str).map_err(|e| Error::Internal {
            message: format!("invalid embedding JSON: {e}"),
            correlation_id: "store_handle_row_embedding".to_string(),
        })?;
    let metadata: DocumentMetadata =
        serde_json::from_str(&metadata_str).map_err(|e| Error::Internal {
            message: format!("invalid metadata JSON: {e}"),
            correlation_id: "store_handle_row_metadata".to_string(),
        })?;

    Ok(ChunkRecord {
        id,
        document_id,
        store_id,
        text,
        span: Span {
            start: span_start as usize,
            end: span_end as usize,
        },
        heading_path,
        embedding,
        policy_version,
        fetched_at,
        content_hash,
        origin_store,
        source_id,
        source_kind,
        mime,
        uri,
        metadata,
    })
}
