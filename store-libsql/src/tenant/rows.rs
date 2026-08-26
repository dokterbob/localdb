use localdb_core::block::{Block, BlockKind, BlockLocation};
use localdb_core::metadata::Metadata;
use localdb_core::types::Span;
use localdb_core::{ChunkRecord, Error};

use crate::connection::{map_libsql_err, parse_metadata_json_lenient};

/// Parse a row produced by the CHUNK_COLS projection in `read.rs`.
///
/// Column index map (must stay in sync with `read::CHUNK_COLS`):
///   0  c.id
///   1  c.resource_id
///   2  c.text
///   3  c.heading_path
///   4  embedding_json     (vector_extract result)
///   5  r.store_id
///   6  r.source_id
///   7  r.ingestor_kind
///   8  r.uri
///   9  r.title            (unused here; kept for positional alignment)
///  10  r.mime
///  11  r.policy_version
///  12  r.added_at         → fetched_at
///  13  r.modified_at      → modified_at
///  14  r.content_hash
///  15  r.origin_store
///  16  r.metadata_json    → metadata
///  17  c.block_seq
///  18  c.seq_in_block
///  19  c.location_json
///  20  c.block_kind
///  21  distance/score     (appended by each query, not read here)
pub(crate) fn row_to_chunk_record_strict(row: &libsql::Row) -> Result<ChunkRecord, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let resource_id: String = row.get(1).map_err(map_libsql_err)?;
    let text: String = row.get(2).map_err(map_libsql_err)?;
    let heading_path_str: String = row.get(3).map_err(map_libsql_err)?;
    let embedding_str: String = row.get(4).map_err(map_libsql_err)?;
    let store_id: String = row.get(5).map_err(map_libsql_err)?;
    let source_id: String = row.get(6).map_err(map_libsql_err)?;
    let ingestor_kind: String = row.get(7).map_err(map_libsql_err)?;
    let uri: String = row.get(8).map_err(map_libsql_err)?;
    let _title: Option<String> = row.get(9).map_err(map_libsql_err)?;
    let mime: Option<String> = row.get(10).map_err(map_libsql_err)?;
    let policy_version: String = row.get(11).map_err(map_libsql_err)?;
    let added_at: String = row.get(12).map_err(map_libsql_err)?; // → fetched_at
    let modified_at: String = row.get(13).map_err(map_libsql_err)?;
    let content_hash: String = row.get(14).map_err(map_libsql_err)?;
    let origin_store: String = row.get(15).map_err(map_libsql_err)?;
    let metadata_str: String = row.get(16).map_err(map_libsql_err)?;

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
    // Read defensively: rows written before the tagged-`Metadata` migration
    // (#130) hold untagged, flat Dublin Core JSON and fail to deserialize as
    // the tagged enum — fall back to `Metadata::default()` rather than
    // erroring the whole read. A genuine parse failure is logged (issue C4);
    // see `parse_metadata_json_lenient`.
    let metadata: Metadata = parse_metadata_json_lenient(&metadata_str, &resource_id);

    let block_seq: i64 = row.get(17).map_err(map_libsql_err)?;
    let seq_in_block: i64 = row.get(18).map_err(map_libsql_err)?;

    // location_json is written by upsert_chunks_inner; fall back to text length
    // for rows written before this column was populated. Shape:
    // `{"start": N, "end": N, "window_block_seqs": [..], "page": N}`, with
    // `window_block_seqs` present only for message-window chunks (#129) and
    // `page` only for paginated formats (#103) — both absent (and thus
    // defaulting to empty / None) for ordinary chunks.
    let text_len = text.len();
    let mut window_block_seqs: Vec<u32> = Vec::new();
    let mut page: Option<u32> = None;
    let span = {
        let location_json: Option<String> = row.get(19).map_err(map_libsql_err)?;
        match location_json {
            Some(json) => {
                let v: serde_json::Value =
                    serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                let start = v.get("start").and_then(|s| s.as_u64()).map(|s| s as usize);
                let end = v.get("end").and_then(|e| e.as_u64()).map(|e| e as usize);
                if let Some(seqs) = v.get("window_block_seqs").and_then(|w| w.as_array()) {
                    window_block_seqs = seqs
                        .iter()
                        .filter_map(|s| s.as_u64())
                        .map(|s| s as u32)
                        .collect();
                }
                page = v.get("page").and_then(|p| p.as_u64()).map(|p| p as u32);
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

    let block_kind: Option<String> = row.get(20).map_err(map_libsql_err)?;

    Ok(ChunkRecord {
        id,
        resource_id,
        store_id,
        text: text.clone(),
        span,
        heading_path,
        embedding,
        policy_version,
        fetched_at: added_at,
        modified_at,
        content_hash,
        origin_store,
        source_id,
        ingestor_kind,
        mime,
        uri,
        metadata,
        block_seq: block_seq as u32,
        seq_in_block: seq_in_block as u32,
        block_kind,
        page,
        window_block_seqs,
        // Write-only stamps (core::store::ChunkRecord doc comment) — not
        // part of `CHUNK_COLS`, so a chunk read never carries them.
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    })
}

/// Parse a row produced by `read::get_blocks_for_resource`'s `SELECT`.
///
/// Column index map:
///   0  seq
///   1  kind           (redundant discriminant string, unused here — the
///                      full typed `BlockKind` is reconstructed from
///                      `metadata_json` below)
///   2  text
///   3  metadata_json   → kind (tagged `BlockKind` JSON, written by
///                       `write::upsert_blocks_inner` as
///                       `serde_json::to_string(&block.kind)`)
///   4  location_json   → location
pub(crate) fn row_to_block(row: &libsql::Row) -> Result<Block, Error> {
    let seq: i64 = row.get(0).map_err(map_libsql_err)?;
    let _kind_str: String = row.get(1).map_err(map_libsql_err)?;
    let text: String = row.get(2).map_err(map_libsql_err)?;
    let metadata_json: Option<String> = row.get(3).map_err(map_libsql_err)?;
    let location_json: Option<String> = row.get(4).map_err(map_libsql_err)?;

    let kind: BlockKind = match metadata_json {
        Some(json) => serde_json::from_str(&json).map_err(|e| Error::Internal {
            message: format!("invalid block metadata_json: {e}"),
            correlation_id: "store_handle_row_block_kind".to_string(),
        })?,
        None => {
            return Err(Error::Internal {
                message: "block row missing metadata_json".to_string(),
                correlation_id: "store_handle_row_block_kind_missing".to_string(),
            })
        }
    };

    let location: Option<BlockLocation> = match location_json {
        Some(json) => Some(serde_json::from_str(&json).map_err(|e| Error::Internal {
            message: format!("invalid block location_json: {e}"),
            correlation_id: "store_handle_row_block_location".to_string(),
        })?),
        None => None,
    };

    Ok(Block {
        seq: seq as u32,
        kind,
        text,
        location,
    })
}
