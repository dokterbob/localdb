//! DDL for the unified single-file SQLite store.
//!
//! Replaces both the per-store `schema.rs` and the runtime-state DB schema in
//! `core/src/config/runtime_state.rs`. Holds, in one file:
//!
//! - `stores` / `sources` — runtime registry (was `runtime-state.db`)
//! - `documents` / `chunks` / `chunks_fts` — corpus (was per-store `store.db`)
//!
//! Foreign keys cascade `stores → sources`, `stores → documents → chunks`,
//! and `chunks_fts` stays in sync via INSERT/DELETE/UPDATE triggers.
//!
//! Composite `(store_id, id)` uniqueness on `documents` and `chunks` preserves
//! "each store has its own row" semantics for content-addressed IDs that collide
//! across stores by design.
//!
//! See `docs/proposals/single-sql-store.md` §2 for design rationale.

use libsql::Connection;
use localdb_core::VectorEncoding;

use crate::vectors::embedding_column_type;

/// Schema version stored in `PRAGMA user_version`.
///
/// Survives `VACUUM` and doesn't require a separate table. Replaces the
/// per-store `schema_version` table from the legacy schema.
pub const UNIFIED_SCHEMA_VERSION: i64 = 1;

/// Run the full DDL for the unified database.
///
/// Idempotent: safe to call on an already-created database. Does NOT set
/// connection-level PRAGMAs (`journal_mode`, `foreign_keys`, `busy_timeout`)
/// — that is the caller's responsibility (see `db::LibsqlDb::open`).
pub async fn create_unified_schema(
    conn: &Connection,
    embedding_dim: usize,
    encoding: VectorEncoding,
) -> Result<(), libsql::Error> {
    create_stores(conn).await?;
    create_sources(conn).await?;
    create_documents(conn).await?;
    create_chunks(conn, embedding_dim, encoding).await?;
    create_fts(conn).await?;
    create_triggers(conn).await?;
    set_user_version(conn).await?;
    Ok(())
}

async fn create_stores(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS stores (
            id              TEXT PRIMARY KEY NOT NULL,
            name            TEXT NOT NULL UNIQUE,
            visibility      TEXT NOT NULL DEFAULT 'private',
            backend         TEXT NOT NULL DEFAULT 'libsql',
            indexing_policy TEXT NOT NULL,
            policy_version  TEXT NOT NULL,
            acl             TEXT NOT NULL DEFAULT '{}',
            created_at      TEXT NOT NULL
        )",
        (),
    )
    .await?;
    Ok(())
}

async fn create_sources(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sources (
            id         TEXT PRIMARY KEY NOT NULL,
            store_id   TEXT NOT NULL REFERENCES stores(id) ON DELETE CASCADE,
            kind       TEXT NOT NULL,
            root       TEXT,
            url        TEXT,
            include    TEXT NOT NULL DEFAULT '[]',
            exclude    TEXT NOT NULL DEFAULT '[]',
            preset     TEXT NOT NULL DEFAULT 'prose',
            refresh    TEXT,
            created_at TEXT NOT NULL,
            CHECK ((kind = 'path' AND root IS NOT NULL AND url IS NULL)
                OR (kind = 'url'  AND url  IS NOT NULL AND root IS NULL))
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sources_store_id ON sources(store_id)",
        (),
    )
    .await?;

    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_sources_store_root \
         ON sources(store_id, root) WHERE root IS NOT NULL",
        (),
    )
    .await?;

    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_sources_store_url \
         ON sources(store_id, url) WHERE url IS NOT NULL",
        (),
    )
    .await?;

    Ok(())
}

async fn create_documents(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS documents (
            rowid          INTEGER PRIMARY KEY,
            store_id       TEXT NOT NULL REFERENCES stores(id)  ON DELETE CASCADE,
            id             TEXT NOT NULL,
            source_id      TEXT NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
            source_kind    TEXT NOT NULL,
            uri            TEXT NOT NULL,
            title          TEXT,
            mime           TEXT,
            content_hash   TEXT NOT NULL,
            fetched_at     TEXT NOT NULL,
            origin_store   TEXT NOT NULL,
            policy_version TEXT NOT NULL,
            metadata       TEXT NOT NULL,
            share_path     TEXT,
            UNIQUE (store_id, id)
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_documents_store_uri ON documents(store_id, uri)",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_documents_source_id ON documents(source_id)",
        (),
    )
    .await?;

    Ok(())
}

async fn create_chunks(
    conn: &Connection,
    embedding_dim: usize,
    encoding: VectorEncoding,
) -> Result<(), libsql::Error> {
    let col_type = embedding_column_type(embedding_dim, encoding);
    let chunks_ddl = format!(
        "CREATE TABLE IF NOT EXISTS chunks (
            rowid        INTEGER PRIMARY KEY,
            store_id     TEXT NOT NULL,
            id           TEXT NOT NULL,
            document_id  TEXT NOT NULL,
            seq          INTEGER NOT NULL,
            text         TEXT NOT NULL,
            span_start   INTEGER NOT NULL,
            span_end     INTEGER NOT NULL,
            heading_path TEXT NOT NULL,
            embedding    {col_type} NOT NULL,
            UNIQUE (store_id, id),
            FOREIGN KEY (store_id, document_id)
                REFERENCES documents(store_id, id) ON DELETE CASCADE
        )"
    );
    conn.execute(&chunks_ddl, ()).await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_chunks_store_doc ON chunks(store_id, document_id)",
        (),
    )
    .await?;

    // DiskANN index. Tuning (max_neighbors=64, compress_neighbors=float8)
    // matches PR #92 review feedback that landed on main.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS chunks_vec_idx ON chunks(\
         libsql_vector_idx(embedding, 'metric=cosine', 'max_neighbors=64', 'compress_neighbors=float8'))",
        (),
    )
    .await?;

    Ok(())
}

async fn create_fts(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
            text,
            content='chunks',
            content_rowid='rowid'
        )",
        (),
    )
    .await?;
    Ok(())
}

async fn create_triggers(conn: &Connection) -> Result<(), libsql::Error> {
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

    Ok(())
}

async fn set_user_version(conn: &Connection) -> Result<(), libsql::Error> {
    // `PRAGMA user_version = N` is idempotent. Use query() not execute()
    // because PRAGMAs may return rows.
    conn.query(
        &format!("PRAGMA user_version = {UNIFIED_SCHEMA_VERSION}"),
        (),
    )
    .await?;
    Ok(())
}

/// Read the schema version from `PRAGMA user_version`.
///
/// Returns `0` on a freshly-created (un-touched) database. Returns the value
/// last set by `set_user_version` (or any other writer) on an initialized one.
#[cfg(test)]
async fn get_unified_schema_version(conn: &Connection) -> Result<i64, libsql::Error> {
    let mut rows = conn.query("PRAGMA user_version", ()).await?;
    match rows.next().await? {
        Some(row) => row.get::<i64>(0),
        None => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsql::Builder;
    use std::collections::HashSet;
    use tempfile::tempdir;

    async fn open_test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        // PRAGMA foreign_keys must be ON for tests that exercise FK cascade.
        conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
        (dir, conn)
    }

    async fn table_names(conn: &Connection) -> HashSet<String> {
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type IN ('table','view') ORDER BY name",
                (),
            )
            .await
            .unwrap();
        let mut names = HashSet::new();
        while let Some(row) = rows.next().await.unwrap() {
            names.insert(row.get::<String>(0).unwrap());
        }
        names
    }

    async fn index_names(conn: &Connection) -> HashSet<String> {
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL ORDER BY name",
                (),
            )
            .await
            .unwrap();
        let mut names = HashSet::new();
        while let Some(row) = rows.next().await.unwrap() {
            names.insert(row.get::<String>(0).unwrap());
        }
        names
    }

    async fn trigger_names(conn: &Connection) -> HashSet<String> {
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name",
                (),
            )
            .await
            .unwrap();
        let mut names = HashSet::new();
        while let Some(row) = rows.next().await.unwrap() {
            names.insert(row.get::<String>(0).unwrap());
        }
        names
    }

    #[tokio::test]
    async fn create_schema_succeeds_on_empty_db() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_schema_is_idempotent() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        // Calling twice must not error.
        create_unified_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn all_expected_tables_exist() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let names = table_names(&conn).await;
        for expected in ["stores", "sources", "documents", "chunks", "chunks_fts"] {
            assert!(
                names.contains(expected),
                "expected table '{expected}' missing; have: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn all_expected_indexes_exist() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let names = index_names(&conn).await;
        for expected in [
            "idx_sources_store_id",
            "idx_sources_store_root",
            "idx_sources_store_url",
            "idx_documents_store_uri",
            "idx_documents_source_id",
            "idx_chunks_store_doc",
            "chunks_vec_idx",
        ] {
            assert!(
                names.contains(expected),
                "expected index '{expected}' missing; have: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn all_expected_triggers_exist() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let names = trigger_names(&conn).await;
        for expected in ["chunks_ai", "chunks_ad", "chunks_au"] {
            assert!(
                names.contains(expected),
                "expected trigger '{expected}' missing; have: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn user_version_set_to_schema_version() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let v = get_unified_schema_version(&conn).await.unwrap();
        assert_eq!(v, UNIFIED_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn fresh_db_reports_user_version_zero() {
        let (_dir, conn) = open_test_db().await;
        let v = get_unified_schema_version(&conn).await.unwrap();
        assert_eq!(v, 0, "fresh DB should have user_version=0");
    }

    #[tokio::test]
    async fn binary_encoding_uses_f1bit_blob_column() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 1024, VectorEncoding::Binary)
            .await
            .unwrap();
        let mut rows = conn
            .query(
                "SELECT type FROM pragma_table_info('chunks') WHERE name = 'embedding'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let col_type: String = row.get(0).unwrap();
        assert_eq!(col_type.to_ascii_uppercase(), "F1BIT_BLOB(1024)");
    }

    #[tokio::test]
    async fn float32_encoding_uses_f32_blob_column() {
        let (_dir, conn) = open_test_db().await;
        create_unified_schema(&conn, 384, VectorEncoding::Float32)
            .await
            .unwrap();
        let mut rows = conn
            .query(
                "SELECT type FROM pragma_table_info('chunks') WHERE name = 'embedding'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let col_type: String = row.get(0).unwrap();
        assert_eq!(col_type.to_ascii_uppercase(), "F32_BLOB(384)");
    }
}
