//! Test for the manual v4 -> v5 migration script (`docs/migrations/v4-to-v5.sql`).
//!
//! Builds a v4-shaped database by hand (old `chunks.block_id` column, old
//! `idx_chunks_store_resource` index, flat/untagged `resources.metadata_json`,
//! `PRAGMA user_version = 4`) with at least one store/source/resource/chunk
//! row, applies the migration script file, then opens it through the normal
//! `SqliteBackend::open` path and asserts:
//!   - no wipe happened (the store/source/resource/chunk rows are intact)
//!   - `PRAGMA user_version` reads 5
//!   - `chunks` has no `block_id` column
//!   - the new composite index exists (and the old one doesn't)
//!   - `resources.metadata_json` was rewritten to the tagged shape

use std::path::{Path, PathBuf};

use libsql::Builder;
use localdb_core::{StoreBackend, StoreBackendConfig, VectorEncoding};
use store_libsql::SqliteBackend;
use tempfile::tempdir;

fn migration_script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/migrations/v4-to-v5.sql")
}

/// Build a v4-shaped `localdb.db` at `path` with one store, one source, one
/// resource (old flat `metadata_json`, no `"kind"` tag), and one chunk (with
/// the retired `block_id` column populated).
async fn build_v4_db(path: &Path) {
    let db = Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();

    // Minimal v4-shaped schema. FK enforcement is left OFF (libsql default)
    // for this raw setup so insert order doesn't matter.
    conn.execute(
        "CREATE TABLE stores (
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
    .await
    .unwrap();

    conn.execute(
        "CREATE TABLE sources (
            id          TEXT PRIMARY KEY NOT NULL,
            store_id    TEXT NOT NULL,
            kind        TEXT NOT NULL,
            root        TEXT,
            url         TEXT,
            include     TEXT NOT NULL DEFAULT '[]',
            exclude     TEXT NOT NULL DEFAULT '[]',
            preset      TEXT NOT NULL DEFAULT 'prose',
            refresh     TEXT,
            created_at  TEXT NOT NULL,
            config_json TEXT,
            UNIQUE (store_id, id)
        )",
        (),
    )
    .await
    .unwrap();

    conn.execute(
        "CREATE TABLE resources (
            rowid             INTEGER PRIMARY KEY,
            store_id          TEXT NOT NULL,
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
            UNIQUE (store_id, id)
        )",
        (),
    )
    .await
    .unwrap();

    // v4 chunks table: has block_id, F32_BLOB(4) to match the embedding_dim=4
    // this test opens with later.
    conn.execute(
        "CREATE TABLE chunks (
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
            embedding     F32_BLOB(4) NOT NULL,
            location_json TEXT,
            UNIQUE (store_id, id)
        )",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "CREATE INDEX idx_chunks_store_resource ON chunks(store_id, resource_id)",
        (),
    )
    .await
    .unwrap();

    // Seed data.
    conn.execute(
        "INSERT INTO stores (id, name, indexing_policy, policy_version, created_at)
         VALUES ('store-1', 'Store One', '{}', 'v1', '2026-01-01T00:00:00Z')",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO sources (id, store_id, kind, root, created_at)
         VALUES ('src-1', 'store-1', 'path', '/test/v4migration', '2026-01-01T00:00:00Z')",
        (),
    )
    .await
    .unwrap();

    // Old flat (untagged) metadata_json — no "kind" discriminator.
    let old_flat_metadata = r#"{"title":"Old Doc","creator":["Alice"],"subject":[],"description":null,"publisher":null,"contributor":[],"date":"2026-01-01","type":null,"format":null,"identifier":null,"source":null,"language":"en","relation":[],"coverage":null,"rights":null}"#;
    conn.execute(
        &format!(
            "INSERT INTO resources
                (store_id, id, source_id, ingestor_kind, resource_kind, uri, content_hash,
                 added_at, modified_at, origin_store, policy_version, metadata_json,
                 extractor_version)
             VALUES
                ('store-1', 'res-1', 'src-1', 'path', 'document', 'file:///doc.md', 'hash1',
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'store-1', 'v1', '{old_flat_metadata}', '1')"
        ),
        (),
    )
    .await
    .unwrap();

    conn.execute(
        "INSERT INTO chunks
            (store_id, id, resource_id, block_id, block_seq, seq_in_block, block_kind,
             text, heading_path, embedding, location_json)
         VALUES
            ('store-1', 'chunk-1', 'res-1', 0, 0, 0, 'paragraph',
             'chunk text', '[]', vector32('[0.1, 0.2, 0.3, 0.4]'), '{\"start\":0,\"end\":10}')",
        (),
    )
    .await
    .unwrap();

    conn.query("PRAGMA user_version = 4", ()).await.unwrap();
}

/// Split the migration script into individual statements and execute them
/// against a raw connection to the v4 database, mirroring how a user would
/// run `sqlite3 localdb.db < docs/migrations/v4-to-v5.sql`.
async fn apply_migration_script(conn: &libsql::Connection) {
    let script = std::fs::read_to_string(migration_script_path())
        .expect("docs/migrations/v4-to-v5.sql should exist and be readable");

    // Strip full-line `--` comments from the WHOLE file first (the prose in
    // this script's header comments contains semicolons of its own, e.g.
    // "left as-is; they are not translated..." — splitting on ';' before
    // removing comments would misparse those as statement boundaries).
    // Also strip sqlite3-CLI dot-commands (`.bail on`): they are interpreted
    // by the CLI, not the SQL engine, and this harness's abort-on-error
    // behavior (unwrap on the first failed statement) matches what `.bail on`
    // gives the CLI.
    let code_only: String = script
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !t.starts_with("--") && !t.starts_with('.')
        })
        .collect::<Vec<_>>()
        .join("\n");

    for raw_stmt in code_only.split(';') {
        let stmt = raw_stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        if stmt.to_uppercase().starts_with("PRAGMA") {
            conn.query(stmt, ()).await.unwrap();
        } else {
            conn.execute(stmt, ()).await.unwrap();
        }
    }
}

#[tokio::test]
async fn v4_to_v5_migration_script_preserves_data_and_upgrades_schema() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");

    build_v4_db(&path).await;

    // Apply the migration script via a fresh raw connection to the same file.
    {
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        apply_migration_script(&conn).await;

        // Sanity: metadata_json was rewritten to the tagged shape immediately
        // after applying the script (before going through the normal open path).
        let mut rows = conn
            .query("SELECT metadata_json FROM resources WHERE id = 'res-1'", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let metadata_json: String = row.get(0).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(parsed["kind"], "document");
        assert!(parsed["page_count"].is_null());
        assert!(parsed["word_count"].is_null());
        assert_eq!(parsed["title"], "Old Doc");
    }

    // Now open through the normal path used by the rest of the application.
    // If the migration script did its job, `user_version` already reads 5,
    // so `LibsqlDb::open` must NOT take the wipe-and-reinit branch.
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path.clone(),
        4,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();

    // The store/source/resource survived (no wipe).
    let store = backend.get_store("store-1").await.unwrap();
    assert!(store.is_some(), "store-1 should survive the migration");

    let source = backend.get_source("src-1").await.unwrap();
    assert!(source.is_some(), "src-1 should survive the migration");

    let handle = backend.retrieval_store("store-1").await.unwrap();
    let chunks = handle.get_chunks_for_resource("res-1").await.unwrap();
    assert_eq!(
        chunks.len(),
        1,
        "chunk-1 should survive the migration; got {} chunks",
        chunks.len()
    );
    assert_eq!(chunks[0].id, "chunk-1");
    assert_eq!(chunks[0].text, "chunk text");

    // Verify schema-level facts directly.
    let db = Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();

    let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
    let version: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert_eq!(version, 5, "user_version should be 5 after migration+open");

    let mut rows = conn
        .query(
            "SELECT name FROM pragma_table_info('chunks') WHERE name = 'block_id'",
            (),
        )
        .await
        .unwrap();
    assert!(
        rows.next().await.unwrap().is_none(),
        "chunks.block_id should be gone after migration"
    );

    let mut rows = conn
        .query(
            "SELECT name FROM sqlite_master WHERE type='index' AND name = 'idx_chunks_store_resource_pos'",
            (),
        )
        .await
        .unwrap();
    assert!(
        rows.next().await.unwrap().is_some(),
        "new composite index should exist after migration"
    );
}
