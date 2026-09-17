//! The migration chain: a frozen baseline version plus the list of
//! migrations that have shipped on top of it.

use localdb_core::{Error, VectorEncoding};

use crate::vectors::vector_index_ddl;

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

/// The exact `chunks_vec_idx` DDL every store carried at schema v5, frozen
/// here as the v6 down-step's target.
///
/// Deliberately a literal rather than `vectors::vector_index_ddl(Float32)` —
/// the two strings coincide today, but this one is a historical constant that
/// must never move if the live tuning changes again.
const V5_VECTOR_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS chunks_vec_idx ON chunks(\
     libsql_vector_idx(embedding, 'metric=cosine', 'max_neighbors=64', 'compress_neighbors=float8'))";

/// `v6`: rebuild `chunks_vec_idx` without `compress_neighbors=float8` /
/// `max_neighbors=64` on binary-encoded stores (issues #179, #177).
///
/// v5 pinned both params for every encoding. On an `F1BIT_BLOB` column that
/// made each DiskANN node blob 67,216 bytes — 9× larger than necessary — for
/// no recall benefit, because float8 edge vectors of a 1-bit source hold only
/// 0 or 255 per byte and so carry exactly the information the 128-byte node
/// vector already has. See `vectors::vector_index_params` for the full cost
/// model. Dropping the params takes the per-row cost to 7,488 bytes; a 600k
/// chunk store goes from ~40 GB of index to ~4.5 GB.
///
/// **Weight class 2** (in-DB rebuild), *not* class 3: `CREATE INDEX` on a
/// vector index returns `CREATE_OK` rather than `CREATE_OK_SKIP_REFILL`, so
/// SQLite runs its normal refill and re-inserts every existing row straight
/// from `chunks.embedding`. No re-embedding, no model download, hence
/// `needs_reindex: false`. It is still a long operation on a large store —
/// one DiskANN insert per chunk — which is why `db migrate` reports per-step
/// progress.
///
/// Freed pages land on the freelist, so the file does not shrink on its own.
/// `db migrate` points the user at `localdb db vacuum` to reclaim them (issue
/// #177, where a `VACUUM` run *before* any rebuild correctly reclaimed
/// nothing).
fn shrink_vector_index_up(ctx: &MigrationContext) -> Vec<String> {
    match ctx.encoding {
        // Drop-first is deliberate, per `runner::apply_pending`'s note:
        // whether libsql unwinds partial ANN construction on rollback is
        // unverified, so a retried migration must not meet a half-built index.
        VectorEncoding::Binary => vec![
            "DROP INDEX IF EXISTS chunks_vec_idx".to_string(),
            vector_index_ddl(VectorEncoding::Binary),
        ],
        // F32_BLOB stores already have the right tuning — for a 4 KiB node
        // vector, float8 edges are a real 4× compression and libsql's default
        // max_neighbors would be 3× worse. Rebuilding would burn minutes to
        // land on a byte-identical index, so this is a bookkeeping-only step
        // for them.
        VectorEncoding::Float32 => vec![],
    }
}

/// The v6 down-step: restore the v5 float8/64 index on binary stores.
///
/// Reversible (unlike v5) because nothing is discarded — the index is derived
/// data rebuilt from `chunks.embedding` in either direction.
fn shrink_vector_index_down(ctx: &MigrationContext) -> Vec<String> {
    match ctx.encoding {
        VectorEncoding::Binary => vec![
            "DROP INDEX IF EXISTS chunks_vec_idx".to_string(),
            V5_VECTOR_INDEX_DDL.to_string(),
        ],
        VectorEncoding::Float32 => vec![],
    }
}

/// `v7`: relax `resources.modified_at`'s `NOT NULL` constraint (not every
/// ingestor kind can supply one) and add `resources.index_updated_at`,
/// backfilled from `added_at` so no row is left `NULL`.
///
/// `index_updated_at` tracks when *our store* last wrote a resource's
/// chunks — distinct from `added_at` (first-ever write, preserved across
/// replaces) and `modified_at` (the origin's claimed content modification
/// time). See specs/02-domain-model.md §2.
///
/// The `modified_at` relaxation can't use a plain `ALTER TABLE ... DROP
/// COLUMN` + `ADD COLUMN` (SQLite has no `ALTER COLUMN`) and can't rebuild
/// `resources` via `DROP TABLE` + recreate either: `chunks` and `blocks` both
/// carry `FOREIGN KEY ... REFERENCES resources` (`schema::create_chunks`,
/// `schema::create_blocks`), and `PRAGMA foreign_keys` can't be toggled
/// mid-transaction (`runner::apply_one` runs every migration inside one), so
/// dropping `resources` here would be either refused by FK enforcement or —
/// if enforcement were ever off — leave `chunks`/`blocks` referencing nothing.
/// Instead this is a column-level dance:
///
/// 1. Add a nullable `modified_at_new` column.
/// 2. Copy every row's `modified_at` into it.
/// 3. Drop the original (`NOT NULL`) `modified_at` column.
/// 4. Rename `modified_at_new` back to `modified_at`.
///
/// `ALTER TABLE ... ADD COLUMN` always appends after the last existing column
/// definition (and before any table-level constraints) — verified empirically
/// against a real database — so running this dance before the
/// `index_updated_at` add is what makes the final column order deterministic:
/// `modified_at` (relaxed, now nullable) lands immediately after
/// `extractor_version`, and `index_updated_at` lands after that. See
/// `schema::create_resources`'s comment for the resulting literal (the
/// write-twice fold-in).
fn relax_modified_at_and_add_index_updated_at_up(_ctx: &MigrationContext) -> Vec<String> {
    vec![
        "ALTER TABLE resources ADD COLUMN modified_at_new TEXT".to_string(),
        "UPDATE resources SET modified_at_new = modified_at".to_string(),
        "ALTER TABLE resources DROP COLUMN modified_at".to_string(),
        "ALTER TABLE resources RENAME COLUMN modified_at_new TO modified_at".to_string(),
        "ALTER TABLE resources ADD COLUMN index_updated_at TEXT".to_string(),
        "UPDATE resources SET index_updated_at = added_at WHERE index_updated_at IS NULL"
            .to_string(),
    ]
}

/// `v8`: add the five conditional-GET/liveness columns
/// (`specs/02-domain-model.md` §"Resource", "Conditional GET and pruning";
/// `specs/04-search-pipeline.md` "Incremental re-index", "Deletes").
///
/// `resources.external_last_modified` is the raw HTTP `Last-Modified`
/// validator, stored verbatim and replayed byte-exact in `If-Modified-Since`
/// — distinct from the `modified_at` date axis (a parsed, source-claimed
/// change time) even once a future `Last-Modified` source lands there.
/// `resources.last_checked_at` is the last successful origin contact for the
/// resource's URI — a `200` or `304` that left the store consistent for it —
/// advanced by the entry loop, `url` sources, single-document feed mode and
/// the liveness sweep, with one deliberate exception: the liveness sweep
/// alone also advances it on a `Blocked` or transport-error probe outcome, so
/// a run of permanently-blocked entries doesn't monopolize the sweep's
/// oldest-first candidate ordering forever. Deliberately a separate column
/// from `index_updated_at`, which normatively means "we last wrote this
/// resource's stored state" and is exposed via
/// `DocumentInfo.index_updated_at`; a successful check that leaves content
/// and metadata unchanged writes nothing there, so reusing that column would
/// misreport a merely-checked resource as re-written.
/// `sources.feed_etag`/`sources.feed_last_modified` are the feed
/// document's own validators, kept on the `sources` row because in
/// discovery mode the feed document itself never becomes a `Resource`.
/// `sources.feed_inputs_digest` guards those two: it records the local
/// inputs — indexing policy, `fetch_full_content`, `max_entries` — that
/// produced the last feed run, so a run whose inputs have moved refuses to
/// replay the validators and refetches unconditionally. An origin's
/// validator only ever speaks for the origin's own bytes, and a 304 that
/// skips the entry loop would otherwise strand every entry on the old
/// inputs indefinitely.
///
/// **Weight class 1** (fast DDL): five plain `ALTER TABLE ... ADD COLUMN`
/// statements, each nullable with no default — no rewrite, no index rebuild,
/// hence `needs_reindex: false`. All five are pure cache state: losing them
/// costs a re-fetch, never a re-index, which is why no row is backfilled and
/// `NULL` is a valid steady state for every one of them. For the digest
/// specifically, `NULL` reads as "inputs unknown", which is a mismatch
/// against any current digest, so an upgraded store refetches its feeds once
/// rather than trusting a validator captured before the guard existed.
fn add_conditional_get_validators_up(_ctx: &MigrationContext) -> Vec<String> {
    vec![
        "ALTER TABLE resources ADD COLUMN external_last_modified TEXT".to_string(),
        "ALTER TABLE resources ADD COLUMN last_checked_at TEXT".to_string(),
        "ALTER TABLE sources ADD COLUMN feed_etag TEXT".to_string(),
        "ALTER TABLE sources ADD COLUMN feed_last_modified TEXT".to_string(),
        "ALTER TABLE sources ADD COLUMN feed_inputs_digest TEXT".to_string(),
    ]
}

/// The v8 down-step: drop all five columns. Reversible with a plain `ALTER
/// TABLE ... DROP COLUMN` — unlike `BASELINE_VERSION + 3`, none of these
/// columns carries a constraint or an original position to restore, so no
/// table rebuild is needed in either direction.
fn add_conditional_get_validators_down(_ctx: &MigrationContext) -> Vec<String> {
    vec![
        "ALTER TABLE resources DROP COLUMN external_last_modified".to_string(),
        "ALTER TABLE resources DROP COLUMN last_checked_at".to_string(),
        "ALTER TABLE sources DROP COLUMN feed_etag".to_string(),
        "ALTER TABLE sources DROP COLUMN feed_last_modified".to_string(),
        "ALTER TABLE sources DROP COLUMN feed_inputs_digest".to_string(),
    ]
}

/// `v9`: create the auth tables and seed the CLI OAuth client.
/// `access_requests.collected_at` is deliberately added separately by v10.
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
        // v9 shape: `collected_at` is added by v10 below.
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

/// `v9`'s down: drop the 7 auth tables. `DROP TABLE` drops its own indexes
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

/// `v10`: guard closed-invite credential collection with an atomic marker.
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
            name: "shrink_vector_index",
            summary: "rebuilds chunks_vec_idx without compress_neighbors=float8/max_neighbors=64 \
                      on binary-encoded stores, cutting the per-chunk DiskANN block from 67,216 \
                      to 7,488 bytes (9.0x); run `localdb db vacuum` afterwards to return the \
                      freed pages to the filesystem",
            up: Up::Sql(shrink_vector_index_up),
            down: Down::Sql(shrink_vector_index_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 3,
            name: "relax_modified_at_and_add_index_updated_at",
            summary: "relaxes resources.modified_at's NOT NULL constraint (via an add/copy/drop/ \
                      rename column dance, since chunks/blocks foreign keys to resources make a \
                      table rebuild unsafe here) and adds resources.index_updated_at (write-time \
                      clock of the last index write for a resource), backfilled from added_at so \
                      no row is left NULL",
            up: Up::Sql(relax_modified_at_and_add_index_updated_at_up),
            down: Down::Unsupported(
                "resources.modified_at's NOT NULL constraint and original column position \
                 cannot be restored by ALTER TABLE alone (SQLite can only append columns); \
                 downgrading would require rebuilding the resources table, which \
                 chunks/blocks' foreign keys to it make unsafe inside a migration transaction",
            ),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 4,
            name: "add_conditional_get_validators",
            summary: "adds resources.external_last_modified (raw HTTP Last-Modified \
                      conditional-GET validator, replayed byte-exact in If-Modified-Since) and \
                      resources.last_checked_at (last successful origin contact for the \
                      resource's URI, advanced by the entry loop, url sources, single-document \
                      feed mode and the liveness sweep, except the liveness sweep also advances \
                      it on a Blocked or transport-error probe outcome, its one deliberate \
                      exception, so permanently-blocked entries don't monopolize its \
                      oldest-first ordering; distinct from index_updated_at because a \
                      successful check that leaves content and metadata unchanged writes \
                      nothing), plus sources.feed_etag and sources.feed_last_modified (the feed \
                      document's own validators, kept on sources since a feed document never \
                      becomes a Resource in discovery mode) and sources.feed_inputs_digest \
                      (the local inputs behind the last feed run, so a policy change stops those \
                      validators being replayed); all five nullable, no default. Downgrading \
                      past this migration discards any accumulated validators, costing one full \
                      re-fetch of every URL and feed entry on the next upgrade",
            up: Up::Sql(add_conditional_get_validators_up),
            down: Down::Sql(add_conditional_get_validators_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 5,
            name: "create_auth_tables",
            summary: "adds the 7 auth tables (users, auth_tokens, oauth_clients, auth_codes, \
                      store_grants, invites, access_requests) and their indexes, plus the \
                      built-in localdb-cli OAuth2 client seed row",
            up: Up::Sql(create_auth_tables_up),
            down: Down::Sql(create_auth_tables_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 6,
            name: "add_access_requests_collected_at_column",
            summary: "adds access_requests.collected_at, the closed-mode invite-polling \
                      collected-exactly-once guard",
            up: Up::Sql(add_access_requests_collected_at_column_up),
            down: Down::Sql(add_access_requests_collected_at_column_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 7,
            name: "add_pending_bootstrap",
            summary: "retain interrupted first-admin setup for recovery",
            up: Up::Sql(|_| {
                vec!["CREATE TABLE IF NOT EXISTS pending_bootstrap (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), user_id TEXT NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE)".into()]
            }),
            down: Down::Sql(|_| vec!["DROP TABLE pending_bootstrap".into()]),
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
mod tests;
