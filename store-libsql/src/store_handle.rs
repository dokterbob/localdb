//! Per-store view of the unified DB.
//!
//! `StoreHandle` wraps `Arc<LibsqlDb>` plus a `store_id` and implements
//! `RetrievalStore` with that `store_id` as the tenant filter on every read.
//!
//! Writes (`upsert_chunks`, `delete_by_document`) operate within the
//! handle's tenant. `delete_by_store` honours its `store_id` parameter
//! (it can purge any tenant — not necessarily this handle's — matching the
//! existing trait semantics). Read methods (`stats`, `dense_search`,
//! `bm25_search`, `get_chunk`, `get_chunks_for_document`,
//! `list_indexed_documents`) always filter by the handle's `store_id`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use libsql::{params, Connection};

use localdb_core::ingestion::DocumentRecord;
use localdb_core::parser::DocumentMetadata;
use localdb_core::store::{ChunkRecord, MetadataFilter, RetrievalStore, SearchResult, StoreStats};
use localdb_core::types::Span;
use localdb_core::{Error, VectorEncoding};

use crate::db::{map_libsql_err, LibsqlDb};
use crate::{build_filter_clauses, escape_fts5_query, vectors};

/// A single-tenant view over the unified DB, implementing `RetrievalStore`.
pub struct StoreHandle {
    db: Arc<LibsqlDb>,
    store_id: String,
}

impl StoreHandle {
    pub fn new(db: Arc<LibsqlDb>, store_id: impl Into<String>) -> Self {
        Self {
            db,
            store_id: store_id.into(),
        }
    }

    pub fn store_id(&self) -> &str {
        &self.store_id
    }
}

#[async_trait]
impl RetrievalStore for StoreHandle {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        let conn = self.db.conn().await;
        let count = records.len();
        let encoding = self.db.encoding();

        conn.execute("BEGIN", ()).await.map_err(map_libsql_err)?;
        let inner = upsert_chunks_inner(&conn, &records, encoding).await;
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

    async fn delete_by_document(&self, document_id: &str) -> Result<usize, Error> {
        let conn = self.db.conn().await;
        let chunk_count = conn
            .execute(
                "DELETE FROM chunks WHERE store_id = ? AND document_id = ?",
                params![self.store_id.clone(), document_id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;
        conn.execute(
            "DELETE FROM documents WHERE store_id = ? AND id = ?",
            params![self.store_id.clone(), document_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
        Ok(chunk_count as usize)
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        let conn = self.db.conn().await;
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

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let conn = self.db.conn().await;

        let filter_clauses = build_filter_clauses(filters);
        let has_filters = !filters.is_empty();
        let encoding = self.db.encoding();
        let dim = self.db.embedding_dim();

        let mut fetch_k = if has_filters { limit * 3 } else { limit };
        let max_fetch = limit * 20;

        loop {
            let qvec_sql = vectors::query_vector_sql(query_vector, encoding);
            let escaped_store_id = self.store_id.replace('\'', "''");
            let sql = format!(
                "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                        c.heading_path, vector_extract(c.embedding) AS embedding_json,
                        d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                        d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                        d.metadata,
                        vector_distance_cos(c.embedding, {qvec_sql}) AS distance
                 FROM vector_top_k('chunks_vec_idx', {qvec_sql}, {fetch_k}) AS v
                 JOIN chunks c ON c.rowid = v.id
                 JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
                 WHERE c.store_id = '{escaped_store_id}'
                 {filter_clauses}
                 ORDER BY distance ASC
                 LIMIT {limit}"
            );

            let mut rows = conn.query(&sql, ()).await.map_err(map_libsql_err)?;

            let mut results = Vec::new();
            while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
                let chunk = row_to_chunk_record_strict(&row)?;
                let distance: f64 = row.get(19).map_err(map_libsql_err)?;
                let score = match encoding {
                    VectorEncoding::Float32 => vectors::cosine_distance_to_score(distance),
                    VectorEncoding::Binary => vectors::hamming_distance_to_score(distance, dim),
                };
                results.push(SearchResult { chunk, score });
            }

            if results.len() >= limit || !has_filters || fetch_k >= max_fetch {
                return Ok(results);
            }
            fetch_k = (fetch_k * 2).min(max_fetch);
        }
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        if query_text.trim().is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.db.conn().await;

        let escaped_query = escape_fts5_query(query_text);
        let filter_clauses = build_filter_clauses(filters);
        let escaped_store_id = self.store_id.replace('\'', "''");

        let sql = format!(
            "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                    c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                    d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                    d.metadata,
                    bm25(chunks_fts) AS score
             FROM chunks_fts f
             JOIN chunks c ON c.rowid = f.rowid
             JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
             WHERE chunks_fts MATCH ?
             AND c.store_id = '{escaped_store_id}'
             {filter_clauses}
             ORDER BY score ASC
             LIMIT {limit}"
        );

        let mut rows = conn
            .query(&sql, params![escaped_query])
            .await
            .map_err(map_libsql_err)?;

        let mut results = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            let chunk = row_to_chunk_record_strict(&row)?;
            let raw_score: f64 = row.get(19).map_err(map_libsql_err)?;
            let score = -raw_score as f32;
            results.push(SearchResult { chunk, score });
        }

        Ok(results)
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        let conn = self.db.conn().await;

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM chunks WHERE store_id = ?",
                params![self.store_id.clone()],
            )
            .await
            .map_err(map_libsql_err)?;
        let chunk_count = match rows.next().await.map_err(map_libsql_err)? {
            Some(row) => row.get::<u64>(0).map_err(map_libsql_err)?,
            None => 0,
        };

        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM documents WHERE store_id = ?",
                params![self.store_id.clone()],
            )
            .await
            .map_err(map_libsql_err)?;
        let document_count = match rows.next().await.map_err(map_libsql_err)? {
            Some(row) => row.get::<u64>(0).map_err(map_libsql_err)?,
            None => 0,
        };

        Ok(StoreStats {
            chunk_count,
            document_count,
        })
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        let conn = self.db.conn().await;
        let mut rows = conn
            .query(
                "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                        c.heading_path, vector_extract(c.embedding) AS embedding_json,
                        d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                        d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                        d.metadata
                 FROM chunks c
                 JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
                 WHERE c.store_id = ? AND c.id = ?",
                params![self.store_id.clone(), chunk_id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;
        match rows.next().await.map_err(map_libsql_err)? {
            Some(row) => Ok(Some(row_to_chunk_record_strict(&row)?)),
            None => Ok(None),
        }
    }

    async fn get_chunks_for_document(&self, document_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        let conn = self.db.conn().await;
        let mut rows = conn
            .query(
                "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                        c.heading_path, vector_extract(c.embedding) AS embedding_json,
                        d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                        d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                        d.metadata
                 FROM chunks c
                 JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
                 WHERE c.store_id = ? AND c.document_id = ?
                 ORDER BY c.seq",
                params![self.store_id.clone(), document_id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            out.push(row_to_chunk_record_strict(&row)?);
        }
        Ok(out)
    }

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        let conn = self.db.conn().await;
        let mut rows = conn
            .query(
                "SELECT id, uri, content_hash, policy_version
                 FROM documents WHERE store_id = ?",
                params![self.store_id.clone()],
            )
            .await
            .map_err(map_libsql_err)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            let document_id: String = row.get(0).map_err(map_libsql_err)?;
            let uri: String = row.get(1).map_err(map_libsql_err)?;
            let content_hash: String = row.get(2).map_err(map_libsql_err)?;
            let policy_version: String = row.get(3).map_err(map_libsql_err)?;
            out.push(DocumentRecord {
                document_id,
                uri,
                content_hash,
                policy_version,
            });
        }
        Ok(out)
    }
}

fn row_to_chunk_record_strict(row: &libsql::Row) -> Result<ChunkRecord, Error> {
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
