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

pub(crate) async fn delete_by_resource(
    store: &TenantStore,
    resource_id: &str,
) -> Result<usize, Error> {
    let conn = store.conn().conn().await;
    conn.execute("BEGIN", ()).await.map_err(map_libsql_err)?;
    let inner = delete_document_inner(&conn, store.store_id(), resource_id).await;
    match inner {
        Ok(count) => {
            conn.execute("COMMIT", ()).await.map_err(map_libsql_err)?;
            Ok(count)
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(e)
        }
    }
}

/// Connection-level helper: delete all chunks and the resource row for a
/// single document (`blocks` are removed via `ON DELETE CASCADE` from
/// `resources`).
///
/// This is the shared implementation behind both the standalone
/// `delete_by_resource` (wrapped in its own BEGIN/COMMIT-or-ROLLBACK, used for
/// source removal / store clearing) and the in-transaction delete performed by
/// `upsert_chunks_and_blocks` when replacing a document (issue #79): the
/// latter runs this against the transaction's own connection, between
/// `BEGIN` and the replacement insert, so a failure anywhere in that
/// transaction rolls back the delete along with the insert.
async fn delete_document_inner(
    conn: &Connection,
    store_id: &str,
    resource_id: &str,
) -> Result<usize, Error> {
    let chunk_count = conn
        .execute(
            "DELETE FROM chunks WHERE store_id = ? AND resource_id = ?",
            params![store_id.to_string(), resource_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    conn.execute(
        "DELETE FROM resources WHERE store_id = ? AND id = ?",
        params![store_id.to_string(), resource_id.to_string()],
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
    conn.execute("BEGIN", ()).await.map_err(map_libsql_err)?;
    let inner = delete_by_store_inner(&conn, store_id).await;
    match inner {
        Ok(chunk_count) => {
            conn.execute("COMMIT", ()).await.map_err(map_libsql_err)?;
            Ok(chunk_count)
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(e)
        }
    }
}

async fn delete_by_store_inner(conn: &Connection, store_id: &str) -> Result<usize, Error> {
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
    resource_id: &str,
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
                resource_id,
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
        // `record.resource_id` maps to the `id` column on `resources`.
        let resource_key = (record.store_id.clone(), record.resource_id.clone());
        if let std::collections::hash_map::Entry::Vacant(e) = seen_resources.entry(resource_key) {
            let metadata_json =
                serde_json::to_string(&record.metadata).map_err(|e| Error::Internal {
                    message: format!("upsert_chunks metadata serialize: {e}"),
                    correlation_id: "store_handle_upsert_meta".to_string(),
                })?;
            let title = record.metadata.title();
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
                    record.resource_id.as_str(), // id column
                    record.source_id.as_str(),
                    record.ingestor_kind.as_str(), // ingestor_kind column
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
        // location_json shape:
        // `{"start": N, "end": N, "window_block_seqs": [..], "page": N}`.
        // `window_block_seqs` is included only for message-window chunks (#129)
        // and `page` only for paginated formats (#103); a plain chunk keeps the
        // original `{start, end}` shape. Missing keys read back as their
        // defaults (empty / None) — no schema/DDL change (#103).
        let mut location_value = serde_json::json!({
            "start": record.span.start,
            "end": record.span.end,
        });
        if !record.window_block_seqs.is_empty() {
            location_value["window_block_seqs"] = serde_json::json!(record.window_block_seqs);
        }
        if let Some(page) = record.page {
            location_value["page"] = serde_json::json!(page);
        }
        let location_json =
            serde_json::to_string(&location_value).map_err(|e| Error::Internal {
                message: format!("upsert_chunks location_json serialize: {e}"),
                correlation_id: "store_handle_upsert_location".to_string(),
            })?;

        // The canonical block reference is `(store_id, resource_id, block_seq)` — no
        // `block_id`/rowid foreign key (#128): rowids aren't stable across a
        // replace, and window chunks (#129) reference a *set* of block seqs,
        // which a single scalar FK can't express.
        let sql = format!(
            "INSERT INTO chunks (store_id, id, resource_id, block_seq,
                 seq_in_block, block_kind, text, heading_path, location_json, embedding)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, {vector_sql})
             ON CONFLICT(store_id, id) DO UPDATE SET
                 resource_id  = excluded.resource_id,
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
                record.resource_id.as_str(), // resource_id column
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

/// Atomically upsert chunks and blocks in a single transaction, optionally
/// replacing an existing document first.
///
/// Unlike calling `upsert_chunks` and `upsert_blocks` separately (two
/// transactions), this wraps both writes in one BEGIN/COMMIT so the resource
/// can never appear indexed (chunks present) but un-blocked.
///
/// When `replaces_resource_id` is `Some(old_id)`, the old document's chunks,
/// blocks, and resource row are deleted **inside this same transaction**,
/// before the new records are inserted (issue #79). This closes the residual
/// same-run window from the A6 design decision (`docs/design-decisions.md`):
/// previously the replace delete ran in its own transaction, so a write
/// failure in the upsert that followed left the old chunks gone for the rest
/// of the run. Folding the delete into this transaction means a failure
/// anywhere below (including the delete-then-reinsert of the very same
/// `resource_id`, for a policy-only re-index) rolls back everything and
/// leaves the old resource intact and searchable.
pub(crate) async fn upsert_chunks_and_blocks(
    store: &TenantStore,
    resource_id: &str,
    records: Vec<ChunkRecord>,
    blocks: &[localdb_core::block::Block],
    replaces_resource_id: Option<&str>,
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
        if let Some(old_id) = replaces_resource_id {
            delete_document_inner(&conn, store.store_id(), old_id).await?;
        }
        upsert_chunks_inner(&conn, &records, store.encoding()).await?;
        upsert_blocks_inner(&conn, store.store_id(), resource_id, blocks).await?;
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
    resource_id: &str,
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
                resource_id,
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

#[cfg(test)]
mod tests {
    use localdb_core::metadata::Metadata;
    use localdb_core::types::{SourceKind, Span, StoreVisibility};
    use localdb_core::{SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
    use tempfile::tempdir;

    use crate::SqliteBackend;

    /// Regression test for issue C4 on the tenant read path
    /// (`tenant::rows::row_to_chunk_record_strict`, via
    /// `connection::parse_metadata_json_lenient`): a resource row with
    /// syntactically invalid `metadata_json` must still be readable through
    /// `get_chunk` — falling back to `Metadata::default()` — rather than
    /// erroring the whole read. This exercises the same shared helper that
    /// `registry::documents::find_document` covers on the registry side
    /// (`registry::tests::find_document_tolerates_invalid_metadata_json`).
    #[tokio::test]
    async fn get_chunk_tolerates_invalid_metadata_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");
        let backend = SqliteBackend::open(StoreBackendConfig::local_path(
            path,
            4,
            VectorEncoding::Float32,
        ))
        .await
        .unwrap();

        backend
            .upsert_store(&StoreRow {
                id: "store-1".to_string(),
                name: "notes".to_string(),
                visibility: StoreVisibility::Private,
                backend: "libsql".to_string(),
                indexing_policy: "{}".to_string(),
                policy_version: "v1".to_string(),
                acl: "{}".to_string(),
                created_at: "2026-07-01T00:00:00Z".to_string(),
            })
            .await
            .unwrap();
        backend
            .upsert_source(&SourceRow {
                id: "src-1".to_string(),
                store_id: "store-1".to_string(),
                kind: SourceKind::Path,
                root: Some("/docs".to_string()),
                url: None,
                include: vec![],
                exclude: vec![],
                preset: "prose".to_string(),
                refresh: None,
                created_at: "2026-07-01T00:00:00Z".to_string(),
                config_json: None,
            })
            .await
            .unwrap();

        let handle = backend.retrieval_store("store-1").await.unwrap();
        let record = localdb_core::ChunkRecord {
            id: "chunk-1".to_string(),
            resource_id: "doc-1".to_string(),
            store_id: "store-1".to_string(),
            text: "some chunk text".to_string(),
            span: Span::new(0, 15),
            heading_path: vec![],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "v1".to_string(),
            fetched_at: "2026-07-01T00:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            origin_store: "store-1".to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: "file:///docs/doc.md".to_string(),
            metadata: Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
        };
        handle.upsert_chunks(vec![record]).await.unwrap();

        // Corrupt the persisted metadata_json directly with syntactically
        // invalid JSON.
        let conn = backend.conn.conn().await;
        conn.execute(
            "UPDATE resources SET metadata_json = ? WHERE id = ?",
            libsql::params!["{not valid json".to_string(), "doc-1".to_string()],
        )
        .await
        .unwrap();
        drop(conn);

        let chunk = handle
            .get_chunk("chunk-1")
            .await
            .unwrap()
            .expect("chunk must still be found despite invalid metadata_json");
        assert_eq!(
            chunk.metadata,
            Metadata::default(),
            "invalid metadata_json must fall back to default metadata, not error the read"
        );
    }
}
