//! The migration chain: a frozen baseline version plus the list of
//! migrations that have shipped on top of it.

use localdb_core::Error;

use super::{Down, Migration, MigrationContext, Up};

/// The frozen v4 baseline version.
///
/// This replaced the old `schema::SCHEMA_VERSION` constant (now removed) as
/// the permanent anchor migrations count up from.
/// `baseline::create_baseline_schema` stamps `PRAGMA user_version =
/// BASELINE_VERSION` on a freshly-created database with no migrations
/// applied.
pub const BASELINE_VERSION: i64 = 4;

/// `v5`: drop `chunks.block_id`, swap in the composite
/// `idx_chunks_store_resource_pos` index, and retag
/// `resources.metadata_json` from the retired flat Dublin-Core-only shape to
/// the tagged `Metadata::Document` encoding.
///
/// Verbatim port of the manual `docs/migrations/v4-to-v5.sql` script (#151)
/// this refactor previously shipped as a run-before-upgrading escape hatch —
/// see that file's history for the full design rationale. The canonical
/// block reference is now `(store_id, resource_id, block_seq)`, looked up by
/// sequence number: `blocks.rowid` is not stable across a replace
/// (delete+insert of a resource mints new block rows), and window chunks
/// (#129) need to reference a *set* of block sequence numbers, which a
/// single scalar FK cannot express.
fn drop_chunks_block_id_and_retag_resource_metadata_up(_ctx: &MigrationContext) -> Vec<String> {
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

/// `v6`: `create_auth_tables` — the auth subsystem's 7 tables (`users`,
/// `auth_tokens`, `oauth_clients`, `auth_codes`, `store_grants`, `invites`,
/// `access_requests`) plus their 4 indexes.
///
/// Ported from the `auth` branch's (issue #98) `store-libsql/src/schema.rs`
/// `create_auth_tables`, converted from that branch's own ad-hoc
/// `Migration`/`MIGRATIONS`/`run_migrations` runner to this chain framework.
/// `auth`'s v5/v6 slots renumber to v6/v7 here since PR #151 claimed v5
/// first — see `docs/migrations.md`'s "Picking the next version". The
/// round-trip is exercised end to end by
/// `store-libsql/tests/real_migrations.rs`.
///
/// `access_requests` here does **not** include `collected_at` — that column
/// arrives in `v7` below. Including it here would make `v7`'s `ALTER TABLE
/// ADD COLUMN` fail with "duplicate column name" for a store migrating
/// v5 -> v7 in one hop. See `store-libsql/tests/real_migrations.rs`'s
/// `v5_create_auth_tables_up` fixture, which mirrors this verbatim — minus
/// the seed `INSERT` below, which that fixture deliberately omits as DML
/// irrelevant to exercising the generic runner/downgrade machinery; this
/// real chain entry needs the seed row (see its own comment below).
fn create_auth_tables_up(_ctx: &MigrationContext) -> Vec<String> {
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
        // Seed the built-in `localdb-cli` OAuth2 public client
        // (`localdb_core::auth::LOCALDB_CLI_CLIENT_ID`) so
        // `auth_codes.client_id`'s FK constraint is satisfiable the moment
        // `/authorize` issues a code for it. This is DML, not DDL, so it
        // produces no `sqlite_master` row and doesn't participate in the
        // write-twice drift guard — `schema::create_auth_tables` fires the
        // identical `INSERT OR IGNORE` for a fresh store; every existing v5
        // store gets it here via `db migrate`. Idempotent, so both firing is
        // safe regardless of which path a given store took.
        "INSERT OR IGNORE INTO oauth_clients (id, client_name, redirect_uris, created_at) \
         VALUES ('localdb-cli', 'localdb CLI', '[]', '1970-01-01T00:00:00Z')"
            .to_string(),
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
        // v6-era shape: no `collected_at` yet (added by v7 below).
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

/// `v6`'s down: drop the 7 auth tables. `DROP TABLE` drops its own indexes
/// automatically, so no separate `DROP INDEX` statements are needed.
/// Children-before-parents ordering (every FK here is in fact
/// `CASCADE`/`SET NULL`, so order isn't strictly required, but this is
/// FK-safe regardless) — mirrors
/// `store-libsql/tests/real_migrations.rs`'s `v5_create_auth_tables_down`.
fn create_auth_tables_down(_ctx: &MigrationContext) -> Vec<String> {
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

/// `v7`: adds `access_requests.collected_at` — the atomic "credential
/// handed out exactly once" guard for closed-mode invite polling
/// (`AuthStore::mark_access_request_collected`).
///
/// Ported from the `auth` branch's `add_access_requests_collected_at_column`.
/// That function guarded the `ALTER TABLE` behind a `pragma_table_info`
/// existence check because its own ad-hoc runner could reach this step from
/// two different starting shapes. A chain entry has no such ambiguity: the
/// chain guarantees this step runs exactly once, immediately after `v6`'s
/// `create_auth_tables`, whose `access_requests` never has `collected_at` —
/// so the guard is dropped here.
fn add_access_requests_collected_at_column_up(_ctx: &MigrationContext) -> Vec<String> {
    vec!["ALTER TABLE access_requests ADD COLUMN collected_at TEXT".to_string()]
}

fn add_access_requests_collected_at_column_down(_ctx: &MigrationContext) -> Vec<String> {
    vec!["ALTER TABLE access_requests DROP COLUMN collected_at".to_string()]
}

/// The real migration registry.
///
/// Consumer branches append entries starting at version `BASELINE_VERSION +
/// 1` (i.e. 5). Because two branches may add migrations concurrently,
/// whoever lands second is responsible for renumbering their entries to
/// stay contiguous with whatever landed first.
pub fn migrations() -> Vec<Migration> {
    vec![
        Migration {
            version: BASELINE_VERSION + 1,
            name: "drop_chunks_block_id_and_retag_resource_metadata",
            summary: "drops chunks.block_id, replaces idx_chunks_store_resource with \
                      idx_chunks_store_resource_pos, retags resources.metadata_json from the \
                      retired flat Dublin-Core shape to the tagged Metadata::Document encoding",
            up: Up::Sql(drop_chunks_block_id_and_retag_resource_metadata_up),
            down: Down::Unsupported(
                "chunks.block_id cannot be reconstructed; re-index required after downgrade",
            ),
            needs_reindex: true,
        },
        Migration {
            version: BASELINE_VERSION + 2,
            name: "create_auth_tables",
            summary: "adds the 7 auth tables (users, auth_tokens, oauth_clients, auth_codes, \
                      store_grants, invites, access_requests) and their indexes, plus the \
                      built-in localdb-cli OAuth2 client seed row",
            up: Up::Sql(create_auth_tables_up),
            down: Down::Sql(create_auth_tables_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 3,
            name: "add_access_requests_collected_at_column",
            summary: "adds access_requests.collected_at, the closed-mode invite-polling \
                      collected-exactly-once guard",
            up: Up::Sql(add_access_requests_collected_at_column_up),
            down: Down::Sql(add_access_requests_collected_at_column_down),
            needs_reindex: false,
        },
    ]
}

/// The schema version a database is at once every migration in `chain` has
/// been applied on top of the baseline.
pub fn head_version(chain: &[Migration]) -> i64 {
    BASELINE_VERSION + chain.len() as i64
}

/// This binary's head version: `head_version(&migrations())`.
///
/// A convenience for callers (the CLI's `db status`/`db migrate`/`db
/// downgrade`) that just want "what version should a healthy store be at"
/// without assembling the real chain themselves.
pub fn head_version_current() -> i64 {
    head_version(&migrations())
}

/// Verify that `chain`'s versions are contiguous starting at
/// `BASELINE_VERSION + 1`, i.e. `chain[i].version == BASELINE_VERSION + 1 + i`.
///
/// Returns `Error::Internal` naming the offending migration and its expected
/// version on the first mismatch found.
pub fn validate_chain(chain: &[Migration]) -> Result<(), Error> {
    for (i, migration) in chain.iter().enumerate() {
        let expected = BASELINE_VERSION + 1 + i as i64;
        if migration.version != expected {
            return Err(Error::Internal {
                message: format!(
                    "migration chain is not contiguous: entry '{name}' at index {i} \
                     has version {actual}, expected version {expected}",
                    name = migration.name,
                    actual = migration.version,
                ),
                correlation_id: "libsql_migrations_invalid_chain".to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::{Down, Up};

    fn trivial_up(_ctx: &super::super::MigrationContext) -> Vec<String> {
        vec!["CREATE TABLE t(x)".into()]
    }

    fn trivial_down(_ctx: &super::super::MigrationContext) -> Vec<String> {
        vec!["DROP TABLE t".into()]
    }

    fn fixture_migration(version: i64, name: &'static str) -> Migration {
        Migration {
            version,
            name,
            summary: "fixture migration for chain tests",
            up: Up::Sql(trivial_up),
            down: Down::Sql(trivial_down),
            needs_reindex: false,
        }
    }

    #[test]
    fn real_migrations_registry_passes_validation() {
        validate_chain(&migrations()).expect("real migrations() chain must be contiguous");
    }

    #[test]
    fn chain_with_a_gap_is_rejected() {
        let chain = vec![
            fixture_migration(BASELINE_VERSION + 1, "first"),
            fixture_migration(BASELINE_VERSION + 3, "skips_one"),
        ];
        let err = validate_chain(&chain).expect_err("gap in versions should be rejected");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_invalid_chain");
                assert!(
                    message.contains("skips_one"),
                    "error should name the offending migration: {message}"
                );
                assert!(
                    message.contains(&(BASELINE_VERSION + 2).to_string()),
                    "error should mention the expected version: {message}"
                );
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[test]
    fn chain_starting_at_wrong_version_is_rejected() {
        let chain = vec![fixture_migration(BASELINE_VERSION + 2, "wrong_start")];
        let err = validate_chain(&chain).expect_err("wrong starting version should be rejected");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_invalid_chain");
                assert!(message.contains("wrong_start"));
                assert!(message.contains(&(BASELINE_VERSION + 1).to_string()));
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[test]
    fn head_version_of_real_chain_is_baseline_plus_its_length() {
        assert_eq!(
            head_version(&migrations()),
            BASELINE_VERSION + migrations().len() as i64
        );
    }

    #[test]
    fn head_version_current_matches_head_version_of_real_migrations() {
        assert_eq!(head_version_current(), head_version(&migrations()));
    }
}
