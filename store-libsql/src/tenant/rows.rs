use localdb_core::parser::DocumentMetadata;
use localdb_core::types::Span;
use localdb_core::{ChunkRecord, Error};

use crate::connection::map_libsql_err;

/// Parse a row produced by the CHUNK_COLS projection in `read.rs`.
///
/// Column index map (must stay in sync with `read::CHUNK_COLS`):
///   0  c.id
///   1  c.resource_id      → document_id
///   2  c.text
///   3  c.heading_path
///   4  embedding_json     (vector_extract result)
///   5  r.store_id
///   6  r.source_id
///   7  r.ingestor_kind    → source_kind
///   8  r.uri
///   9  r.title            (unused here; kept for positional alignment)
///  10  r.mime
///  11  r.policy_version
///  12  r.added_at         → fetched_at
///  13  r.content_hash
///  14  r.origin_store
///  15  r.metadata_json    → metadata
///  16  c.block_seq
///  17  c.seq_in_block
///  18  c.location_json
///  19  distance/score     (appended by each query, not read here)
pub(crate) fn row_to_chunk_record_strict(row: &libsql::Row) -> Result<ChunkRecord, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let resource_id: String = row.get(1).map_err(map_libsql_err)?; // → document_id
    let text: String = row.get(2).map_err(map_libsql_err)?;
    let heading_path_str: String = row.get(3).map_err(map_libsql_err)?;
    let embedding_str: String = row.get(4).map_err(map_libsql_err)?;
    let store_id: String = row.get(5).map_err(map_libsql_err)?;
    let source_id: String = row.get(6).map_err(map_libsql_err)?;
    let ingestor_kind: String = row.get(7).map_err(map_libsql_err)?; // → source_kind
    let uri: String = row.get(8).map_err(map_libsql_err)?;
    let _title: Option<String> = row.get(9).map_err(map_libsql_err)?;
    let mime: Option<String> = row.get(10).map_err(map_libsql_err)?;
    let policy_version: String = row.get(11).map_err(map_libsql_err)?;
    let added_at: String = row.get(12).map_err(map_libsql_err)?; // → fetched_at
    let content_hash: String = row.get(13).map_err(map_libsql_err)?;
    let origin_store: String = row.get(14).map_err(map_libsql_err)?;
    let metadata_str: String = row.get(15).map_err(map_libsql_err)?;

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

    let block_seq: i64 = row.get(16).map_err(map_libsql_err)?;
    let seq_in_block: i64 = row.get(17).map_err(map_libsql_err)?;

    // location_json is written by upsert_chunks_inner; fall back to text length
    // for rows written before this column was populated.
    let text_len = text.len();
    let span = {
        let location_json: Option<String> = row.get(18).map_err(map_libsql_err)?;
        match location_json {
            Some(json) => {
                let v: serde_json::Value =
                    serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                let start = v.get("start").and_then(|s| s.as_u64()).map(|s| s as usize);
                let end = v.get("end").and_then(|e| e.as_u64()).map(|e| e as usize);
                match (start, end) {
                    (Some(s), Some(e)) => Span { start: s, end: e },
                    _ => Span {
                        start: 0,
                        end: text_len,
                    },
                }
            }
            None => Span {
                start: 0,
                end: text_len,
            },
        }
    };

    Ok(ChunkRecord {
        id,
        document_id: resource_id,
        store_id,
        text: text.clone(),
        span,
        heading_path,
        embedding,
        policy_version,
        fetched_at: added_at,
        content_hash,
        origin_store,
        source_id,
        source_kind: ingestor_kind,
        mime,
        uri,
        metadata,
        block_seq: block_seq as u32,
        seq_in_block: seq_in_block as u32,
        block_kind: None, // not stored in DB yet; follow-up migration pending
    })
}
