use libsql::Connection;
use localdb_core::VectorEncoding;

use crate::vectors::{embedding_column_type, vector_index_ddl};

/// Run the full DDL for the unified database.
///
/// Idempotent: safe to call on an already-created database. Does NOT set
/// connection-level PRAGMAs (`journal_mode`, `foreign_keys`, `busy_timeout`)
/// — that is the caller's responsibility (see `db::LibsqlDb::open`). Also
/// does NOT touch `PRAGMA user_version`: fresh-create callers
/// (`connection.rs`'s `Fresh` branch, `migrate.rs`'s v==0 and legacy-rebuild
/// paths) stamp it themselves, as the LAST step of
/// `runner::seed_for_fresh_create`'s seeding transaction, only after the
/// `schema_migrations` rows exist — see that function's doc comment for why
/// the ordering matters.
pub async fn create_schema(
    conn: &Connection,
    embedding_dim: usize,
    encoding: VectorEncoding,
) -> Result<(), libsql::Error> {
    create_stores(conn).await?;
    create_sources(conn).await?;
    create_resources(conn).await?;
    create_blocks(conn).await?;
    create_chunks(conn, embedding_dim, encoding).await?;
    create_fts(conn).await?;
    create_triggers(conn).await?;
    create_sync_state(conn).await?;
    create_credentials(conn).await?;
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
    // `feed_etag`, `feed_last_modified` and `feed_inputs_digest` are
    // appended after `config_json`
    // (and before the CHECK/UNIQUE table-level constraints) rather than
    // grouped with the source's own columns above: schema v8 adds them via
    // plain `ALTER TABLE sources ADD COLUMN`, which SQLite always appends
    // after the last existing column definition and before any table-level
    // constraints. This literal must stay byte-for-byte identical to that or
    // the drift-guard test
    // (`migrations::runner::drift_guard_create_schema_equals_baseline_plus_chain`)
    // fails (write-twice rule, docs/migrations.md).
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
            config_json TEXT, feed_etag TEXT, feed_last_modified TEXT, feed_inputs_digest TEXT,
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
    // `modified_at` and `index_updated_at` are both appended after
    // `extractor_version` rather than `modified_at` staying grouped with
    // `added_at` above: schema v7 relaxes `modified_at`'s `NOT NULL` via a
    // column-level add/copy/drop/rename dance (no `ALTER COLUMN` in SQLite,
    // and no table rebuild — chunks/blocks' foreign keys to resources make
    // that unsafe inside a migration transaction), which SQLite implements by
    // dropping the original column and re-adding it under the final name;
    // `index_updated_at` then lands via a plain `ALTER TABLE resources ADD
    // COLUMN` on top of that. SQLite always appends `ADD COLUMN` after the
    // last existing column definition, so both end up here, in application
    // order, in place of `modified_at`'s original position — verified
    // empirically. `external_last_modified` and `last_checked_at` (schema v8)
    // are two further plain `ALTER TABLE ... ADD COLUMN`s on top of that, so
    // they land last, in the order their migration statements run. This
    // literal must stay byte-for-byte identical to the result or the
    // drift-guard test
    // (`migrations::runner::drift_guard_create_schema_equals_baseline_plus_chain`)
    // fails (write-twice rule, docs/migrations.md).
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
            thread_id         TEXT,
            channel           TEXT,
            participants      TEXT DEFAULT '[]',
            metadata_json     TEXT NOT NULL,
            origin_store      TEXT NOT NULL,
            policy_version    TEXT NOT NULL,
            share_path        TEXT,
            extractor_version TEXT NOT NULL, modified_at TEXT, index_updated_at TEXT, external_last_modified TEXT, last_checked_at TEXT,
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

async fn create_chunks(
    conn: &Connection,
    embedding_dim: usize,
    encoding: VectorEncoding,
) -> Result<(), libsql::Error> {
    let col_type = embedding_column_type(embedding_dim, encoding);
    let chunks_ddl = format!(
        "CREATE TABLE IF NOT EXISTS chunks (
            rowid         INTEGER PRIMARY KEY,
            store_id      TEXT NOT NULL,
            id            TEXT NOT NULL,
            resource_id   TEXT NOT NULL,
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

    // Canonical block reference is (store_id, resource_id, block_seq) — see
    // schema v5 (#128): no block_id/rowid FK. This composite index supports
    // both per-document chunk listing and block-scoped context expansion.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_chunks_store_resource_pos \
         ON chunks(store_id, resource_id, block_seq, seq_in_block)",
        (),
    )
    .await?;

    // DiskANN index. Tuning is encoding-dependent and derived in one place —
    // see `vectors::vector_index_params` for the block-size cost model and
    // why a binary column must not carry float8 neighbors (issue #179).
    // Schema v6 must keep emitting this byte-for-byte identically to the
    // chain migration; both call the same helper for exactly that reason.
    conn.execute(&vector_index_ddl(encoding), ()).await?;

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

/// Read the schema version from `PRAGMA user_version`.
///
/// Returns `0` on a freshly-created (un-touched) database, and on one where
/// `create_schema` has run but nothing has stamped a version yet (see that
/// function's doc comment). Returns the value last stamped by
/// `runner::seed_for_fresh_create`, the migration runner, or downgrade (or
/// any other writer) on an initialized one.
pub(crate) async fn get_schema_version(conn: &Connection) -> Result<i64, libsql::Error> {
    let mut rows = conn.query("PRAGMA user_version", ()).await?;
    match rows.next().await? {
        Some(row) => row.get::<i64>(0),
        None => Ok(0),
    }
}

/// Drop all user-created tables, indexes, triggers, and virtual tables so the
/// schema can be cleanly recreated from scratch.
///
/// No longer called from `LibsqlDb::open` — a version-mismatched store is
/// never mutated on open (see `connection.rs`). This is the primitive behind
/// the explicit legacy-rebuild path in `migrations::migrate::migrate_store`
/// (for pre-baseline v1-v3 stores, where the user has explicitly opted into
/// erasing and rebuilding the database via `allow_legacy_rebuild`).
pub(crate) async fn drop_all_tables(conn: &Connection) -> Result<(), libsql::Error> {
    // Collect all triggers first, then drop them.
    let mut triggers = Vec::new();
    {
        let mut rows = conn
            .query("SELECT name FROM sqlite_master WHERE type = 'trigger'", ())
            .await?;
        while let Some(row) = rows.next().await? {
            triggers.push(row.get::<String>(0)?);
        }
    }
    for name in &triggers {
        conn.execute(&format!("DROP TRIGGER IF EXISTS \"{name}\""), ())
            .await?;
    }

    // Collect all virtual tables (fts5 etc.) and regular tables.
    let mut tables = Vec::new();
    {
        let mut rows = conn
            .query(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') \
                 AND name NOT LIKE 'sqlite_%'",
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            tables.push(row.get::<String>(0)?);
        }
    }
    // Disable FK enforcement during the drop cascade so order doesn't matter.
    conn.execute("PRAGMA foreign_keys = OFF", ()).await?;
    for name in &tables {
        conn.execute(&format!("DROP TABLE IF EXISTS \"{name}\""), ())
            .await?;
    }
    conn.execute("PRAGMA foreign_keys = ON", ()).await?;

    // Reset user_version to 0.
    conn.query("PRAGMA user_version = 0", ()).await?;

    Ok(())
}

#[cfg(test)]
mod tests;
