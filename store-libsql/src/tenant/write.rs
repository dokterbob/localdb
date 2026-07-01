use std::collections::HashMap;

use libsql::{params, Connection};
use localdb_core::{ChunkRecord, Error, VectorEncoding};

use super::TenantStore;
use crate::connection::map_libsql_err;
use crate::vectors;

pub(crate) async fn upsert_chunks(
    store: &TenantStore,
    records: Vec<ChunkRecord>,
) -> Result<usize, Error> {
    for record in &records {
        if record.store_id != store.store_id() {
            return tenant_violation(format!(
                "chunk '{id}' has store_id '{rec}' but handle owns store_id '{handle}'",
                id = record.id,
                rec = record.store_id,
                handle = store.store_id()
            ));
        }
    }
    let conn = store.conn().conn().await;
    let count = records.len();
    conn.execute("BEGIN", ()).await.map_err(map_libsql_err)?;
    let inner = upsert_chunks_inner(&conn, &records, store.encoding()).await;
    match inner {
        Ok(()) => {
            conn.execute("COMMIT", ()).await.map_err(map_libsql_err)?;
            Ok(count)
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(e)
        }
    }
}

pub(crate) async fn delete_by_document(
    store: &TenantStore,
    document_id: &str,
) -> Result<usize, Error> {
    let conn = store.conn().conn().await;
    // `document_id` on ChunkRecord maps to `resource_id` in the schema.
    let chunk_count = conn
        .execute(
            "DELETE FROM chunks WHERE store_id = ? AND resource_id = ?",
            params![store.store_id().to_string(), document_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    conn.execute(
        "DELETE FROM resources WHERE store_id = ? AND id = ?",
        params![store.store_id().to_string(), document_id.to_string()],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(chunk_count as usize)
}

pub(crate) async fn delete_by_store(store: &TenantStore, store_id: &str) -> Result<usize, Error> {
    if store_id != store.store_id() {
        return tenant_violation(format!(
            "delete_by_store requested store_id '{store_id}' but handle owns store_id '{handle}'",
            handle = store.store_id()
        ));
    }
    let conn = store.conn().conn().await;
    let chunk_count = conn
        .execute(
            "DELETE FROM chunks WHERE store_id = ?",
            params![store_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    conn.execute(
        "DELETE FROM resources WHERE store_id = ?",
        params![store_id.to_string()],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(chunk_count as usize)
}

fn tenant_violation<T>(message: String) -> Result<T, Error> {
    Err(Error::Internal {
        message,
        correlation_id: "store_handle_tenant_violation".to_string(),
    })
}

pub(crate) async fn upsert_blocks(
    store: &TenantStore,
    document_id: &str,
    blocks: &[localdb_core::block::Block],
) -> Result<(), localdb_core::Error> {
    let conn = store.conn().conn().await;
    for block in blocks {
        let kind_str = block.kind.kind_str();
        let metadata_json =
            serde_json::to_string(&block.kind).map_err(|e| localdb_core::Error::Internal {
                message: format!("block metadata serialize: {e}"),
                correlation_id: "store_upsert_blocks_meta".to_string(),
            })?;
        let location_json = block
            .location
            .as_ref()
            .map(|loc| serde_json::to_string(loc).unwrap_or_default());
        conn.execute(
            "INSERT INTO blocks (store_id, resource_id, seq, kind, text, metadata_json, location_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(store_id, resource_id, seq) DO UPDATE SET
                 kind = excluded.kind,
                 text = excluded.text,
                 metadata_json = excluded.metadata_json,
                 location_json = excluded.location_json",
            libsql::params![
                store.store_id(),
                document_id,
                block.seq as i64,
                kind_str,
                block.text.as_str(),
                metadata_json.as_str(),
                location_json.as_deref(),
            ],
        )
        .await
        .map_err(crate::connection::map_libsql_err)?;
    }
    Ok(())
}

async fn upsert_chunks_inner(
    conn: &Connection,
    records: &[ChunkRecord],
    encoding: VectorEncoding,
) -> Result<(), Error> {
    // Track which (store_id, resource_id) pairs we've already upserted in this
    // batch so we don't issue duplicate resource upserts.
    let mut seen_resources: HashMap<(String, String), bool> = HashMap::new();

    for record in records {
        // `document_id` on ChunkRecord maps to `id` (and `resource_id`) in the
        // new schema.  `source_kind` maps to `ingestor_kind`.
        let resource_key = (record.store_id.clone(), record.document_id.clone());
        if let std::collections::hash_map::Entry::Vacant(e) = seen_resources.entry(resource_key) {
            // TODO(#130): record.metadata is flat parser::DocumentMetadata; should serialize as tagged Metadata enum once Resource-based reads land (#117)
            let metadata_json =
                serde_json::to_string(&record.metadata).map_err(|e| Error::Internal {
                    message: format!("upsert_chunks metadata serialize: {e}"),
                    correlation_id: "store_handle_upsert_meta".to_string(),
                })?;
            let title = record.metadata.title.as_deref();
            conn.execute(
                "INSERT INTO resources (store_id, id, source_id, ingestor_kind, resource_kind,
                     uri, title, mime, content_hash, added_at, modified_at, origin_store,
                     policy_version, metadata_json, extractor_version)
                 VALUES (?, ?, ?, ?, 'document', ?, ?, ?, ?, ?, ?, ?, ?, ?, '1')
                 ON CONFLICT(store_id, id) DO UPDATE SET
                     source_id      = excluded.source_id,
                     ingestor_kind  = excluded.ingestor_kind,
                     uri            = excluded.uri,
                     title          = excluded.title,
                     mime           = excluded.mime,
                     content_hash   = excluded.content_hash,
                     modified_at    = excluded.modified_at,
                     origin_store   = excluded.origin_store,
                     policy_version = excluded.policy_version,
                     metadata_json  = excluded.metadata_json",
                params![
                    record.store_id.as_str(),
                    record.document_id.as_str(), // id column
                    record.source_id.as_str(),
                    record.source_kind.as_str(), // ingestor_kind column
                    record.uri.as_str(),
                    title,
                    record.mime.as_deref(),
                    record.content_hash.as_str(),
                    record.fetched_at.as_str(), // added_at column
                    record.fetched_at.as_str(), // modified_at column
                    record.origin_store.as_str(),
                    record.policy_version.as_str(),
                    metadata_json.as_str(),
                ],
            )
            .await
            .map_err(map_libsql_err)?;
            e.insert(true);
        }

        let vector_sql = match encoding {
            VectorEncoding::Float32 => vectors::f32_to_vector32_sql(&record.embedding),
            VectorEncoding::Binary => vectors::f32_to_vector1bit_sql(&record.embedding),
        };
        let heading_path_json =
            serde_json::to_string(&record.heading_path).map_err(|e| Error::Internal {
                message: format!("upsert_chunks heading_path serialize: {e}"),
                correlation_id: "store_handle_upsert_heading".to_string(),
            })?;
        let location_json = serde_json::to_string(&serde_json::json!({
            "start": record.span.start,
            "end": record.span.end,
        }))
        .map_err(|e| Error::Internal {
            message: format!("upsert_chunks location_json serialize: {e}"),
            correlation_id: "store_handle_upsert_location".to_string(),
        })?;

        // TODO(#128): block_id is hardcoded to 0; should reference the actual blocks.rowid
        let sql = format!(
            "INSERT INTO chunks (store_id, id, resource_id, block_id, block_seq,
                 seq_in_block, block_kind, text, heading_path, location_json, embedding)
             VALUES (?, ?, ?, 0, ?, ?, ?, ?, ?, ?, {vector_sql})
             ON CONFLICT(store_id, id) DO UPDATE SET
                 resource_id  = excluded.resource_id,
                 block_id     = excluded.block_id,
                 block_seq    = excluded.block_seq,
                 seq_in_block = excluded.seq_in_block,
                 block_kind   = excluded.block_kind,
                 text         = excluded.text,
                 heading_path = excluded.heading_path,
                 location_json = excluded.location_json,
                 embedding    = excluded.embedding"
        );
        conn.execute(
            &sql,
            params![
                record.store_id.as_str(),
                record.id.as_str(),
                record.document_id.as_str(), // resource_id column
                record.block_seq as i64,
                record.seq_in_block as i64,
                record.block_kind.as_deref(),
                record.text.as_str(),
                heading_path_json.as_str(),
                location_json.as_str(),
            ],
        )
        .await
        .map_err(map_libsql_err)?;
    }
    Ok(())
}

/// Atomically upsert chunks and blocks in a single transaction.
///
/// Unlike calling `upsert_chunks` and `upsert_blocks` separately (two
/// transactions), this wraps both writes in one BEGIN/COMMIT so the resource
/// can never appear indexed (chunks present) but un-blocked.
pub(crate) async fn upsert_chunks_and_blocks(
    store: &TenantStore,
    document_id: &str,
    records: Vec<ChunkRecord>,
    blocks: &[localdb_core::block::Block],
) -> Result<usize, localdb_core::Error> {
    for record in &records {
        if record.store_id != store.store_id() {
            return Err(localdb_core::Error::Internal {
                message: format!(
                    "chunk '{id}' has store_id '{rec}' but handle owns store_id '{handle}'",
                    id = record.id,
                    rec = record.store_id,
                    handle = store.store_id()
                ),
                correlation_id: "store_handle_tenant_violation".to_string(),
            });
        }
    }
    let conn = store.conn().conn().await;
    let count = records.len();
    conn.execute("BEGIN", ()).await.map_err(map_libsql_err)?;
    let inner = async {
        upsert_chunks_inner(&conn, &records, store.encoding()).await?;
        upsert_blocks_inner(&conn, store.store_id(), document_id, blocks).await?;
        Ok::<(), localdb_core::Error>(())
    }
    .await;
    match inner {
        Ok(()) => {
            conn.execute("COMMIT", ()).await.map_err(map_libsql_err)?;
            Ok(count)
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(e)
        }
    }
}

/// Inner (connection-level) helper for upserting blocks within an existing transaction.
async fn upsert_blocks_inner(
    conn: &Connection,
    store_id: &str,
    document_id: &str,
    blocks: &[localdb_core::block::Block],
) -> Result<(), localdb_core::Error> {
    for block in blocks {
        let kind_str = block.kind.kind_str();
        let metadata_json =
            serde_json::to_string(&block.kind).map_err(|e| localdb_core::Error::Internal {
                message: format!("block metadata serialize: {e}"),
                correlation_id: "store_upsert_blocks_meta".to_string(),
            })?;
        let location_json = block
            .location
            .as_ref()
            .map(|loc| serde_json::to_string(loc).unwrap_or_default());
        conn.execute(
            "INSERT INTO blocks (store_id, resource_id, seq, kind, text, metadata_json, location_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(store_id, resource_id, seq) DO UPDATE SET
                 kind = excluded.kind,
                 text = excluded.text,
                 metadata_json = excluded.metadata_json,
                 location_json = excluded.location_json",
            libsql::params![
                store_id,
                document_id,
                block.seq as i64,
                kind_str,
                block.text.as_str(),
                metadata_json.as_str(),
                location_json.as_deref(),
            ],
        )
        .await
        .map_err(map_libsql_err)?;
    }
    Ok(())
}
