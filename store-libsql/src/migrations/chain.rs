//! The migration chain: a frozen baseline version plus the list of
//! migrations that have shipped on top of it.

use localdb_core::Error;

use super::Migration;

/// The frozen v4 baseline version.
///
/// This replaced the old `schema::SCHEMA_VERSION` constant (now removed) as
/// the permanent anchor migrations count up from.
/// `baseline::create_baseline_schema` stamps `PRAGMA user_version =
/// BASELINE_VERSION` on a freshly-created database with no migrations
/// applied.
pub const BASELINE_VERSION: i64 = 4;

/// The real migration registry.
///
/// Empty in this PR — consumer branches append entries starting at version
/// `BASELINE_VERSION + 1` (i.e. 5). Because two branches may add migrations
/// concurrently, whoever lands second is responsible for renumbering their
/// entries to stay contiguous with whatever landed first.
pub fn migrations() -> Vec<Migration> {
    Vec::new()
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
    fn head_version_of_empty_real_chain_is_baseline_version() {
        assert_eq!(head_version(&migrations()), BASELINE_VERSION);
    }

    #[test]
    fn head_version_current_matches_head_version_of_real_migrations() {
        assert_eq!(head_version_current(), head_version(&migrations()));
    }
}
