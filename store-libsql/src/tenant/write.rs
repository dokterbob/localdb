use std::collections::HashMap;

use libsql::{params, Connection};
use localdb_core::{ChunkRecord, Error, ResourceRecord, VectorEncoding};

use super::TenantStore;
use crate::connection::{map_libsql_err, WriteTx};
use crate::vectors;

/// Commit `result`'s transaction on `Ok`, roll back explicitly on `Err`
/// (logging a rollback failure rather than propagating it), returning the
/// original error either way — that's the failure the caller actually needs
/// to see and react to.
///
/// Explicit rollback here — not `WriteTx`'s `Drop` backstop — is the primary
/// error path: `commit()`/`rollback()` return a `Result` we can log and
/// handle, which Drop's own rollback can't (it panics on failure instead).
/// That said, this isn't fully panic-proof either — see
/// [`crate::connection::WriteTx`]'s doc comment for the caveat: a
/// persistently-failing COMMIT/ROLLBACK can still panic inside libsql before
/// this function's error handling ever runs. Do not "simplify" the arms
/// below away in favor of Drop on the strength of that caveat — explicit
/// remains strictly better, handling and logging every failure Drop can't
/// return at all.
async fn finish_write_tx<T>(tx: WriteTx<'_>, result: Result<T, Error>) -> Result<T, Error> {
    match result {
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
    let tx = store.conn().write_tx().await?;
    let result = upsert_chunks_inner(&tx, &records, store.encoding())
        .await
        .map(|()| count);
    finish_write_tx(tx, result).await
}

pub(crate) async fn delete_by_resource(
    store: &TenantStore,
    resource_id: &str,
) -> Result<usize, Error> {
    let tx = store.conn().write_tx().await?;
    let result = delete_document_inner(&tx, store.store_id(), resource_id).await;
    finish_write_tx(tx, result).await
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

/// Connection-level helper: delete a document's chunks and blocks only —
/// the `resources` row is left in place.
///
/// Used by `upsert_chunks_and_blocks`'s same-resource-id replace path (a
/// policy-only re-index, where `replaces_resource_id == Some(resource_id)`):
/// leaving the `resources` row in place means the INSERT that follows hits
/// its `ON CONFLICT(store_id, id) DO UPDATE` instead of a fresh insert, and
/// `added_at` — deliberately absent from that `DO UPDATE SET` list — is
/// preserved for free. `modified_at`/`index_updated_at`/etc. still refresh
/// normally, since they're bound from the new scan and ARE in that SET list.
///
/// Both `chunks` and `blocks` carry their own `ON DELETE CASCADE` FK to
/// `resources(store_id, id)` (`schema.rs`), which is why
/// `delete_document_inner` can get away with deleting `chunks` explicitly and
/// letting `blocks` cascade off the `resources` row's own delete — but with
/// `resources` staying put here, both need an explicit delete.
async fn delete_chunks_and_blocks_inner(
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
        "DELETE FROM blocks WHERE store_id = ? AND resource_id = ?",
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
    let tx = store.conn().write_tx().await?;
    let result = delete_by_store_inner(&tx, store_id).await;
    finish_write_tx(tx, result).await
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

/// Rewrite an existing resource row's metadata in place (issue #176's
/// metadata-only incremental update) — no chunks/blocks/embeddings touched.
///
/// Own `write_tx`, mirroring `delete_by_resource`/`upsert_blocks` above
/// rather than `upsert_chunks_and_blocks`'s inline-`async` composition: this
/// is a single-statement write with nothing else to compose it with.
pub(crate) async fn update_resource_metadata(
    store: &TenantStore,
    store_id: &str,
    resource_id: &str,
    record: &ResourceRecord,
) -> Result<(), Error> {
    if store_id != store.store_id() {
        return tenant_violation(format!(
            "update_resource_metadata requested store_id '{store_id}' but handle owns store_id \
             '{handle}'",
            handle = store.store_id()
        ));
    }
    let tx = store.conn().write_tx().await?;
    let result = update_resource_metadata_inner(&tx, store_id, resource_id, record).await;
    finish_write_tx(tx, result).await
}

async fn update_resource_metadata_inner(
    conn: &Connection,
    store_id: &str,
    resource_id: &str,
    record: &ResourceRecord,
) -> Result<(), Error> {
    let metadata_json = serde_json::to_string(&record.metadata).map_err(|e| Error::Internal {
        message: format!("update_resource_metadata metadata serialize: {e}"),
        correlation_id: "store_handle_update_metadata".to_string(),
    })?;
    let title = record.metadata.title();
    // One write-time clock reading for this write, mirroring
    // `upsert_chunks_inner`'s single `now_rfc3339()` call per batch — see
    // `ResourceRecord`'s doc comment for why `index_updated_at` isn't a
    // caller-supplied field.
    let index_updated_at = localdb_core::ingestion::now_rfc3339();
    let rows_affected = conn
        .execute(
            "UPDATE resources SET
                 metadata_json    = ?,
                 title            = ?,
                 external_id      = ?,
                 external_etag    = ?,
                 modified_at      = ?,
                 date_original    = ?,
                 date_parsed      = ?,
                 index_updated_at = ?
             WHERE store_id = ? AND id = ?",
            params![
                metadata_json.as_str(),
                title,
                record.external_id.as_deref(),
                record.external_etag.as_deref(),
                record.modified_at.as_deref(),
                record.date_original.as_deref(),
                record.date_parsed.as_deref(),
                index_updated_at.as_str(),
                store_id.to_string(),
                resource_id.to_string(),
            ],
        )
        .await
        .map_err(map_libsql_err)?;
    if rows_affected == 0 {
        // The row vanished — a concurrent delete raced this update. Report
        // it rather than silently succeeding: the caller's `DocumentIndex`
        // entry would otherwise be stamped with a metadata_hash for a
        // resource_id the store no longer has any row for, which the next
        // run's skip-check would compare against a phantom.
        return Err(Error::ResourceNotFound {
            id: resource_id.to_string(),
        });
    }
    Ok(())
}

pub(crate) async fn upsert_blocks(
    store: &TenantStore,
    resource_id: &str,
    blocks: &[localdb_core::block::Block],
) -> Result<(), localdb_core::Error> {
    let tx = store.conn().write_tx().await?;
    let result = upsert_blocks_inner(&tx, store.store_id(), resource_id, blocks).await;
    finish_write_tx(tx, result).await
}

async fn upsert_chunks_inner(
    conn: &Connection,
    records: &[ChunkRecord],
    encoding: VectorEncoding,
) -> Result<(), Error> {
    // Track which (store_id, resource_id) pairs we've already upserted in this
    // batch so we don't issue duplicate resource upserts.
    let mut seen_resources: HashMap<(String, String), bool> = HashMap::new();
    // One write-time clock reading for the whole batch: every resource
    // touched by this call was written "now", by definition of this call
    // happening now. See specs/02-domain-model.md §2.
    let index_updated_at = localdb_core::ingestion::now_rfc3339();

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
                     uri, title, mime, content_hash, added_at, modified_at, index_updated_at,
                     origin_store, policy_version, metadata_json, extractor_version,
                     date_original, date_parsed, external_id, external_etag)
                 VALUES (?, ?, ?, ?, 'document', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '1', ?, ?, ?, ?)
                 ON CONFLICT(store_id, id) DO UPDATE SET
                     source_id        = excluded.source_id,
                     ingestor_kind    = excluded.ingestor_kind,
                     uri              = excluded.uri,
                     title            = excluded.title,
                     mime             = excluded.mime,
                     content_hash     = excluded.content_hash,
                     modified_at      = excluded.modified_at,
                     index_updated_at = excluded.index_updated_at,
                     origin_store     = excluded.origin_store,
                     policy_version   = excluded.policy_version,
                     metadata_json    = excluded.metadata_json,
                     date_original    = excluded.date_original,
                     date_parsed      = excluded.date_parsed,
                     external_id      = excluded.external_id,
                     external_etag    = excluded.external_etag",
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
                    record.modified_at.as_deref(),
                    index_updated_at.as_str(),
                    record.origin_store.as_str(),
                    record.policy_version.as_str(),
                    metadata_json.as_str(),
                    record.date_original.as_deref(),
                    record.date_parsed.as_deref(),
                    record.external_id.as_deref(),
                    record.external_etag.as_deref(),
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
/// When `replaces_resource_id` is `Some(old_id)`, the old document's chunks
/// and blocks are deleted **inside this same transaction**, before the new
/// records are inserted (issue #79). This closes the residual same-run
/// window from the A6 design decision (`docs/design-decisions.md`):
/// previously the replace delete ran in its own transaction, so a write
/// failure in the upsert that followed left the old chunks gone for the rest
/// of the run. Folding the delete into this transaction means a failure
/// anywhere below rolls back everything and leaves the old resource intact
/// and searchable.
///
/// Whether the old `resources` row itself is deleted depends on whether
/// `old_id` names the *same* resource being written here (a policy-only
/// re-index — chunk boundaries can change under a new chunking policy even
/// though the content and its identity haven't) or a genuinely different one
/// (content changed enough to mint a new content-addressed ID): see
/// `delete_chunks_and_blocks_inner`'s doc comment for why the same-ID case
/// keeps the row in place.
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
    let tx = store.conn().write_tx().await?;
    // Inline `async` block, not a separate closure/helper: it only borrows
    // `&tx` (and the still-owned `store`/`records`/`resource_id`/`blocks`/
    // `replaces_resource_id` locals) for the duration of this single
    // `.await` right below — an ordinary borrow the compiler checks against
    // this function's own scope, no `'static` bound or boxing required.
    let result: Result<usize, localdb_core::Error> = async {
        if let Some(old_id) = replaces_resource_id {
            if old_id == resource_id {
                delete_chunks_and_blocks_inner(&tx, store.store_id(), old_id).await?;
            } else {
                delete_document_inner(&tx, store.store_id(), old_id).await?;
            }
        }
        upsert_chunks_inner(&tx, &records, store.encoding()).await?;
        upsert_blocks_inner(&tx, store.store_id(), resource_id, blocks).await?;
        Ok(count)
    }
    .await;
    finish_write_tx(tx, result).await
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
