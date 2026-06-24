use libsql::Connection;
use localdb_core::VectorEncoding;

use crate::vectors::embedding_column_type;

const SCHEMA_VERSION: i64 = 1;

/// Run the full DDL for a new store database.
///
/// Creates: documents, chunks (with vector column), DiskANN index,
/// FTS5 external-content table, sync triggers, schema_version.
pub async fn create_schema(
    conn: &Connection,
    embedding_dim: usize,
    encoding: VectorEncoding,
) -> Result<(), libsql::Error> {
    // -- Documents table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS documents (
            rowid INTEGER PRIMARY KEY,
            id TEXT NOT NULL UNIQUE,
            store_id TEXT NOT NULL,
            source_id TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            uri TEXT NOT NULL,
            title TEXT,
            mime TEXT,
            content_hash TEXT NOT NULL,
            fetched_at TEXT NOT NULL,
            origin_store TEXT NOT NULL,
            policy_version TEXT NOT NULL,
            metadata TEXT DEFAULT '{}',
            share_path TEXT
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_documents_uri ON documents(uri)",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_documents_source_id ON documents(source_id)",
        (),
    )
    .await?;

    // -- Chunks table (embedding column type varies by encoding)
    let col_type = embedding_column_type(embedding_dim, encoding);
    let chunks_ddl = format!(
        "CREATE TABLE IF NOT EXISTS chunks (
            rowid INTEGER PRIMARY KEY,
            id TEXT NOT NULL UNIQUE,
            document_id TEXT NOT NULL REFERENCES documents(id),
            seq INTEGER NOT NULL,
            text TEXT NOT NULL,
            span_start INTEGER NOT NULL,
            span_end INTEGER NOT NULL,
            heading_path TEXT NOT NULL,
            embedding {col_type} NOT NULL
        )"
    );
    conn.execute(&chunks_ddl, ()).await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_chunks_document_id ON chunks(document_id)",
        (),
    )
    .await?;

    // -- DiskANN vector index
    conn.execute(
        "CREATE INDEX IF NOT EXISTS chunks_vec_idx ON chunks(libsql_vector_idx(embedding, 'metric=cosine'))",
        (),
    )
    .await?;

    // -- FTS5 external-content table
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
            text,
            content='chunks',
            content_rowid='rowid'
        )",
        (),
    )
    .await?;

    // -- FTS5 sync triggers
    conn.execute(
        "CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
            INSERT INTO chunks_fts(rowid, text) VALUES (new.rowid, new.text);
        END",
        (),
    )
    .await?;

    conn.execute(
        "CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
            INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.rowid, old.text);
        END",
        (),
    )
    .await?;

    conn.execute(
        "CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
            INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.rowid, old.text);
            INSERT INTO chunks_fts(rowid, text) VALUES (new.rowid, new.text);
        END",
        (),
    )
    .await?;

    // -- Schema version
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
        (),
    )
    .await?;

    conn.execute(
        &format!("INSERT OR IGNORE INTO schema_version VALUES ({SCHEMA_VERSION})"),
        (),
    )
    .await?;

    Ok(())
}

/// Check the current schema version. Returns None if table doesn't exist.
#[allow(dead_code)] // used in Wave 4 (migrations) and tests
pub async fn get_schema_version(conn: &Connection) -> Result<Option<i64>, libsql::Error> {
    let mut rows = match conn
        .query("SELECT version FROM schema_version LIMIT 1", ())
        .await
    {
        Ok(rows) => rows,
        Err(_) => return Ok(None),
    };

    match rows.next().await? {
        Some(row) => {
            let version: i64 = row.get(0)?;
            Ok(Some(version))
        }
        None => Ok(None),
    }
}
