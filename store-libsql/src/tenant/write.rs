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
    let chunk_count = conn
        .execute(
            "DELETE FROM chunks WHERE store_id = ? AND document_id = ?",
            params![store.store_id().to_string(), document_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    conn.execute(
        "DELETE FROM documents WHERE store_id = ? AND id = ?",
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
        "DELETE FROM documents WHERE store_id = ?",
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

async fn upsert_chunks_inner(
    conn: &Connection,
    records: &[ChunkRecord],
    encoding: VectorEncoding,
) -> Result<(), Error> {
    let mut seen_documents: HashMap<(String, String), bool> = HashMap::new();
    let mut doc_seq_counters: HashMap<(String, String), i64> = HashMap::new();

    for record in records {
        let doc_key = (record.store_id.clone(), record.document_id.clone());
        if !seen_documents.contains_key(&doc_key) {
            let metadata_json =
                serde_json::to_string(&record.metadata).map_err(|e| Error::Internal {
                    message: format!("upsert_chunks metadata serialize: {e}"),
                    correlation_id: "store_handle_upsert_meta".to_string(),
                })?;
            let title = record.metadata.title.as_deref();
            conn.execute(
                "INSERT INTO documents (store_id, id, source_id, source_kind, uri, title, mime,
                     content_hash, fetched_at, origin_store, policy_version, metadata)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(store_id, id) DO UPDATE SET
                     source_id = excluded.source_id,
                     source_kind = excluded.source_kind,
                     uri = excluded.uri,
                     title = excluded.title,
                     mime = excluded.mime,
                     content_hash = excluded.content_hash,
                     fetched_at = excluded.fetched_at,
                     origin_store = excluded.origin_store,
                     policy_version = excluded.policy_version,
                     metadata = excluded.metadata",
                params![
                    record.store_id.as_str(),
                    record.document_id.as_str(),
                    record.source_id.as_str(),
                    record.source_kind.as_str(),
                    record.uri.as_str(),
                    title,
                    record.mime.as_deref(),
                    record.content_hash.as_str(),
                    record.fetched_at.as_str(),
                    record.origin_store.as_str(),
                    record.policy_version.as_str(),
                    metadata_json.as_str(),
                ],
            )
            .await
            .map_err(map_libsql_err)?;
            seen_documents.insert(doc_key.clone(), true);
        }

        let seq = doc_seq_counters.entry(doc_key.clone()).or_insert(0);
        let current_seq = *seq;
        *seq += 1;
        let vector_sql = match encoding {
            VectorEncoding::Float32 => vectors::f32_to_vector32_sql(&record.embedding),
            VectorEncoding::Binary => vectors::f32_to_vector1bit_sql(&record.embedding),
        };
        let heading_path_json =
            serde_json::to_string(&record.heading_path).map_err(|e| Error::Internal {
                message: format!("upsert_chunks heading_path serialize: {e}"),
                correlation_id: "store_handle_upsert_heading".to_string(),
            })?;
        let sql = format!(
            "INSERT INTO chunks (store_id, id, document_id, seq, text, span_start, span_end, heading_path, embedding)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, {vector_sql})
             ON CONFLICT(store_id, id) DO UPDATE SET
                 document_id = excluded.document_id,
                 seq = excluded.seq,
                 text = excluded.text,
                 span_start = excluded.span_start,
                 span_end = excluded.span_end,
                 heading_path = excluded.heading_path,
                 embedding = excluded.embedding"
        );
        conn.execute(
            &sql,
            params![
                record.store_id.as_str(),
                record.id.as_str(),
                record.document_id.as_str(),
                current_seq,
                record.text.as_str(),
                record.span.start as i64,
                record.span.end as i64,
                heading_path_json.as_str(),
            ],
        )
        .await
        .map_err(map_libsql_err)?;
    }
    Ok(())
}
