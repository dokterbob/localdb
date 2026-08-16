use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use libsql::{params, Connection};
use localdb_core::{ChunkRecord, Error, VectorEncoding};

use super::TenantStore;
use crate::connection::map_libsql_err;
use crate::vectors;

/// A boxed, single-use future returned by an [`in_write_tx`] closure.
///
/// This is the standard "boxed HRTB async closure" pattern: a bare generic
/// `Fut: Future<...>` associated type can't itself vary per higher-ranked
/// lifetime (the closure needs to borrow a fresh `&'c Connection` on each
/// call and hold it across `.await` points), but a `Pin<Box<dyn Future<...>
/// + 'c>>` trait object can, because its type explicitly names `'c`.
type WriteFut<'c, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'c>>;

/// Run `f` against a fresh [`crate::connection::LibsqlDb::write_tx`],
/// committing on success and rolling back explicitly on failure.
///
/// Explicit rollback is the primary error path here, not `WriteTx`'s `Drop`
/// backstop: if the rollback itself fails, that's logged rather than
/// propagated (or panicking, which is what an unhandled `Drop`-rollback
/// failure would do) — the original write error `e` is still what's
/// returned to the caller, since that's the failure the caller actually
/// needs to see and react to.
async fn in_write_tx<T, F>(store: &TenantStore, f: F) -> Result<T, Error>
where
    F: for<'c> FnOnce(&'c Connection) -> WriteFut<'c, T>,
{
    let tx = store.conn().write_tx().await?;
    match f(&tx).await {
        Ok(v) => {
            tx.commit().await?;
            Ok(v)
        }
        Err(e) => {
            if let Err(rollback_err) = tx.rollback().await {
                tracing::error!(
                    error = %rollback_err,
                    "explicit rollback after write failure also failed"
                );
            }
            Err(e)
        }
    }
}

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
    let count = records.len();
    let encoding = store.encoding();
    in_write_tx(store, move |conn: &Connection| -> WriteFut<'_, usize> {
        Box::pin(async move {
            upsert_chunks_inner(conn, &records, encoding).await?;
            Ok(count)
        })
    })
    .await
}

pub(crate) async fn delete_by_resource(
    store: &TenantStore,
    resource_id: &str,
) -> Result<usize, Error> {
    // Owned, not borrowed: the closure below must satisfy `for<'c> FnOnce(&'c
    // Connection) -> WriteFut<'c, T>` for *any* `'c`, including lifetimes
    // longer than `store`/`resource_id`'s own — so it can only borrow the
    // freshly-supplied `&'c Connection`, nothing else.
    let store_id = store.store_id().to_string();
    let resource_id = resource_id.to_string();
    in_write_tx(store, move |conn: &Connection| -> WriteFut<'_, usize> {
        Box::pin(async move { delete_document_inner(conn, &store_id, &resource_id).await })
    })
    .await
}

/// Connection-level helper: delete all chunks and the resource row for a
/// single document (`blocks` are removed via `ON DELETE CASCADE` from
/// `resources`).
///
/// This is the shared implementation behind both the standalone
/// `delete_by_resource` (wrapped in its own `write_tx`, used for source
/// removal / store clearing) and the in-transaction delete performed by
/// `upsert_chunks_and_blocks` when replacing a document (issue #79): the
/// latter runs this against the transaction's own connection, before the
/// replacement insert, so a failure anywhere in that transaction rolls back
/// the delete along with the insert.
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
    let store_id = store_id.to_string();
    in_write_tx(store, move |conn: &Connection| -> WriteFut<'_, usize> {
        Box::pin(async move { delete_by_store_inner(conn, &store_id).await })
    })
    .await
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
    let store_id = store.store_id().to_string();
    let resource_id = resource_id.to_string();
    let blocks = blocks.to_vec();
    in_write_tx(store, move |conn: &Connection| -> WriteFut<'_, ()> {
        Box::pin(async move { upsert_blocks_inner(conn, &store_id, &resource_id, &blocks).await })
    })
    .await
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
/// transactions), this wraps both writes in one `write_tx` so the resource
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
    let count = records.len();
    let store_id = store.store_id().to_string();
    let encoding = store.encoding();
    let resource_id = resource_id.to_string();
    let blocks = blocks.to_vec();
    let replaces_resource_id = replaces_resource_id.map(str::to_string);
    in_write_tx(store, move |conn: &Connection| -> WriteFut<'_, usize> {
        Box::pin(async move {
            if let Some(old_id) = replaces_resource_id.as_deref() {
                delete_document_inner(conn, &store_id, old_id).await?;
            }
            upsert_chunks_inner(conn, &records, encoding).await?;
            upsert_blocks_inner(conn, &store_id, &resource_id, &blocks).await?;
            Ok(count)
        })
    })
    .await
}

/// Inner (connection-level) helper for upserting blocks within an existing
/// `write_tx`. The sole implementation shared by both the standalone
/// `upsert_blocks` (its own single-purpose `write_tx`) and
/// `upsert_chunks_and_blocks` (blocks upserted as one step of a larger
/// transaction).
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
