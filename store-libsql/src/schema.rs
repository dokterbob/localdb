use libsql::Connection;
use localdb_core::VectorEncoding;

use crate::vectors::embedding_column_type;

/// Schema version stored in `PRAGMA user_version`.
///
/// Survives `VACUUM` and doesn't require a separate table. Replaces the
/// per-store `schema_version` table from the legacy schema.
pub const SCHEMA_VERSION: i64 = 2;

/// Run the full DDL for the unified database.
///
/// Idempotent: safe to call on an already-created database. Does NOT set
/// connection-level PRAGMAs (`journal_mode`, `foreign_keys`, `busy_timeout`)
/// — that is the caller's responsibility (see `db::LibsqlDb::open`).
pub async fn create_schema(
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
                OR (kind = 'url'  AND url  IS NOT NULL AND root IS NULL)),
            UNIQUE (store_id, id)
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
            store_id       TEXT NOT NULL REFERENCES stores(id) ON DELETE CASCADE,
            id             TEXT NOT NULL,
            source_id      TEXT NOT NULL,
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
            UNIQUE (store_id, id),
            FOREIGN KEY (store_id, source_id) REFERENCES sources(store_id, id) ON DELETE CASCADE
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
    conn.query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"), ())
        .await?;
    Ok(())
}

/// Read the schema version from `PRAGMA user_version`.
///
/// Returns `0` on a freshly-created (un-touched) database. Returns the value
/// last set by `set_user_version` (or any other writer) on an initialized one.
pub(crate) async fn get_schema_version(conn: &Connection) -> Result<i64, libsql::Error> {
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
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_schema_is_idempotent() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        // Calling twice must not error.
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn all_expected_tables_exist() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 4, VectorEncoding::Float32)
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
        create_schema(&conn, 4, VectorEncoding::Float32)
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
        create_schema(&conn, 4, VectorEncoding::Float32)
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
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        let v = get_schema_version(&conn).await.unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn fresh_db_reports_user_version_zero() {
        let (_dir, conn) = open_test_db().await;
        let v = get_schema_version(&conn).await.unwrap();
        assert_eq!(v, 0, "fresh DB should have user_version=0");
    }

    #[tokio::test]
    async fn binary_encoding_uses_f1bit_blob_column() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 1024, VectorEncoding::Binary)
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
        create_schema(&conn, 384, VectorEncoding::Float32)
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

    /// Insert fixtures shared by the store-isolation FK tests.
    ///
    /// Creates store-a and store-b, one source per store, and one document in
    /// store-a that references store-a's source.  Returns early before the
    /// document insert so callers can attempt their own insert and assert the
    /// outcome.
    async fn insert_two_stores_and_sources(conn: &Connection) {
        for (id, name) in [("store-a", "Store A"), ("store-b", "Store B")] {
            conn.execute(
                &format!(
                    "INSERT INTO stores \
                     (id, name, indexing_policy, policy_version, created_at) \
                     VALUES ('{id}', '{name}', '{{}}', '1', '2024-01-01T00:00:00Z')"
                ),
                (),
            )
            .await
            .unwrap();
        }
        for (id, store_id, root) in [
            ("src-a", "store-a", "/path/a"),
            ("src-b", "store-b", "/path/b"),
        ] {
            conn.execute(
                &format!(
                    "INSERT INTO sources (id, store_id, kind, root, created_at) \
                     VALUES ('{id}', '{store_id}', 'path', '{root}', '2024-01-01T00:00:00Z')"
                ),
                (),
            )
            .await
            .unwrap();
        }
    }

    /// A document in store A must not be able to reference a source in store B.
    ///
    /// This guards against the cross-store contamination bug: with only a
    /// simple `REFERENCES sources(id)` FK a document in store A could point to
    /// a source in store B, and a cascade-delete of store B would then silently
    /// remove store A's documents.  The composite FK
    /// `FOREIGN KEY (store_id, source_id) REFERENCES sources(store_id, id)`
    /// closes that gap.
    #[tokio::test]
    async fn cross_store_source_reference_is_rejected() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();

        insert_two_stores_and_sources(&conn).await;

        // Attempt: document lives in store-a but references src-b (store-b).
        let result = conn
            .execute(
                "INSERT INTO documents \
                 (store_id, id, source_id, source_kind, uri, \
                  content_hash, fetched_at, origin_store, policy_version, metadata) \
                 VALUES \
                 ('store-a', 'doc-x', 'src-b', 'path', 'file:///doc.md', \
                  'abc', '2024-01-01T00:00:00Z', 'store-a', '1', '{}')",
                (),
            )
            .await;

        assert!(
            result.is_err(),
            "inserting a document in store-a that references a source in store-b \
             should be rejected by the composite FK constraint"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("FOREIGN KEY"),
            "expected a FOREIGN KEY constraint error, got: {err_msg}"
        );
    }

    /// Deleting store B must not cascade-delete documents that belong to store A.
    ///
    /// With the old simple `REFERENCES sources(id)` FK, deleting store B would
    /// cascade-delete its sources, which in turn (if a document in store A
    /// somehow referenced a store-B source) would cascade-delete store A's
    /// documents.  The composite FK makes cross-store references impossible
    /// in the first place, and cascade deletions remain store-scoped.
    #[tokio::test]
    async fn deleting_store_b_does_not_cascade_to_store_a_documents() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();

        insert_two_stores_and_sources(&conn).await;

        // Insert a document in store-a that references store-a's own source.
        conn.execute(
            "INSERT INTO documents \
             (store_id, id, source_id, source_kind, uri, \
              content_hash, fetched_at, origin_store, policy_version, metadata) \
             VALUES \
             ('store-a', 'doc-1', 'src-a', 'path', 'file:///doc.md', \
              'abc', '2024-01-01T00:00:00Z', 'store-a', '1', '{}')",
            (),
        )
        .await
        .unwrap();

        // Delete store B — should cascade only to store B's own rows.
        conn.execute("DELETE FROM stores WHERE id = 'store-b'", ())
            .await
            .unwrap();

        // Store A's document must still be present.
        let mut rows = conn
            .query("SELECT id FROM documents WHERE store_id = 'store-a'", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap();
        assert!(
            row.is_some(),
            "store A's document should still exist after deleting store B"
        );
    }
}
