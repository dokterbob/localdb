use libsql::Connection;
use localdb_core::{Error, VectorEncoding};

use crate::connection::map_libsql_err;
use crate::vectors::embedding_column_type;

/// Schema version stored in `PRAGMA user_version`.
///
/// Survives `VACUUM` and doesn't require a separate table. Replaces the
/// per-store `schema_version` table from the legacy schema.
///
/// v4 -> v5 (issue #98): added the auth tables (`users`, `auth_tokens`,
/// `oauth_clients`, `auth_codes`, `store_grants`, `invites`,
/// `access_requests`). Purely additive — see `MIGRATIONS` below.
pub const SCHEMA_VERSION: i64 = 5;

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
    create_resources(conn).await?;
    create_blocks(conn).await?;
    create_chunks(conn, embedding_dim, encoding).await?;
    create_fts(conn).await?;
    create_triggers(conn).await?;
    create_sync_state(conn).await?;
    create_credentials(conn).await?;
    create_auth_tables(conn).await?;
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

/// DDL for the auth subsystem (issue #98, D1/D5/D7/D13): `users`,
/// `auth_tokens`, `oauth_clients`, `auth_codes`, `store_grants`, `invites`,
/// `access_requests`.
///
/// Shared verbatim between fresh `create_schema` and the `4 -> 5` migration
/// step (`MIGRATIONS`) so both paths converge on an identical schema (D13).
/// `oauth_clients`/`auth_codes` have no corresponding `core::auth` Rust types
/// yet — their DDL ships now (so this is the only migration this feature
/// ever needs) but the OAuth2 code+PKCE flow lands in a later ticket.
async fn create_auth_tables(conn: &Connection) -> Result<(), libsql::Error> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS users (
            id         TEXT PRIMARY KEY NOT NULL,
            name       TEXT NOT NULL UNIQUE,
            role       TEXT NOT NULL,
            created_at TEXT NOT NULL
        )",
        (),
    )
    .await?;

    conn.execute(
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
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_auth_tokens_user ON auth_tokens(user_id)",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_auth_tokens_family ON auth_tokens(family_id)",
        (),
    )
    .await?;

    // OAuth2 dynamic client registration (RFC 7591) — client rows are not
    // written until a later ticket implements the `/register` route, but the
    // table ships now per D13 (one migration for the whole auth feature).
    conn.execute(
        "CREATE TABLE IF NOT EXISTS oauth_clients (
            id            TEXT PRIMARY KEY NOT NULL,
            client_name   TEXT,
            redirect_uris TEXT NOT NULL DEFAULT '[]',
            created_at    TEXT NOT NULL
        )",
        (),
    )
    .await?;

    // Seed the built-in `localdb-cli` public client (T4,
    // `localdb_core::auth::LOCALDB_CLI_CLIENT_ID`). Its recognition and
    // redirect-uri policy (RFC 8252 §7.3 loopback exception) are pure-core
    // logic (`localdb_core::auth::validate_redirect_uri`) — this row exists
    // solely so `auth_codes.client_id`'s FK constraint is satisfiable when
    // `/authorize` issues a code for it; `redirect_uris` is left empty here
    // since the actual policy is enforced in `core`, not read from this row.
    conn.execute(
        "INSERT OR IGNORE INTO oauth_clients (id, client_name, redirect_uris, created_at)
         VALUES ('localdb-cli', 'localdb CLI', '[]', '1970-01-01T00:00:00Z')",
        (),
    )
    .await?;

    // OAuth2 authorization codes (code+PKCE flow, later ticket).
    conn.execute(
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
        )",
        (),
    )
    .await?;

    // Store-name/user-id grants (D7). The composite primary key doubles as
    // the required UNIQUE (store, user) index. FK to `stores(name)` (which
    // is UNIQUE — see `create_stores`) cascades grant cleanup on store
    // deletion.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS store_grants (
            store_name TEXT NOT NULL REFERENCES stores(name) ON DELETE CASCADE,
            user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            granted_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (store_name, user_id)
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_store_grants_user ON store_grants(user_id)",
        (),
    )
    .await?;

    conn.execute(
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
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS access_requests (
            id                 TEXT PRIMARY KEY NOT NULL,
            invite_id          TEXT NOT NULL REFERENCES invites(id) ON DELETE CASCADE,
            requested_name     TEXT NOT NULL,
            secret_hash        TEXT NOT NULL,
            state              TEXT NOT NULL DEFAULT 'pending',
            resulting_user_id  TEXT REFERENCES users(id) ON DELETE SET NULL,
            created_at         TEXT NOT NULL,
            decided_at         TEXT
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_access_requests_invite ON access_requests(invite_id)",
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

// ---------------------------------------------------------------------------
// Stepwise migration runner (D13: never destructive)
// ---------------------------------------------------------------------------

/// A single migration step from schema version `from` to `to`.
///
/// `run` performs the additive DDL only — the caller (`run_migrations`) wraps
/// it in a transaction and bumps `PRAGMA user_version` atomically alongside
/// it, so a failure partway through leaves the database at the last
/// successfully completed version, never partially upgraded.
pub struct Migration {
    pub from: i64,
    pub to: i64,
    run: MigrationFn,
}

type MigrationFn = for<'a> fn(
    &'a Connection,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), libsql::Error>> + Send + 'a>,
>;

/// Ordered migration list, keyed by the version each step starts from.
///
/// Every step reuses the same DDL helper `create_schema` itself calls, so a
/// freshly created database and a migrated one converge on an identical
/// schema (D13). This PR ships exactly one step: `4 -> 5` (create the auth
/// tables). A schema version with no entry here — older than the oldest
/// step, or newer than `SCHEMA_VERSION` — has no migration path;
/// `run_migrations` returns a hard error rather than dropping data.
pub const MIGRATIONS: &[Migration] = &[Migration {
    from: 4,
    to: 5,
    run: |conn| Box::pin(create_auth_tables(conn)),
}];

/// Migrate `conn` from `current_version` up to `target_version`, one step at
/// a time, per `MIGRATIONS`.
///
/// Each step runs inside its own `BEGIN`/`COMMIT` transaction that also
/// bumps `PRAGMA user_version`, so a mid-step failure rolls back cleanly and
/// leaves the prior version's data completely intact — never a silent data
/// loss (D13). A version with no migration path is a hard error instructing
/// the user to recreate or reindex, returned *before* any transaction is
/// opened, so the database is left byte-for-byte untouched.
pub(crate) async fn run_migrations(
    conn: &Connection,
    mut current_version: i64,
    target_version: i64,
) -> Result<(), Error> {
    while current_version < target_version {
        let step = MIGRATIONS
            .iter()
            .find(|m| m.from == current_version)
            .ok_or_else(|| Error::InvalidConfig {
                message: format!(
                    "database schema version {current_version} has no migration path to \
                     v{target_version}; this build cannot upgrade it automatically — \
                     delete the database and re-run `localdb index` to recreate it, \
                     or restore from a backup taken before this version"
                ),
            })?;

        conn.execute("BEGIN", ()).await.map_err(map_libsql_err)?;
        let result: Result<(), libsql::Error> = async {
            (step.run)(conn).await?;
            conn.query(&format!("PRAGMA user_version = {}", step.to), ())
                .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                conn.execute("COMMIT", ()).await.map_err(map_libsql_err)?;
                current_version = step.to;
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(map_libsql_err(e));
            }
        }
    }
    Ok(())
}

/// Test-only helper: build the pre-auth (v4) schema directly, without the
/// auth tables `create_schema` now also creates, and stamp `user_version =
/// 4`. Used to construct a realistic "old" database for migration tests —
/// reuses the same per-table DDL functions `create_schema` calls, so it
/// stays in lockstep with the actual v4 shape instead of duplicating DDL.
#[cfg(test)]
pub(crate) async fn create_pre_auth_schema_v4_for_test(
    conn: &Connection,
) -> Result<(), libsql::Error> {
    create_stores(conn).await?;
    create_sources(conn).await?;
    create_resources(conn).await?;
    create_blocks(conn).await?;
    create_chunks(conn, 4, VectorEncoding::Float32).await?;
    create_fts(conn).await?;
    create_triggers(conn).await?;
    create_sync_state(conn).await?;
    create_credentials(conn).await?;
    conn.query("PRAGMA user_version = 4", ()).await?;
    Ok(())
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
        for expected in [
            "stores",
            "sources",
            "resources",
            "blocks",
            "chunks",
            "chunks_fts",
            "sync_state",
            "credentials",
            "users",
            "auth_tokens",
            "oauth_clients",
            "auth_codes",
            "store_grants",
            "invites",
            "access_requests",
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
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
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
            "idx_auth_tokens_user",
            "idx_auth_tokens_family",
            "idx_store_grants_user",
            "idx_access_requests_invite",
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
    /// Creates store-a and store-b, one source per store, and one resource in
    /// store-a that references store-a's source.  Returns early before the
    /// resource insert so callers can attempt their own insert and assert the
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

    /// A resource in store A must not be able to reference a source in store B.
    ///
    /// This guards against the cross-store contamination bug: with only a
    /// simple `REFERENCES sources(id)` FK a resource in store A could point to
    /// a source in store B, and a cascade-delete of store B would then silently
    /// remove store A's resources.  The composite FK
    /// `FOREIGN KEY (store_id, source_id) REFERENCES sources(store_id, id)`
    /// closes that gap.
    #[tokio::test]
    async fn cross_store_source_reference_is_rejected() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();

        insert_two_stores_and_sources(&conn).await;

        // Attempt: resource lives in store-a but references src-b (store-b).
        let result = conn
            .execute(
                "INSERT INTO resources \
                 (store_id, id, source_id, ingestor_kind, resource_kind, uri, \
                  content_hash, added_at, modified_at, origin_store, policy_version, \
                  metadata_json, extractor_version) \
                 VALUES \
                 ('store-a', 'res-x', 'src-b', 'path', 'file', 'file:///doc.md', \
                  'abc', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', 'store-a', '1', \
                  '{}', '1')",
                (),
            )
            .await;

        assert!(
            result.is_err(),
            "inserting a resource in store-a that references a source in store-b \
             should be rejected by the composite FK constraint"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("FOREIGN KEY"),
            "expected a FOREIGN KEY constraint error, got: {err_msg}"
        );
    }

    /// Deleting store B must not cascade-delete resources that belong to store A.
    #[tokio::test]
    async fn deleting_store_b_does_not_cascade_to_store_a_resources() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();

        insert_two_stores_and_sources(&conn).await;

        // Insert a resource in store-a that references store-a's own source.
        conn.execute(
            "INSERT INTO resources \
             (store_id, id, source_id, ingestor_kind, resource_kind, uri, \
              content_hash, added_at, modified_at, origin_store, policy_version, \
              metadata_json, extractor_version) \
             VALUES \
             ('store-a', 'res-1', 'src-a', 'path', 'file', 'file:///doc.md', \
              'abc', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', 'store-a', '1', \
              '{}', '1')",
            (),
        )
        .await
        .unwrap();

        // Delete store B — should cascade only to store B's own rows.
        conn.execute("DELETE FROM stores WHERE id = 'store-b'", ())
            .await
            .unwrap();

        // Store A's resource must still be present.
        let mut rows = conn
            .query("SELECT id FROM resources WHERE store_id = 'store-a'", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap();
        assert!(
            row.is_some(),
            "store A's resource should still exist after deleting store B"
        );
    }

    // -----------------------------------------------------------------
    // Migration runner (D13: stepwise, never destructive)
    // -----------------------------------------------------------------

    /// The `4 -> 5` migration adds the auth tables and preserves every
    /// pre-existing row — the whole point of D13.
    #[tokio::test]
    async fn migrate_v4_to_v5_adds_auth_tables_and_preserves_existing_rows() {
        let (_dir, conn) = open_test_db().await;
        create_pre_auth_schema_v4_for_test(&conn).await.unwrap();

        // Confirm the auth tables genuinely don't exist yet pre-migration.
        let names_before = table_names(&conn).await;
        assert!(
            !names_before.contains("auth_tokens"),
            "auth_tokens should not exist before migration"
        );

        // Real pre-existing data that must survive the migration untouched.
        insert_two_stores_and_sources(&conn).await;
        conn.execute(
            "INSERT INTO resources \
             (store_id, id, source_id, ingestor_kind, resource_kind, uri, \
              content_hash, added_at, modified_at, origin_store, policy_version, \
              metadata_json, extractor_version) \
             VALUES \
             ('store-a', 'res-1', 'src-a', 'path', 'file', 'file:///doc.md', \
              'abc', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', 'store-a', '1', \
              '{}', '1')",
            (),
        )
        .await
        .unwrap();

        let v = get_schema_version(&conn).await.unwrap();
        assert_eq!(v, 4);

        run_migrations(&conn, v, SCHEMA_VERSION).await.unwrap();

        let v_after = get_schema_version(&conn).await.unwrap();
        assert_eq!(v_after, SCHEMA_VERSION);

        let names_after = table_names(&conn).await;
        for expected in [
            "users",
            "auth_tokens",
            "oauth_clients",
            "auth_codes",
            "store_grants",
            "invites",
            "access_requests",
        ] {
            assert!(
                names_after.contains(expected),
                "expected auth table '{expected}' after migration; have: {names_after:?}"
            );
        }

        // Pre-existing rows are untouched.
        let mut rows = conn.query("SELECT COUNT(*) FROM stores", ()).await.unwrap();
        let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(count, 2, "both pre-existing stores must survive");

        let mut rows = conn
            .query("SELECT COUNT(*) FROM resources WHERE id = 'res-1'", ())
            .await
            .unwrap();
        let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(count, 1, "pre-existing resource must survive");
    }

    /// Migrating an already-current (v5) database is a no-op: the loop body
    /// never runs because `current_version == target_version`.
    #[tokio::test]
    async fn migrate_noop_when_already_current() {
        let (_dir, conn) = open_test_db().await;
        create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        run_migrations(&conn, SCHEMA_VERSION, SCHEMA_VERSION)
            .await
            .unwrap();
        assert_eq!(get_schema_version(&conn).await.unwrap(), SCHEMA_VERSION);
    }

    /// Versions 1-3 predate this migration list entirely: there is no step
    /// starting from them, so `run_migrations` must hard-error rather than
    /// guess. The database must be left completely untouched — no tables
    /// dropped, no partial DDL applied, version unchanged.
    #[tokio::test]
    async fn versions_with_no_migration_path_error_and_leave_db_untouched() {
        for old_version in [1i64, 2, 3] {
            let (_dir, conn) = open_test_db().await;
            // A pre-v4 database wouldn't have the full v4 shape, but all we
            // need here is *some* user table plus the stamped version, to
            // prove nothing about it changes.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS legacy_marker (id TEXT PRIMARY KEY)",
                (),
            )
            .await
            .unwrap();
            conn.execute("INSERT INTO legacy_marker (id) VALUES ('keep-me')", ())
                .await
                .unwrap();
            conn.query(&format!("PRAGMA user_version = {old_version}"), ())
                .await
                .unwrap();

            let result = run_migrations(&conn, old_version, SCHEMA_VERSION).await;
            match result {
                Err(Error::InvalidConfig { message }) => {
                    assert!(
                        message.contains("no migration path"),
                        "error should explain the lack of a migration path: {message}"
                    );
                }
                other => panic!("expected InvalidConfig for version {old_version}, got: {other:?}"),
            }

            // Nothing was touched: version unchanged, marker row still there.
            assert_eq!(get_schema_version(&conn).await.unwrap(), old_version);
            let mut rows = conn
                .query("SELECT COUNT(*) FROM legacy_marker", ())
                .await
                .unwrap();
            let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
            assert_eq!(count, 1, "pre-existing data must be untouched on error");
        }
    }
}
