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
