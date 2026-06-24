mod schema;
mod vectors;

use async_trait::async_trait;
use libsql::{params, Builder, Connection, Database};
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::Mutex;

use localdb_core::ingestion::DocumentRecord;
use localdb_core::parser::DocumentMetadata;
use localdb_core::store::{
    ChunkRecord, MetadataFilter, RetrievalStore, SearchResult, StoreStats,
};
use localdb_core::types::Span;
use localdb_core::{Error, VectorEncoding};

/// A `RetrievalStore` backed by libSQL (Turso's SQLite fork).
///
/// Each store is a single `store.db` file with:
/// - `documents` table (one row per document)
/// - `chunks` table with DiskANN-indexed vector column
/// - FTS5 external-content table with sync triggers
pub struct LibsqlStore {
    #[allow(dead_code)] // used in Wave 4 (reconnect / close)
    db: Database,
    conn: Mutex<Connection>,
    #[allow(dead_code)] // used in Wave 4 (search methods)
    embedding_dim: usize,
    encoding: VectorEncoding,
}

impl LibsqlStore {
    /// Open (or create) a store database at the given path.
    ///
    /// Creates the schema if the database is new. Enables WAL mode.
    pub async fn open(
        path: &Path,
        embedding_dim: usize,
        encoding: VectorEncoding,
    ) -> Result<Self, Error> {
        let db = Builder::new_local(path)
            .build()
            .await
            .map_err(|e| Error::Internal {
                message: format!("Failed to open libsql database: {e}"),
                correlation_id: "libsql_open".to_string(),
            })?;

        let conn = db.connect().map_err(|e| Error::Internal {
            message: format!("Failed to connect: {e}"),
            correlation_id: "libsql_connect".to_string(),
        })?;

        // WAL mode — use query() not execute() because PRAGMA returns a row
        conn.query("PRAGMA journal_mode=WAL", ())
            .await
            .map_err(|e| Error::Internal {
                message: format!("WAL: {e}"),
                correlation_id: "libsql_wal".to_string(),
            })?;

        // Foreign keys
        conn.query("PRAGMA foreign_keys=ON", ())
            .await
            .map_err(|e| Error::Internal {
                message: format!("FK: {e}"),
                correlation_id: "libsql_fk".to_string(),
            })?;

        schema::create_schema(&conn, embedding_dim, encoding)
            .await
            .map_err(|e| Error::Internal {
                message: format!("Schema creation failed: {e}"),
                correlation_id: "libsql_schema".to_string(),
            })?;

        Ok(Self {
            db,
            conn: Mutex::new(conn),
            embedding_dim,
            encoding,
        })
    }
}

#[async_trait]
impl RetrievalStore for LibsqlStore {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        let conn = self.conn.lock().await;
        let count = records.len();

        conn.execute("BEGIN", ())
            .await
            .map_err(|e| Error::Internal {
                message: format!("upsert_chunks BEGIN: {e}"),
                correlation_id: "libsql_upsert_begin".to_string(),
            })?;

        let result = self.upsert_chunks_inner(&conn, &records).await;

        match result {
            Ok(()) => {
                conn.execute("COMMIT", ())
                    .await
                    .map_err(|e| Error::Internal {
                        message: format!("upsert_chunks COMMIT: {e}"),
                        correlation_id: "libsql_upsert_commit".to_string(),
                    })?;
                Ok(count)
            }
            Err(e) => {
                conn.execute("ROLLBACK", ()).await.ok();
                Err(e)
            }
        }
    }

    async fn delete_by_document(&self, document_id: &str) -> Result<usize, Error> {
        let conn = self.conn.lock().await;

        let chunk_count = conn
            .execute(
                "DELETE FROM chunks WHERE document_id = ?",
                params![document_id],
            )
            .await
            .map_err(|e| Error::Internal {
                message: format!("delete_by_document chunks: {e}"),
                correlation_id: "libsql_delete_doc_chunks".to_string(),
            })?;

        conn.execute("DELETE FROM documents WHERE id = ?", params![document_id])
            .await
            .map_err(|e| Error::Internal {
                message: format!("delete_by_document documents: {e}"),
                correlation_id: "libsql_delete_doc".to_string(),
            })?;

        Ok(chunk_count as usize)
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        let conn = self.conn.lock().await;

        let chunk_count = conn
            .execute(
                "DELETE FROM chunks WHERE document_id IN (SELECT id FROM documents WHERE store_id = ?)",
                params![store_id],
            )
            .await
            .map_err(|e| Error::Internal {
                message: format!("delete_by_store chunks: {e}"),
                correlation_id: "libsql_delete_store_chunks".to_string(),
            })?;

        conn.execute(
            "DELETE FROM documents WHERE store_id = ?",
            params![store_id],
        )
        .await
        .map_err(|e| Error::Internal {
            message: format!("delete_by_store documents: {e}"),
            correlation_id: "libsql_delete_store_docs".to_string(),
        })?;

        Ok(chunk_count as usize)
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let conn = self.conn.lock().await;

        let qvec_sql = crate::vectors::query_vector_sql(query_vector, self.encoding);
        let fetch_k = if filters.is_empty() {
            limit
        } else {
            limit * 3
        };
        let filter_clauses = build_filter_clauses(filters);
        let where_sql = if filter_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE 1=1 {filter_clauses}")
        };

        let sql = format!(
            "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                    c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                    d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                    d.metadata,
                    vector_distance_cos(c.embedding, {qvec_sql}) AS distance
             FROM vector_top_k('chunks_vec_idx', {qvec_sql}, {fetch_k}) AS v
             JOIN chunks c ON c.rowid = v.id
             JOIN documents d ON d.id = c.document_id
             {where_sql}
             ORDER BY distance ASC
             LIMIT {limit}"
        );

        let mut rows = conn
            .query(&sql, ())
            .await
            .map_err(|e| Error::Internal {
                message: format!("dense_search query: {e}"),
                correlation_id: "libsql_dense_search".to_string(),
            })?;

        let encoding = self.encoding;
        let dim = self.embedding_dim;
        let mut results = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| Error::Internal {
            message: format!("dense_search next: {e}"),
            correlation_id: "libsql_dense_next".to_string(),
        })? {
            let chunk = row_to_chunk_record(&row)?;
            let distance: f64 = row.get(19).map_err(|e| Error::Internal {
                message: format!("dense_search distance: {e}"),
                correlation_id: "libsql_dense_dist".to_string(),
            })?;
            let score = match encoding {
                VectorEncoding::Float32 => {
                    crate::vectors::cosine_distance_to_score(distance)
                }
                VectorEncoding::Binary => {
                    crate::vectors::hamming_distance_to_score(distance, dim)
                }
            };
            results.push(SearchResult { chunk, score });
        }

        Ok(results)
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let conn = self.conn.lock().await;

        let filter_clauses = build_filter_clauses(filters);

        let sql = format!(
            "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                    c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                    d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                    d.metadata,
                    bm25(chunks_fts) AS score
             FROM chunks_fts f
             JOIN chunks c ON c.rowid = f.rowid
             JOIN documents d ON d.id = c.document_id
             WHERE chunks_fts MATCH ?
             {filter_clauses}
             ORDER BY score ASC
             LIMIT {limit}"
        );

        let mut rows = conn
            .query(&sql, params![query_text])
            .await
            .map_err(|e| Error::Internal {
                message: format!("bm25_search query: {e}"),
                correlation_id: "libsql_bm25_search".to_string(),
            })?;

        let mut results = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| Error::Internal {
            message: format!("bm25_search next: {e}"),
            correlation_id: "libsql_bm25_next".to_string(),
        })? {
            let chunk = row_to_chunk_record(&row)?;
            let raw_score: f64 = row.get(19).map_err(|e| Error::Internal {
                message: format!("bm25_search score: {e}"),
                correlation_id: "libsql_bm25_score".to_string(),
            })?;
            // FTS5 bm25() returns negative scores (more negative = better).
            // Negate to make positive (higher = better).
            let score = -raw_score as f32;
            results.push(SearchResult { chunk, score });
        }

        Ok(results)
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        let conn = self.conn.lock().await;

        let mut rows = conn
            .query("SELECT COUNT(*) FROM chunks", ())
            .await
            .map_err(|e| Error::Internal {
                message: format!("stats chunks: {e}"),
                correlation_id: "libsql_stats_chunks".to_string(),
            })?;
        let chunk_count = match rows.next().await.map_err(|e| Error::Internal {
            message: format!("stats chunks next: {e}"),
            correlation_id: "libsql_stats_chunks_next".to_string(),
        })? {
            Some(row) => row.get::<u64>(0).map_err(|e| Error::Internal {
                message: format!("stats chunks get: {e}"),
                correlation_id: "libsql_stats_chunks_get".to_string(),
            })?,
            None => 0,
        };

        let mut rows = conn
            .query("SELECT COUNT(*) FROM documents", ())
            .await
            .map_err(|e| Error::Internal {
                message: format!("stats documents: {e}"),
                correlation_id: "libsql_stats_docs".to_string(),
            })?;
        let document_count = match rows.next().await.map_err(|e| Error::Internal {
            message: format!("stats documents next: {e}"),
            correlation_id: "libsql_stats_docs_next".to_string(),
        })? {
            Some(row) => row.get::<u64>(0).map_err(|e| Error::Internal {
                message: format!("stats documents get: {e}"),
                correlation_id: "libsql_stats_docs_get".to_string(),
            })?,
            None => 0,
        };

        Ok(StoreStats {
            chunk_count,
            document_count,
        })
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        let conn = self.conn.lock().await;

        let mut rows = conn
            .query(
                "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                        c.heading_path, vector_extract(c.embedding) AS embedding_json,
                        d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                        d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                        d.metadata
                 FROM chunks c
                 JOIN documents d ON d.id = c.document_id
                 WHERE c.id = ?",
                params![chunk_id],
            )
            .await
            .map_err(|e| Error::Internal {
                message: format!("get_chunk query: {e}"),
                correlation_id: "libsql_get_chunk".to_string(),
            })?;

        match rows.next().await.map_err(|e| Error::Internal {
            message: format!("get_chunk next: {e}"),
            correlation_id: "libsql_get_chunk_next".to_string(),
        })? {
            Some(row) => Ok(Some(row_to_chunk_record(&row)?)),
            None => Ok(None),
        }
    }

    async fn get_chunks_for_document(
        &self,
        document_id: &str,
    ) -> Result<Vec<ChunkRecord>, Error> {
        let conn = self.conn.lock().await;

        let mut rows = conn
            .query(
                "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                        c.heading_path, vector_extract(c.embedding) AS embedding_json,
                        d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                        d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                        d.metadata
                 FROM chunks c
                 JOIN documents d ON d.id = c.document_id
                 WHERE c.document_id = ?
                 ORDER BY c.seq",
                params![document_id],
            )
            .await
            .map_err(|e| Error::Internal {
                message: format!("get_chunks_for_document query: {e}"),
                correlation_id: "libsql_get_doc_chunks".to_string(),
            })?;

        let mut records = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| Error::Internal {
            message: format!("get_chunks_for_document next: {e}"),
            correlation_id: "libsql_get_doc_chunks_next".to_string(),
        })? {
            records.push(row_to_chunk_record(&row)?);
        }

        Ok(records)
    }

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        let conn = self.conn.lock().await;

        let mut rows = conn
            .query(
                "SELECT id, uri, content_hash, policy_version FROM documents",
                (),
            )
            .await
            .map_err(|e| Error::Internal {
                message: format!("list_indexed_documents query: {e}"),
                correlation_id: "libsql_list_docs".to_string(),
            })?;

        let mut records = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| Error::Internal {
            message: format!("list_indexed_documents next: {e}"),
            correlation_id: "libsql_list_docs_next".to_string(),
        })? {
            let document_id: String = row.get(0).map_err(|e| Error::Internal {
                message: format!("list_indexed_documents id: {e}"),
                correlation_id: "libsql_list_docs_id".to_string(),
            })?;
            let uri: String = row.get(1).map_err(|e| Error::Internal {
                message: format!("list_indexed_documents uri: {e}"),
                correlation_id: "libsql_list_docs_uri".to_string(),
            })?;
            let content_hash: String = row.get(2).map_err(|e| Error::Internal {
                message: format!("list_indexed_documents hash: {e}"),
                correlation_id: "libsql_list_docs_hash".to_string(),
            })?;
            let policy_version: String = row.get(3).map_err(|e| Error::Internal {
                message: format!("list_indexed_documents policy: {e}"),
                correlation_id: "libsql_list_docs_policy".to_string(),
            })?;
            records.push(DocumentRecord {
                document_id,
                uri,
                content_hash,
                policy_version,
            });
        }

        Ok(records)
    }
}

impl LibsqlStore {
    /// Inner helper for upsert_chunks that runs inside a transaction.
    /// Separated to make rollback-on-error clean.
    async fn upsert_chunks_inner(
        &self,
        conn: &Connection,
        records: &[ChunkRecord],
    ) -> Result<(), Error> {
        // Group records by document_id to track per-document seq counters
        // and upsert each document once.
        let mut seen_documents: HashMap<String, bool> = HashMap::new();
        let mut doc_seq_counters: HashMap<String, i64> = HashMap::new();

        for record in records {
            // Upsert the document if we haven't already in this batch
            if !seen_documents.contains_key(record.document_id.as_str()) {
                let metadata_json =
                    serde_json::to_string(&record.metadata).map_err(|e| Error::Internal {
                        message: format!("upsert_chunks metadata serialize: {e}"),
                        correlation_id: "libsql_upsert_meta".to_string(),
                    })?;

                let title = record.metadata.title.as_deref();

                conn.execute(
                    "INSERT OR REPLACE INTO documents (id, store_id, source_id, source_kind, uri, title, mime,
                        content_hash, fetched_at, origin_store, policy_version, metadata)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        record.document_id.as_str(),
                        record.store_id.as_str(),
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
                .map_err(|e| Error::Internal {
                    message: format!("upsert_chunks document INSERT: {e}"),
                    correlation_id: "libsql_upsert_doc".to_string(),
                })?;

                seen_documents.insert(record.document_id.clone(), true);
            }

            // Compute seq as the per-document chunk counter
            let seq = doc_seq_counters
                .entry(record.document_id.clone())
                .or_insert(0);
            let current_seq = *seq;
            *seq += 1;

            // Build the embedding SQL literal
            let vector_sql = match self.encoding {
                VectorEncoding::Float32 => {
                    crate::vectors::f32_to_vector32_sql(&record.embedding)
                }
                VectorEncoding::Binary => {
                    crate::vectors::f32_to_vector1bit_sql(&record.embedding)
                }
            };

            let heading_path_json =
                serde_json::to_string(&record.heading_path).map_err(|e| Error::Internal {
                    message: format!("upsert_chunks heading_path serialize: {e}"),
                    correlation_id: "libsql_upsert_heading".to_string(),
                })?;

            // The vector literal must be inlined in the SQL string because
            // vector32()/vector1bit() are SQL functions that need the literal.
            let sql = format!(
                "INSERT OR REPLACE INTO chunks (id, document_id, seq, text, span_start, span_end, heading_path, embedding)
                VALUES (?, ?, ?, ?, ?, ?, ?, {vector_sql})"
            );

            conn.execute(
                &sql,
                params![
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
            .map_err(|e| Error::Internal {
                message: format!("upsert_chunks chunk INSERT: {e}"),
                correlation_id: "libsql_upsert_chunk".to_string(),
            })?;
        }

        Ok(())
    }
}

/// Build SQL filter clauses from `MetadataFilter` variants.
///
/// Returns a string of `AND ...` clauses suitable for appending after a `WHERE`
/// or `WHERE 1=1`. Empty string if no filters.
fn build_filter_clauses(filters: &[MetadataFilter]) -> String {
    let mut clauses = String::new();
    for filter in filters {
        match filter {
            MetadataFilter::Mime(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND d.mime = '{escaped}'"));
            }
            MetadataFilter::UriPrefix(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND d.uri LIKE '{escaped}%'"));
            }
            MetadataFilter::FetchedAfter(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND d.fetched_at >= '{escaped}'"));
            }
            MetadataFilter::FetchedBefore(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND d.fetched_at <= '{escaped}'"));
            }
            MetadataFilter::SourceId(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND d.source_id = '{escaped}'"));
            }
            MetadataFilter::DocumentId(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND c.document_id = '{escaped}'"));
            }
            MetadataFilter::PolicyVersion(v) => {
                let escaped = v.replace('\'', "''");
                clauses.push_str(&format!(" AND d.policy_version = '{escaped}'"));
            }
        }
    }
    clauses
}

/// Extract a `ChunkRecord` from a row with columns in the standard SELECT order:
///
/// 0: c.id, 1: c.document_id, 2: c.seq, 3: c.text, 4: c.span_start, 5: c.span_end,
/// 6: c.heading_path, 7: vector_extract(c.embedding) AS embedding_json,
/// 8: d.store_id, 9: d.source_id, 10: d.source_kind, 11: d.uri, 12: d.title, 13: d.mime,
/// 14: d.policy_version, 15: d.fetched_at, 16: d.content_hash, 17: d.origin_store,
/// 18: d.metadata
fn row_to_chunk_record(row: &libsql::Row) -> Result<ChunkRecord, Error> {
    let id: String = row.get(0).map_err(|e| Error::Internal {
        message: format!("row_to_chunk id: {e}"),
        correlation_id: "libsql_row_id".to_string(),
    })?;
    let document_id: String = row.get(1).map_err(|e| Error::Internal {
        message: format!("row_to_chunk document_id: {e}"),
        correlation_id: "libsql_row_doc_id".to_string(),
    })?;
    let _seq: i64 = row.get(2).map_err(|e| Error::Internal {
        message: format!("row_to_chunk seq: {e}"),
        correlation_id: "libsql_row_seq".to_string(),
    })?;
    let text: String = row.get(3).map_err(|e| Error::Internal {
        message: format!("row_to_chunk text: {e}"),
        correlation_id: "libsql_row_text".to_string(),
    })?;
    let span_start: i64 = row.get(4).map_err(|e| Error::Internal {
        message: format!("row_to_chunk span_start: {e}"),
        correlation_id: "libsql_row_span_start".to_string(),
    })?;
    let span_end: i64 = row.get(5).map_err(|e| Error::Internal {
        message: format!("row_to_chunk span_end: {e}"),
        correlation_id: "libsql_row_span_end".to_string(),
    })?;
    let heading_path_str: String = row.get(6).map_err(|e| Error::Internal {
        message: format!("row_to_chunk heading_path: {e}"),
        correlation_id: "libsql_row_heading".to_string(),
    })?;
    let embedding_str: String = row.get(7).map_err(|e| Error::Internal {
        message: format!("row_to_chunk embedding: {e}"),
        correlation_id: "libsql_row_embedding".to_string(),
    })?;
    let store_id: String = row.get(8).map_err(|e| Error::Internal {
        message: format!("row_to_chunk store_id: {e}"),
        correlation_id: "libsql_row_store_id".to_string(),
    })?;
    let source_id: String = row.get(9).map_err(|e| Error::Internal {
        message: format!("row_to_chunk source_id: {e}"),
        correlation_id: "libsql_row_source_id".to_string(),
    })?;
    let source_kind: String = row.get(10).map_err(|e| Error::Internal {
        message: format!("row_to_chunk source_kind: {e}"),
        correlation_id: "libsql_row_source_kind".to_string(),
    })?;
    let uri: String = row.get(11).map_err(|e| Error::Internal {
        message: format!("row_to_chunk uri: {e}"),
        correlation_id: "libsql_row_uri".to_string(),
    })?;
    let title: Option<String> = row.get(12).map_err(|e| Error::Internal {
        message: format!("row_to_chunk title: {e}"),
        correlation_id: "libsql_row_title".to_string(),
    })?;
    let mime: Option<String> = row.get(13).map_err(|e| Error::Internal {
        message: format!("row_to_chunk mime: {e}"),
        correlation_id: "libsql_row_mime".to_string(),
    })?;
    let policy_version: String = row.get(14).map_err(|e| Error::Internal {
        message: format!("row_to_chunk policy_version: {e}"),
        correlation_id: "libsql_row_policy".to_string(),
    })?;
    let fetched_at: String = row.get(15).map_err(|e| Error::Internal {
        message: format!("row_to_chunk fetched_at: {e}"),
        correlation_id: "libsql_row_fetched".to_string(),
    })?;
    let content_hash: String = row.get(16).map_err(|e| Error::Internal {
        message: format!("row_to_chunk content_hash: {e}"),
        correlation_id: "libsql_row_hash".to_string(),
    })?;
    let origin_store: String = row.get(17).map_err(|e| Error::Internal {
        message: format!("row_to_chunk origin_store: {e}"),
        correlation_id: "libsql_row_origin".to_string(),
    })?;
    let metadata_str: String = row.get(18).map_err(|e| Error::Internal {
        message: format!("row_to_chunk metadata: {e}"),
        correlation_id: "libsql_row_metadata".to_string(),
    })?;

    let heading_path: Vec<String> =
        serde_json::from_str(&heading_path_str).unwrap_or_default();
    let embedding: Vec<f32> = serde_json::from_str(&embedding_str).unwrap_or_default();
    let mut metadata: DocumentMetadata =
        serde_json::from_str(&metadata_str).unwrap_or_default();

    // Fill in title from the documents table if metadata.title is not set
    if metadata.title.is_none() {
        metadata.title = title;
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_open_creates_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.db");
        let store = LibsqlStore::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        // Verify open succeeded and schema was created by checking the version
        let conn = store.conn.lock().await;
        let version = schema::get_schema_version(&conn).await.unwrap();
        assert_eq!(version, Some(1));
        drop(conn);
        drop(store);
    }
}
