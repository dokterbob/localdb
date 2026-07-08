//! FROZEN v4 baseline schema. Never edit.
//!
//! This is a byte-for-byte copy of the DDL `schema.rs` produced at schema
//! version 4, the version the migration chain (see `chain::BASELINE_VERSION`)
//! starts counting from. It exists so consumer branches can build "old DB"
//! test fixtures — create a database via `create_baseline_schema`, then
//! apply the migration chain on top, exactly like a real pre-migrations
//! database would be upgraded.
//!
//! A drift-guard test at the bottom of this file asserts that the
//! normalized `sqlite_master` produced here is identical to what
//! `schema::create_schema` produces today. That equality only holds because
//! this file is frozen: `schema.rs`'s `create_*` functions will keep
//! evolving as new migrations are added to the chain, but this file must
//! not change to track them. **New schema changes belong in chain entries
//! (plus, if the "current schema" helper is kept up to date elsewhere, in
//! `schema::create_schema`) — never here.**

use libsql::Connection;

use super::MigrationContext;

/// Run the full frozen v4 DDL against `conn`.
///
/// Mirrors `schema::create_schema` as it existed at schema version 4,
/// verbatim. Does not call any `schema.rs` function, and must not: this copy
/// is frozen forever while `schema.rs` continues to evolve.
pub async fn create_baseline_schema(
    conn: &Connection,
    ctx: &MigrationContext,
) -> Result<(), libsql::Error> {
    create_stores(conn).await?;
    create_sources(conn).await?;
    create_resources(conn).await?;
    create_blocks(conn).await?;
    create_chunks(conn, ctx.embedding_dim, ctx.encoding).await?;
    create_fts(conn).await?;
    create_triggers(conn).await?;
    create_sync_state(conn).await?;
    create_credentials(conn).await?;
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
            id          TEXT PRIMARY KEY NOT NULL,
            store_id    TEXT NOT NULL REFERENCES stores(id) ON DELETE CASCADE,
            kind        TEXT NOT NULL,
            root        TEXT,
            url         TEXT,
            include     TEXT NOT NULL DEFAULT '[]',
            exclude     TEXT NOT NULL DEFAULT '[]',
            preset      TEXT NOT NULL DEFAULT 'prose',
            refresh     TEXT,
            created_at  TEXT NOT NULL,
            config_json TEXT,
            CHECK (
                (kind = 'path' AND root IS NOT NULL)
                OR (kind = 'url'  AND url  IS NOT NULL)
                OR (kind NOT IN ('path', 'url'))
            ),
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

async fn create_resources(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS resources (
            rowid             INTEGER PRIMARY KEY,
            store_id          TEXT NOT NULL REFERENCES stores(id) ON DELETE CASCADE,
            id                TEXT NOT NULL,
            source_id         TEXT NOT NULL,
            ingestor_kind     TEXT NOT NULL,
            resource_kind     TEXT NOT NULL,
            uri               TEXT NOT NULL,
            external_id       TEXT,
            external_etag     TEXT,
            content_hash      TEXT NOT NULL,
            title             TEXT,
            mime              TEXT,
            language          TEXT,
            date_original     TEXT,
            date_parsed       TEXT,
            added_at          TEXT NOT NULL,
            modified_at       TEXT NOT NULL,
            thread_id         TEXT,
            channel           TEXT,
            participants      TEXT DEFAULT '[]',
            metadata_json     TEXT NOT NULL,
            origin_store      TEXT NOT NULL,
            policy_version    TEXT NOT NULL,
            share_path        TEXT,
            extractor_version TEXT NOT NULL,
            UNIQUE (store_id, id),
            FOREIGN KEY (store_id, source_id) REFERENCES sources(store_id, id) ON DELETE CASCADE
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_resources_store_uri ON resources(store_id, uri)",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_resources_source_id ON resources(source_id)",
        (),
    )
    .await?;

    Ok(())
}

async fn create_blocks(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS blocks (
            rowid         INTEGER PRIMARY KEY,
            store_id      TEXT NOT NULL,
            resource_id   TEXT NOT NULL,
            seq           INTEGER NOT NULL,
            kind          TEXT NOT NULL,
            text          TEXT NOT NULL,
            metadata_json TEXT,
            location_json TEXT,
            UNIQUE (store_id, resource_id, seq),
            FOREIGN KEY (store_id, resource_id) REFERENCES resources(store_id, id) ON DELETE CASCADE
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_blocks_resource ON blocks(store_id, resource_id)",
        (),
    )
    .await?;

    Ok(())
}

/// The v4-era `chunks.embedding` column type mapping, inlined verbatim
/// rather than calling the live `crate::vectors::embedding_column_type`
/// helper.
///
/// This file is the FROZEN v4 baseline (see the module doc comment above) —
/// it must reproduce exactly what schema version 4 looked like, forever. If
/// `create_chunks` called the live helper instead, a future change to that
/// helper's mapping would silently change what this "frozen" baseline
/// produces too, and the runner's drift guard
/// (`drift_guard_create_schema_equals_baseline_plus_chain` in `runner.rs`)
/// would move both sides of its comparison together — this file and
/// `schema::create_schema`'s output — instead of catching the missing chain
/// entry a real change to the mapping would require. Copied verbatim from
/// `vectors::embedding_column_type` as it exists today; pinned against
/// literal strings by `embedding_column_type_v4_matches_pinned_strings`
/// below so it can never silently drift either.
fn embedding_column_type_v4(dim: usize, encoding: localdb_core::VectorEncoding) -> String {
    match encoding {
        localdb_core::VectorEncoding::Float32 => format!("F32_BLOB({dim})"),
        localdb_core::VectorEncoding::Binary => format!("F1BIT_BLOB({dim})"),
    }
}

async fn create_chunks(
    conn: &Connection,
    embedding_dim: usize,
    encoding: localdb_core::VectorEncoding,
) -> Result<(), libsql::Error> {
    let col_type = embedding_column_type_v4(embedding_dim, encoding);
    let chunks_ddl = format!(
        "CREATE TABLE IF NOT EXISTS chunks (
            rowid         INTEGER PRIMARY KEY,
            store_id      TEXT NOT NULL,
            id            TEXT NOT NULL,
            resource_id   TEXT NOT NULL,
            block_id      INTEGER NOT NULL,
            block_seq     INTEGER NOT NULL,
            seq_in_block  INTEGER NOT NULL DEFAULT 0,
            block_kind    TEXT,
            text          TEXT NOT NULL,
            heading_path  TEXT NOT NULL,
            embedding     {col_type} NOT NULL,
            location_json TEXT,
            UNIQUE (store_id, id),
            FOREIGN KEY (store_id, resource_id)
                REFERENCES resources(store_id, id) ON DELETE CASCADE
        )"
    );
    conn.execute(&chunks_ddl, ()).await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_chunks_store_resource ON chunks(store_id, resource_id)",
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

async fn create_sync_state(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sync_state (
            source_id    TEXT PRIMARY KEY,
            cursor_json  TEXT,
            last_sync_at TEXT,
            items_synced INTEGER DEFAULT 0
        )",
        (),
    )
    .await?;
    Ok(())
}

async fn create_credentials(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS credentials (
            ingestor_kind   TEXT NOT NULL,
            source_id       TEXT NOT NULL,
            key             TEXT NOT NULL,
            value_encrypted BLOB,
            updated_at      TEXT NOT NULL,
            PRIMARY KEY (ingestor_kind, source_id, key)
        )",
        (),
    )
    .await?;
    Ok(())
}

async fn set_user_version(conn: &Connection) -> Result<(), libsql::Error> {
    // `PRAGMA user_version = N` is idempotent. Use query() not execute()
    // because PRAGMAs may return rows.
    conn.query(
        &format!(
            "PRAGMA user_version = {version}",
            version = super::chain::BASELINE_VERSION
        ),
        (),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::chain::BASELINE_VERSION;
    use libsql::Builder;
    use localdb_core::VectorEncoding;
    use std::collections::HashSet;
    use tempfile::tempdir;

    async fn open_test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
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

    // Codex review #152 fix 3: pin the frozen v4 embedding column mapping
    // against literal strings, independent of whatever
    // `crate::vectors::embedding_column_type` currently produces — this is
    // what makes a future change to the live helper unable to silently drag
    // the "frozen" baseline along with it.
    #[test]
    fn embedding_column_type_v4_matches_pinned_strings() {
        assert_eq!(
            embedding_column_type_v4(384, VectorEncoding::Float32),
            "F32_BLOB(384)"
        );
        assert_eq!(
            embedding_column_type_v4(1024, VectorEncoding::Binary),
            "F1BIT_BLOB(1024)"
        );
    }

    #[tokio::test]
    async fn create_baseline_schema_succeeds_on_empty_db() {
        let (_dir, conn) = open_test_db().await;
        let ctx = MigrationContext {
            embedding_dim: 4,
            encoding: VectorEncoding::Float32,
        };
        create_baseline_schema(&conn, &ctx).await.unwrap();
    }

    #[tokio::test]
    async fn all_expected_tables_exist() {
        let (_dir, conn) = open_test_db().await;
        let ctx = MigrationContext {
            embedding_dim: 4,
            encoding: VectorEncoding::Float32,
        };
        create_baseline_schema(&conn, &ctx).await.unwrap();
        let names = table_names(&conn).await;
        for expected in [
            "stores",
            "sources",
            "resources",
            "blocks",
            "chunks",
            "chunks_fts",
            "sync_state",
            "credentials",
        ] {
            assert!(
                names.contains(expected),
                "expected table '{expected}' missing; have: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn all_expected_indexes_exist() {
        let (_dir, conn) = open_test_db().await;
        let ctx = MigrationContext {
            embedding_dim: 4,
            encoding: VectorEncoding::Float32,
        };
        create_baseline_schema(&conn, &ctx).await.unwrap();
        let names = index_names(&conn).await;
        for expected in [
            "idx_sources_store_id",
            "idx_sources_store_root",
            "idx_sources_store_url",
            "idx_resources_store_uri",
            "idx_resources_source_id",
            "idx_blocks_resource",
            "idx_chunks_store_resource",
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
        let ctx = MigrationContext {
            embedding_dim: 4,
            encoding: VectorEncoding::Float32,
        };
        create_baseline_schema(&conn, &ctx).await.unwrap();
        let names = trigger_names(&conn).await;
        for expected in ["chunks_ai", "chunks_ad", "chunks_au"] {
            assert!(
                names.contains(expected),
                "expected trigger '{expected}' missing; have: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn user_version_set_to_baseline_version() {
        let (_dir, conn) = open_test_db().await;
        let ctx = MigrationContext {
            embedding_dim: 4,
            encoding: VectorEncoding::Float32,
        };
        create_baseline_schema(&conn, &ctx).await.unwrap();
        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let v: i64 = row.get(0).unwrap();
        assert_eq!(v, BASELINE_VERSION);
        assert_eq!(
            BASELINE_VERSION, 4,
            "BASELINE_VERSION must equal today's v4"
        );
    }

    // Note: the direct baseline-vs-create_schema byte-equality check that
    // used to live here was removed once the migration runner landed. It
    // only held while `chain::migrations()` was empty, and would wrongly
    // fail the moment a real migration existed (baseline + chain would
    // legitimately diverge from a frozen "verbatim" baseline). The
    // equivalent (and now correct) drift guard is
    // `runner::tests::drift_guard_create_schema_equals_baseline_plus_chain`,
    // which compares `schema::create_schema`'s output against baseline DDL
    // plus the real chain applied on top — see docs/migrations.md.
}
