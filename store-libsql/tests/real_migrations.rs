//! End-to-end integration test for the schema-migrations framework
//! (issue #127) against a fixture chain whose DDL was originally copied
//! **verbatim** from two in-flight consumer branches, as of 2026-07-08:
//!
//! - `v5`/`v6` mirror the `auth` branch's (issue #98, not yet landed)
//!   `store-libsql/src/schema.rs`: `create_auth_tables` (the 7 auth tables +
//!   their indexes) and the `v5 -> v6` `add_access_requests_collected_at_column`
//!   step.
//! - `v7` mirrors PR #151 / `refactor/117-parser-ingestor-wiring`'s former
//!   `docs/migrations/v4-to-v5.sql`: dropping `chunks.block_id`, replacing
//!   `idx_chunks_store_resource` with `idx_chunks_store_resource_pos`, and
//!   retagging `resources.metadata_json` from the old flat Dublin-Core shape
//!   to the tagged `Metadata::Document` shape.
//!
//! **PR #151 has since landed**: its migration is now the real chain's first
//! entry, `chain::migrations()`'s version 5
//! (`drop_chunks_block_id_and_retag_resource_metadata` in `chain.rs`) — not
//! version 7, since `auth`'s v5/v6 hadn't claimed those slots yet at adoption
//! time. This file's own `fixture_chain()` deliberately keeps its own
//! independent v5/v6/v7 numbering rather than reusing `chain::migrations()`:
//! it exists to exercise the generic runner/downgrade machinery against a
//! *multi-step* chain (an auth-shaped pair plus a block_id-drop-shaped
//! step), which is broader coverage than replaying today's one-entry real
//! chain would give. `migrations::runner::drift_guard_create_schema_equals_baseline_plus_chain`
//! is the test that pins the real chain (`chain::migrations()`) against
//! `schema::create_schema` instead; this file's fixtures are intentionally
//! synthetic and may drift from whatever the real chain looks like at any
//! given time.

use std::path::{Path, PathBuf};

use libsql::{params, Builder, Connection};
use tempfile::TempDir;

use localdb_core::{Error, VectorEncoding};
use store_libsql::migrations::baseline::create_baseline_schema;
use store_libsql::migrations::runner::apply_pending;
use store_libsql::migrations::table::{self, MigrationRow};
use store_libsql::migrations::{Down, Migration, MigrationContext, Up};
use store_libsql::{downgrade_store, BASELINE_VERSION};

// -- Fixture chain: v5/v6 mirror `auth`, v7 mirrors PR #151 -----------------

/// `auth`'s `create_auth_tables` (schema.rs ~line 314), verbatim, EXCEPT
/// `access_requests.collected_at` is stripped: that column didn't exist in
/// the v5-era DDL (see `auth`'s own comment ahead of
/// `add_access_requests_collected_at_column`, ~line 465-476) — it's added by
/// the v6 step below. At `auth`'s HEAD, `create_auth_tables` already
/// includes `collected_at` (the write-twice fold-in for a fresh create), so
/// this fixture's v5 statements are the pre-fold-in, v5-era shape.
fn v5_create_auth_tables_up(_ctx: &MigrationContext) -> Vec<String> {
    vec![
        "CREATE TABLE IF NOT EXISTS users (
            id         TEXT PRIMARY KEY NOT NULL,
            name       TEXT NOT NULL UNIQUE,
            role       TEXT NOT NULL,
            created_at TEXT NOT NULL
        )"
        .to_string(),
        "CREATE TABLE IF NOT EXISTS auth_tokens (
            id            TEXT PRIMARY KEY NOT NULL,
            user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            kind          TEXT NOT NULL,
            secret_hash   TEXT NOT NULL UNIQUE,
            expires_at    TEXT,
            last_used_at  TEXT,
            revoked_at    TEXT,
            created_at    TEXT NOT NULL,
            family_id     TEXT,
            rotated_from  TEXT
        )"
        .to_string(),
        "CREATE INDEX IF NOT EXISTS idx_auth_tokens_user ON auth_tokens(user_id)".to_string(),
        "CREATE INDEX IF NOT EXISTS idx_auth_tokens_family ON auth_tokens(family_id)".to_string(),
        "CREATE TABLE IF NOT EXISTS oauth_clients (
            id            TEXT PRIMARY KEY NOT NULL,
            client_name   TEXT,
            redirect_uris TEXT NOT NULL DEFAULT '[]',
            created_at    TEXT NOT NULL
        )"
        .to_string(),
        // NOTE: `auth`'s `create_auth_tables` also seeds an `INSERT OR
        // IGNORE` row for the built-in `localdb-cli` OAuth client here. That
        // seed is DML, not DDL, and irrelevant to exercising the migration
        // machinery (no fixture data in this test references it), so it's
        // deliberately omitted — a deviation from "verbatim" worth flagging.
        "CREATE TABLE IF NOT EXISTS auth_codes (
            id                    TEXT PRIMARY KEY NOT NULL,
            client_id             TEXT NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
            user_id               TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            code_hash             TEXT NOT NULL UNIQUE,
            code_challenge        TEXT NOT NULL,
            code_challenge_method TEXT NOT NULL DEFAULT 'S256',
            redirect_uri          TEXT NOT NULL,
            expires_at            TEXT NOT NULL,
            consumed_at           TEXT,
            created_at            TEXT NOT NULL
        )"
        .to_string(),
        "CREATE TABLE IF NOT EXISTS store_grants (
            store_name TEXT NOT NULL REFERENCES stores(name) ON DELETE CASCADE,
            user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            granted_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (store_name, user_id)
        )"
        .to_string(),
        "CREATE INDEX IF NOT EXISTS idx_store_grants_user ON store_grants(user_id)".to_string(),
        "CREATE TABLE IF NOT EXISTS invites (
            id           TEXT PRIMARY KEY NOT NULL,
            token_hash   TEXT NOT NULL UNIQUE,
            mode         TEXT NOT NULL,
            store_grants TEXT NOT NULL DEFAULT '[]',
            max_uses     INTEGER NOT NULL DEFAULT 1,
            uses         INTEGER NOT NULL DEFAULT 0,
            expires_at   TEXT,
            revoked_at   TEXT,
            created_by   TEXT NOT NULL,
            created_at   TEXT NOT NULL
        )"
        .to_string(),
        // v5-era shape: no `collected_at` yet (added by v6 below).
        "CREATE TABLE IF NOT EXISTS access_requests (
            id                 TEXT PRIMARY KEY NOT NULL,
            invite_id          TEXT NOT NULL REFERENCES invites(id) ON DELETE CASCADE,
            requested_name     TEXT NOT NULL,
            secret_hash        TEXT NOT NULL,
            state              TEXT NOT NULL DEFAULT 'pending',
            resulting_user_id  TEXT REFERENCES users(id) ON DELETE SET NULL,
            created_at         TEXT NOT NULL,
            decided_at         TEXT
        )"
        .to_string(),
        "CREATE INDEX IF NOT EXISTS idx_access_requests_invite ON access_requests(invite_id)"
            .to_string(),
    ]
}

/// Drop the 7 auth tables. `DROP TABLE` drops its own indexes automatically,
/// so no explicit `DROP INDEX` is needed. Order is children-before-parents
/// (`auth_codes`/`access_requests`/`store_grants`/`auth_tokens` all carry FKs
/// to `oauth_clients`/`invites`/`users`) so the drop is FK-safe regardless of
/// each FK's `ON DELETE` action — even though every FK here is in fact
/// `CASCADE`/`SET NULL`, so order wouldn't strictly matter either way.
fn v5_create_auth_tables_down(_ctx: &MigrationContext) -> Vec<String> {
    vec![
        "DROP TABLE auth_codes".to_string(),
        "DROP TABLE access_requests".to_string(),
        "DROP TABLE store_grants".to_string(),
        "DROP TABLE auth_tokens".to_string(),
        "DROP TABLE invites".to_string(),
        "DROP TABLE oauth_clients".to_string(),
        "DROP TABLE users".to_string(),
    ]
}

/// `auth`'s `add_access_requests_collected_at_column` (schema.rs ~line 476).
/// Unlike `auth`'s own version, this has NO `pragma_table_info` guard: that
/// guard exists there to make the same function idempotent whether a store
/// jumps straight from v4 (already-tagged via `create_auth_tables` at HEAD)
/// or was already sitting at v5 pre-ticket. A migration *chain* entry doesn't
/// need that guard — the chain guarantees this step runs exactly once, from
/// the exact v5 predecessor fixed above (without `collected_at`).
fn v6_up(_ctx: &MigrationContext) -> Vec<String> {
    vec!["ALTER TABLE access_requests ADD COLUMN collected_at TEXT".to_string()]
}
fn v6_down(_ctx: &MigrationContext) -> Vec<String> {
    vec!["ALTER TABLE access_requests DROP COLUMN collected_at".to_string()]
}

/// PR #151 / `refactor/117-parser-ingestor-wiring`'s
/// `docs/migrations/v4-to-v5.sql`, verbatim, minus the sqlite3 dot-command
/// (`.bail on`) and the `BEGIN IMMEDIATE`/`COMMIT`/`PRAGMA user_version = 5`
/// wrapper — the runner owns the transaction and the version stamp itself
/// (see `runner::apply_one`).
fn v7_up(_ctx: &MigrationContext) -> Vec<String> {
    vec![
        "ALTER TABLE chunks DROP COLUMN block_id".to_string(),
        "DROP INDEX IF EXISTS idx_chunks_store_resource".to_string(),
        "CREATE INDEX IF NOT EXISTS idx_chunks_store_resource_pos \
         ON chunks(store_id, resource_id, block_seq, seq_in_block)"
            .to_string(),
        "UPDATE resources \
         SET metadata_json = json_set( \
             metadata_json, \
             '$.kind', 'document', \
             '$.page_count', NULL, \
             '$.word_count', NULL \
         ) \
         WHERE json_valid(metadata_json) \
           AND json_extract(metadata_json, '$.kind') IS NULL"
            .to_string(),
    ]
}

fn fixture_chain() -> Vec<Migration> {
    vec![
        Migration {
            version: BASELINE_VERSION + 1,
            name: "create_auth_tables",
            summary: "mirrors auth branch: adds the 7 auth tables (users, auth_tokens, \
                      oauth_clients, auth_codes, store_grants, invites, access_requests)",
            up: Up::Sql(v5_create_auth_tables_up),
            down: Down::Sql(v5_create_auth_tables_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 2,
            name: "add_access_requests_collected_at_column",
            summary: "mirrors auth branch: adds access_requests.collected_at",
            up: Up::Sql(v6_up),
            down: Down::Sql(v6_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 3,
            name: "drop_chunks_block_id_and_retag_resource_metadata",
            summary: "mirrors PR #151 (docs/migrations/v4-to-v5.sql): drops chunks.block_id, \
                      replaces idx_chunks_store_resource with \
                      idx_chunks_store_resource_pos, retags resources.metadata_json",
            up: Up::Sql(v7_up),
            down: Down::Unsupported(
                "chunks.block_id cannot be reconstructed; re-index required after downgrade",
            ),
            needs_reindex: true,
        },
    ]
}

// -- Shared scaffolding ------------------------------------------------------

fn ctx() -> MigrationContext {
    MigrationContext {
        embedding_dim: 4,
        encoding: VectorEncoding::Float32,
    }
}

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    (dir, path)
}

async fn open_conn(path: &Path) -> (libsql::Database, Connection) {
    let db = Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
    (db, conn)
}

/// Seed realistic v4-era rows across stores/sources/resources/blocks/chunks:
/// two stores, each with one source, one resource (old flat, untagged
/// `metadata_json`), two blocks, and two chunks (with a real embedding
/// vector literal and a `block_id` pointing at one of the seeded blocks).
async fn seed_v4_data(conn: &Connection) {
    for (store_id, name) in [("store-1", "Store One"), ("store-2", "Store Two")] {
        conn.execute(
            "INSERT INTO stores (id, name, indexing_policy, policy_version, created_at) \
             VALUES (?, ?, '{}', '1', '2024-01-01T00:00:00Z')",
            params![store_id, name],
        )
        .await
        .unwrap();
    }

    for (src_id, store_id, root) in [
        ("src-1", "store-1", "/test/path1"),
        ("src-2", "store-2", "/test/path2"),
    ] {
        conn.execute(
            "INSERT INTO sources (id, store_id, kind, root, created_at) \
             VALUES (?, ?, 'path', ?, '2024-01-01T00:00:00Z')",
            params![src_id, store_id, root],
        )
        .await
        .unwrap();
    }

    // Old flat, untagged Dublin-Core-only metadata_json shape (pre-v7).
    for (res_id, store_id, src_id, uri, title) in [
        ("res-1", "store-1", "src-1", "file:///doc1.md", "Doc One"),
        ("res-2", "store-2", "src-2", "file:///doc2.md", "Doc Two"),
    ] {
        let metadata_json =
            format!("{{\"title\":\"{title}\",\"creator\":\"Alice\",\"language\":\"en\"}}");
        conn.execute(
            "INSERT INTO resources \
             (store_id, id, source_id, ingestor_kind, resource_kind, uri, \
              content_hash, added_at, modified_at, origin_store, policy_version, \
              metadata_json, extractor_version) \
             VALUES (?, ?, ?, 'path', 'file', ?, 'hash-abc', \
                     '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', ?, '1', ?, '1')",
            params![store_id, res_id, src_id, uri, store_id, metadata_json],
        )
        .await
        .unwrap();
    }

    // Two blocks + two chunks per resource, with explicit rowids so the
    // chunk rows can reference a known block_id without needing
    // last_insert_rowid().
    let mut block_rowid = 1i64;
    for (store_id, res_id) in [("store-1", "res-1"), ("store-2", "res-2")] {
        for seq in 0..2i64 {
            conn.execute(
                "INSERT INTO blocks (rowid, store_id, resource_id, seq, kind, text) \
                 VALUES (?, ?, ?, ?, 'paragraph', ?)",
                params![
                    block_rowid,
                    store_id,
                    res_id,
                    seq,
                    format!("block text {block_rowid}")
                ],
            )
            .await
            .unwrap();

            let chunk_id = format!("chunk-{block_rowid}");
            conn.execute(
                &format!(
                    "INSERT INTO chunks \
                     (store_id, id, resource_id, block_id, block_seq, seq_in_block, \
                      block_kind, text, heading_path, embedding) \
                     VALUES ('{store_id}', '{chunk_id}', '{res_id}', {block_rowid}, {seq}, 0, \
                             'paragraph', 'chunk text {block_rowid}', '[]', \
                             vector32('[0.1,0.2,0.3,0.4]'))"
                ),
                (),
            )
            .await
            .unwrap();

            block_rowid += 1;
        }
    }
}

async fn user_version(conn: &Connection) -> i64 {
    let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

async fn table_exists(conn: &Connection, name: &str) -> bool {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?",
            params![name],
        )
        .await
        .unwrap();
    rows.next().await.unwrap().is_some()
}

async fn index_exists(conn: &Connection, name: &str) -> bool {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name = ?",
            params![name],
        )
        .await
        .unwrap();
    rows.next().await.unwrap().is_some()
}

async fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let mut rows = conn
        .query(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?"),
            params![column],
        )
        .await
        .unwrap();
    let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
    count > 0
}

async fn row_count(conn: &Connection, table: &str) -> i64 {
    let mut rows = conn
        .query(&format!("SELECT COUNT(*) FROM {table}"), ())
        .await
        .unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

/// `sqlite_master` rows with sqlite's own bookkeeping, FTS5 shadow tables,
/// and `schema_migrations` itself stripped out — the same normalization
/// `runner.rs`'s drift-guard test and `downgrade.rs`'s fixtures use to
/// compare two databases' user-visible schema.
async fn normalized_master_rows(conn: &Connection) -> Vec<(String, String, String)> {
    let mut rows = conn
        .query(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE 'chunks_fts_%' \
             AND name != 'schema_migrations' \
             ORDER BY type, name",
            (),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        out.push((
            row.get::<String>(0).unwrap(),
            row.get::<String>(1).unwrap(),
            row.get::<String>(2).unwrap(),
        ));
    }
    out
}

/// Everything a refused migrate/downgrade must leave untouched: raw
/// `sqlite_master` rows, `PRAGMA user_version`, and `schema_migrations`'
/// rows (if present). Mirrors the lib's own (private) `test_fixtures::dump_db`.
#[derive(Debug, PartialEq)]
struct DbDump {
    master_rows: Vec<(String, String, String)>,
    user_version: i64,
    migration_rows: Vec<MigrationRow>,
}

async fn dump_db(path: &Path) -> DbDump {
    let (_db, conn) = open_conn(path).await;

    let mut rows = conn
        .query(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            (),
        )
        .await
        .unwrap();
    let mut master_rows = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        master_rows.push((
            row.get::<String>(0).unwrap(),
            row.get::<String>(1).unwrap(),
            row.get::<String>(2).unwrap(),
        ));
    }

    let version = user_version(&conn).await;
    let migration_rows = if table_exists(&conn, "schema_migrations").await {
        table::list_rows_desc_above(&conn, i64::MIN).await.unwrap()
    } else {
        Vec::new()
    };

    DbDump {
        master_rows,
        user_version: version,
        migration_rows,
    }
}

/// Build a v4 baseline DB at `path`, seed it, and apply `chain` (a prefix of
/// [`fixture_chain`]) on top. Drops its connection before returning so a
/// subsequent path-based reopen (`downgrade_store`, or a fresh inspection
/// connection) doesn't contend with it.
async fn build_seeded_db(path: &Path, chain: &[Migration]) {
    let (_db, conn) = open_conn(path).await;
    create_baseline_schema(&conn, &ctx()).await.unwrap();
    seed_v4_data(&conn).await;
    apply_pending(&conn, chain, &ctx()).await.unwrap();
}

// -- Tests -------------------------------------------------------------------

/// 1. Forward end-to-end: v4 -> v7 preserves seeded data and produces exactly
/// the schema PR #151 and the auth branch each independently describe.
#[tokio::test]
async fn forward_migration_v4_to_v7_matches_real_consumer_ddl() {
    let (_dir, path) = temp_db_path();
    let (_db, conn) = open_conn(&path).await;
    create_baseline_schema(&conn, &ctx()).await.unwrap();
    seed_v4_data(&conn).await;

    let chain = fixture_chain();
    let report = apply_pending(&conn, &chain, &ctx()).await.unwrap();
    assert_eq!(report.applied.len(), 3);

    assert_eq!(user_version(&conn).await, BASELINE_VERSION + 3);

    // The 7 auth tables exist.
    for table in [
        "users",
        "auth_tokens",
        "oauth_clients",
        "auth_codes",
        "store_grants",
        "invites",
        "access_requests",
    ] {
        assert!(table_exists(&conn, table).await, "missing table {table}");
    }

    // access_requests has collected_at (added at v6).
    assert!(column_exists(&conn, "access_requests", "collected_at").await);

    // chunks has no block_id column (dropped at v7).
    assert!(!column_exists(&conn, "chunks", "block_id").await);

    // Index swap at v7.
    assert!(!index_exists(&conn, "idx_chunks_store_resource").await);
    assert!(index_exists(&conn, "idx_chunks_store_resource_pos").await);

    // Seeded data preserved: counts.
    assert_eq!(row_count(&conn, "stores").await, 2);
    assert_eq!(row_count(&conn, "sources").await, 2);
    assert_eq!(row_count(&conn, "resources").await, 2);
    assert_eq!(row_count(&conn, "blocks").await, 4);
    assert_eq!(row_count(&conn, "chunks").await, 4);

    // Spot-check a chunk row survives with its text and store/resource
    // linkage intact (block_id column is gone, so it's not selected here).
    let mut rows = conn
        .query(
            "SELECT store_id, resource_id, text FROM chunks WHERE id = 'chunk-1'",
            (),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("chunk-1 must survive");
    let store_id: String = row.get(0).unwrap();
    let resource_id: String = row.get(1).unwrap();
    let text: String = row.get(2).unwrap();
    assert_eq!(store_id, "store-1");
    assert_eq!(resource_id, "res-1");
    assert_eq!(text, "chunk text 1");

    // resources.metadata_json is now tagged: kind == "document", page_count
    // and word_count present (and NULL), original flat fields intact.
    let mut rows = conn
        .query("SELECT metadata_json FROM resources WHERE id = 'res-1'", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let metadata_json: String = row.get(0).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
    assert_eq!(parsed["kind"], "document");
    assert!(parsed.get("page_count").is_some());
    assert!(parsed["page_count"].is_null());
    assert!(parsed.get("word_count").is_some());
    assert!(parsed["word_count"].is_null());
    assert_eq!(parsed["title"], "Doc One");
    assert_eq!(parsed["creator"], "Alice");
    assert_eq!(parsed["language"], "en");
}

/// 2. Backward with an Unsupported stop: downgrading from v7 all the way to
/// baseline is refused because v7 (the PR #151 mirror) has no down path, and
/// the refusal must name it and leave the store untouched.
#[tokio::test]
async fn downgrade_from_v7_is_refused_and_leaves_store_untouched() {
    let (_dir, path) = temp_db_path();
    build_seeded_db(&path, &fixture_chain()).await;

    let before = dump_db(&path).await;
    let result = downgrade_store(&path, None).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("drop_chunks_block_id_and_retag_resource_metadata"),
                "should name the blocking migration: {message}"
            );
            assert!(
                message.contains(&(BASELINE_VERSION + 3).to_string()),
                "should name the blocking version (7): {message}"
            );
            assert!(
                message.contains("chunks.block_id cannot be reconstructed"),
                "should include the stored reason: {message}"
            );
            assert!(
                message.contains("--to 7"),
                "should suggest downgrading to v7's own version to keep it applied: {message}"
            );
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }

    assert_eq!(
        before, after,
        "a refused downgrade must not mutate the store at all"
    );
}

/// 4. Full reversibility without v7: applying only the auth mirror (v5+v6)
/// and downgrading all the way back to baseline using nothing but the stored
/// down-SQL must restore an exact fresh-baseline schema, while preserving
/// the pre-seeded v4 data untouched.
#[tokio::test]
async fn downgrade_v6_to_baseline_restores_fresh_schema_and_keeps_v4_data() {
    let (_dir, path) = temp_db_path();
    let chain = fixture_chain();
    build_seeded_db(&path, &chain[..2]).await; // v5 + v6 only, no v7

    downgrade_store(&path, Some(BASELINE_VERSION))
        .await
        .unwrap();

    let (_db, conn) = open_conn(&path).await;
    assert_eq!(user_version(&conn).await, BASELINE_VERSION);

    let (_fresh_dir, fresh_path) = temp_db_path();
    let (_fresh_db, fresh_conn) = open_conn(&fresh_path).await;
    create_baseline_schema(&fresh_conn, &ctx()).await.unwrap();

    assert_eq!(
        normalized_master_rows(&conn).await,
        normalized_master_rows(&fresh_conn).await,
        "a full downgrade of the auth mirror must restore exactly a fresh baseline schema"
    );

    // Pre-seeded v4 data must survive the round trip (neither v5 nor v6
    // touch stores/sources/resources/blocks/chunks).
    assert_eq!(row_count(&conn, "stores").await, 2);
    assert_eq!(row_count(&conn, "sources").await, 2);
    assert_eq!(row_count(&conn, "resources").await, 2);
    assert_eq!(row_count(&conn, "blocks").await, 4);
    assert_eq!(row_count(&conn, "chunks").await, 4);

    let mut rows = conn
        .query("SELECT metadata_json FROM resources WHERE id = 'res-1'", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let metadata_json: String = row.get(0).unwrap();
    assert_eq!(
        metadata_json, "{\"title\":\"Doc One\",\"creator\":\"Alice\",\"language\":\"en\"}",
        "metadata_json must be untouched (only the v7 step retags it)"
    );
}

/// 5. Mid-chain downgrade: from a v6 store (auth mirror only, no v7),
/// downgrading to v5 must drop `access_requests.collected_at`, keep the 7
/// auth tables in place, and remove exactly the v6 `schema_migrations` row.
#[tokio::test]
async fn downgrade_v6_to_v5_removes_only_the_collected_at_column() {
    let (_dir, path) = temp_db_path();
    let chain = fixture_chain();
    build_seeded_db(&path, &chain[..2]).await; // v5 + v6

    let report = downgrade_store(&path, Some(BASELINE_VERSION + 1))
        .await
        .unwrap();
    assert_eq!(report.to_version, BASELINE_VERSION + 1);

    let (_db, conn) = open_conn(&path).await;
    assert_eq!(user_version(&conn).await, BASELINE_VERSION + 1);

    assert!(!column_exists(&conn, "access_requests", "collected_at").await);
    for table in [
        "users",
        "auth_tokens",
        "oauth_clients",
        "auth_codes",
        "store_grants",
        "invites",
        "access_requests",
    ] {
        assert!(
            table_exists(&conn, table).await,
            "auth table {table} must still be present after downgrading only the v6 step"
        );
    }

    let remaining = table::list_rows_desc_above(&conn, BASELINE_VERSION)
        .await
        .unwrap();
    let versions: Vec<i64> = remaining.iter().map(|r| r.version).collect();
    assert_eq!(
        versions,
        vec![BASELINE_VERSION + 1],
        "only the v5 row should remain above baseline; the v6 row must be gone"
    );
}

/// End-to-end proof against the REAL, compiled chain (`chain::migrations()`,
/// via the public `store_libsql::migrate_store`), not the fixture mirror
/// above: seed a realistic v4 store (the same `seed_v4_data` fixture the
/// fixture-chain test uses — two stores, blocks with a `block_id` FK,
/// untagged flat `metadata_json`), run `migrate_store` exactly the way
/// `localdb db migrate` does, and confirm it lands on v5 with `block_id`
/// gone, the composite index swapped in, and `resources.metadata_json`
/// retagged — while leaving the chunk rows' own data intact.
#[tokio::test]
async fn migrate_store_on_real_chain_drops_block_id_and_retags_metadata() {
    let (_dir, path) = temp_db_path();
    {
        let (_db, conn) = open_conn(&path).await;
        create_baseline_schema(&conn, &ctx()).await.unwrap();
        seed_v4_data(&conn).await;
    }

    let report = store_libsql::migrate_store(&path, &ctx(), false)
        .await
        .unwrap();

    assert_eq!(report.from_version, BASELINE_VERSION);
    assert_eq!(report.to_version, BASELINE_VERSION + 3);
    assert_eq!(
        report.applied.iter().map(|s| s.version).collect::<Vec<_>>(),
        vec![
            BASELINE_VERSION + 1,
            BASELINE_VERSION + 2,
            BASELINE_VERSION + 3
        ],
        "a v4 store steps through the whole compiled chain, not just v5"
    );
    assert!(!report.legacy_rebuilt);
    assert!(
        report.staleness_marked,
        "the block_id-drop migration is needs_reindex: true (v6/v7, the index \
         shrink and the index_updated_at backfill, are not)"
    );

    let (_db, conn) = open_conn(&path).await;
    assert_eq!(user_version(&conn).await, BASELINE_VERSION + 3);
    assert!(!column_exists(&conn, "chunks", "block_id").await);
    assert!(!index_exists(&conn, "idx_chunks_store_resource").await);
    assert!(index_exists(&conn, "idx_chunks_store_resource_pos").await);

    // Seeded rows preserved.
    assert_eq!(row_count(&conn, "stores").await, 2);
    assert_eq!(row_count(&conn, "chunks").await, 4);
    let mut rows = conn
        .query(
            "SELECT store_id, resource_id, text FROM chunks WHERE id = 'chunk-1'",
            (),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("chunk-1 must survive");
    let text: String = row.get(2).unwrap();
    assert_eq!(text, "chunk text 1");

    // metadata_json retagged to the tagged Metadata::Document shape.
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
    assert_eq!(parsed["title"], "Doc One");
}

/// v7 (`add_index_updated_at`), against the REAL compiled chain: a v4 store
/// migrated all the way to head gets `resources.index_updated_at` backfilled
/// from `added_at` for every pre-existing row, so no row is left `NULL`.
#[tokio::test]
async fn migrate_store_on_real_chain_backfills_index_updated_at_from_added_at() {
    let (_dir, path) = temp_db_path();
    {
        let (_db, conn) = open_conn(&path).await;
        create_baseline_schema(&conn, &ctx()).await.unwrap();
        seed_v4_data(&conn).await;
    }

    let report = store_libsql::migrate_store(&path, &ctx(), false)
        .await
        .unwrap();
    assert_eq!(report.to_version, BASELINE_VERSION + 3);

    let (_db, conn) = open_conn(&path).await;
    assert!(column_exists(&conn, "resources", "index_updated_at").await);

    for res_id in ["res-1", "res-2"] {
        let mut rows = conn
            .query(
                "SELECT added_at, index_updated_at FROM resources WHERE id = ?",
                params![res_id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let added_at: String = row.get(0).unwrap();
        let index_updated_at: Option<String> = row.get(1).unwrap();
        assert_eq!(
            index_updated_at.as_deref(),
            Some(added_at.as_str()),
            "{res_id}'s index_updated_at must be backfilled from added_at"
        );
    }
}

/// v7's down-step: downgrading a store sitting at head (v7) back to v6 drops
/// `resources.index_updated_at` and nothing else, via the real compiled
/// chain's stored `down_sql` — not a fixture mirror.
#[tokio::test]
async fn downgrade_real_chain_from_head_drops_index_updated_at() {
    let (_dir, path) = temp_db_path();
    {
        let (_db, conn) = open_conn(&path).await;
        create_baseline_schema(&conn, &ctx()).await.unwrap();
        seed_v4_data(&conn).await;
    }
    store_libsql::migrate_store(&path, &ctx(), false)
        .await
        .unwrap();

    let report = store_libsql::downgrade_store(&path, Some(BASELINE_VERSION + 2))
        .await
        .unwrap();
    assert_eq!(report.to_version, BASELINE_VERSION + 2);

    let (_db, conn) = open_conn(&path).await;
    assert_eq!(user_version(&conn).await, BASELINE_VERSION + 2);
    assert!(!column_exists(&conn, "resources", "index_updated_at").await);

    // v6's own effects (chunks_vec_idx tuning) are untouched by this
    // downgrade — only the v7 step is reverted.
    assert_eq!(row_count(&conn, "resources").await, 2);
}
