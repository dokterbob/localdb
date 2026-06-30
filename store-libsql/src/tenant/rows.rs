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

    // Span is no longer stored per-chunk in the new schema; use the text
    // length as a proxy so existing callers that read the span field get a
    // safe default rather than a zero-length span.
    let text_len = text.len();

    Ok(ChunkRecord {
        id,
        document_id: resource_id, // schema rename: resource_id → document_id
        store_id,
        text: text.clone(),
        span: Span {
            start: 0,
            end: text_len,
        },
        heading_path,
        embedding,
        policy_version,
        fetched_at: added_at, // schema rename: added_at → fetched_at
        content_hash,
        origin_store,
        source_id,
        source_kind: ingestor_kind, // schema rename: ingestor_kind → source_kind
        mime,
        uri,
        metadata,
        block_seq: 0,
        seq_in_block: 0,
    })
}
