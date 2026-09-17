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

#[test]
fn head_version_current_is_eleven() {
    // Pins the concrete number so a chain edit that silently drops or
    // duplicates an entry fails here, not just via the relative
    // assertions above.
    assert_eq!(head_version_current(), 11);
}
