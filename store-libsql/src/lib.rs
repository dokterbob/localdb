mod schema;
mod vectors;

use async_trait::async_trait;
use libsql::{params, Builder, Connection, Database};
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::Mutex;

use localdb_core::ingestion::DocumentRecord;
use localdb_core::store::{
    ChunkRecord, MetadataFilter, RetrievalStore, SearchResult, StoreStats,
};
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
        _query_vector: &[f32],
        _limit: usize,
        _filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        todo!("vector_top_k + vector_distance_cos + metadata filters")
    }

    async fn bm25_search(
        &self,
        _query_text: &str,
        _limit: usize,
        _filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        todo!("FTS5 MATCH + bm25() + metadata filters")
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        todo!("SELECT COUNT(*) from chunks + documents")
    }

    async fn get_chunk(&self, _chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        todo!("SELECT ... FROM chunks JOIN documents WHERE chunks.id = ?")
    }

    async fn get_chunks_for_document(
        &self,
        _document_id: &str,
    ) -> Result<Vec<ChunkRecord>, Error> {
        todo!("SELECT ... FROM chunks JOIN documents WHERE document_id = ? ORDER BY seq")
    }

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        todo!("SELECT id, uri, content_hash, policy_version FROM documents")
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
