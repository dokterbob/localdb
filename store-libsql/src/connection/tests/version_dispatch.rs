//! Version-classification dispatch: `classify_version` and every
//! `LibsqlDb::open` disposition branch (legacy, pending, at-head, too-new)
//! that leans on it.

use tempfile::tempdir;

use localdb_core::{Error, VectorEncoding};

use super::common::dump_db;
use crate::connection::{classify_version, LibsqlDb, VersionDisposition};
use crate::migrations::chain;
use crate::schema;

#[tokio::test]
async fn reopen_with_legacy_schema_version_is_refused_without_mutation() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    // Stamp version 1 (pre-baseline legacy) on a raw libsql DB (bypassing
    // LibsqlDb::open).
    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.query("PRAGMA user_version = 1", ()).await.unwrap();
    }

    let before = dump_db(&path).await;
    let result = LibsqlDb::open(&path, 4, VectorEncoding::Float32).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("db migrate"),
                "error should point at 'localdb db migrate': {message}"
            );
            assert!(
                message.contains("predates"),
                "error should explain the version predates the baseline: {message}"
            );
        }
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!("expected InvalidConfig, but reopen of legacy schema succeeded"),
    }

    assert_eq!(
        before, after,
        "a refused open of a legacy-version store must not mutate it at all"
    );
}

/// A DB stamped at the pre-#128 v4 schema (old `chunks.block_id` column
/// and `idx_chunks_store_resource` index, `user_version=4`, no
/// `schema_migrations` table) is exactly the `Pending` disposition this
/// binary's compiled chain now makes reachable (head is 5, one migration
/// past baseline) — `LibsqlDb::open` must refuse it with a `db migrate`
/// hint and leave it byte-for-byte untouched, not silently wipe and
/// reinitialise it the way the pre-framework binary used to.
#[tokio::test]
async fn reopen_with_v4_era_block_id_schema_is_refused_without_mutation() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        // Old (v4) chunks table shape: has block_id, old index name.
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
        conn.query("PRAGMA user_version = 4", ()).await.unwrap();
    }

    let before = dump_db(&path).await;
    let result = LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("db migrate"),
                "error should point at 'localdb db migrate': {message}"
            );
            assert!(
                message.contains("behind"),
                "error should explain the version is behind this build: {message}"
            );
        }
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!(
            "expected InvalidConfig, but reopen of a pending v4-era block_id schema succeeded"
        ),
    }

    assert_eq!(
        before, after,
        "a refused open of a pending store must not mutate it at all — block_id, the old \
         index, and user_version=4 must all still be exactly as they were"
    );
}

#[tokio::test]
async fn reopen_with_newer_schema_version_returns_invalid_config_error() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    let head = chain::head_version(&chain::migrations());
    // Stamp a version head + 1 on a raw libsql DB (bypassing LibsqlDb::open).
    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let future_version = head + 1;
        conn.query(&format!("PRAGMA user_version = {future_version}"), ())
            .await
            .unwrap();
    }

    let before = dump_db(&path).await;
    let result = LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("newer"),
                "error should mention 'newer': {message}"
            );
            assert!(
                message.contains("db downgrade"),
                "error should point at 'localdb db downgrade': {message}"
            );
        }
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!("expected InvalidConfig, but reopen with newer schema succeeded"),
    }

    assert_eq!(
        before, after,
        "a refused open of a too-new store must not mutate it at all"
    );
}

// -- classify_version: the pure five-way dispatch helper, exercised
// directly against a synthetic head (in addition to the real chain's
// current head, which `reopen_with_v4_era_block_id_schema_is_refused_
// without_mutation` above already exercises `Pending` through).
#[test]
fn classify_version_covers_all_five_branches() {
    let baseline = chain::BASELINE_VERSION;
    assert_eq!(classify_version(0, baseline), VersionDisposition::Fresh);
    assert_eq!(
        classify_version(1, baseline),
        VersionDisposition::Legacy,
        "1 < BASELINE_VERSION is legacy"
    );
    assert_eq!(
        classify_version(baseline, baseline + 2),
        VersionDisposition::Pending,
        "at baseline but behind a (synthetic) head is pending"
    );
    assert_eq!(
        classify_version(baseline, baseline),
        VersionDisposition::AtHead
    );
    assert_eq!(
        classify_version(baseline + 1, baseline),
        VersionDisposition::TooNew
    );
}

// Plan test 12 (superseded): opening a raw v4 store that predates the
// migrations framework (no `schema_migrations` table at all) used to be
// silently backfilled with just the baseline row when the real chain
// was empty — back then `head == BASELINE_VERSION`, so a bare-baseline
// store genuinely was `AtHead`. Now that a real chain entry exists, that
// same store is `Pending` instead (see
// `reopen_with_v4_era_block_id_schema_is_refused_without_mutation`
// above), so `AtHead`'s backfill path (`table::ensure_baseline_row`
// followed by `checksum::verify_checksums`) is only ever reachable with
// *some* bookkeeping already in place.
//
// This test now pins the resulting behavior for a store that is
// fabricated to claim head's `user_version` without ever having run
// through the framework (impossible via any real code path once the
// chain is non-empty, since reaching head always means `apply_pending`
// or `seed_for_fresh_create` ran and left chain-entry rows behind): it
// must be refused as corrupt bookkeeping, not silently trusted just
// because the version number matches.
#[tokio::test]
async fn at_head_store_missing_chain_entry_rows_is_refused_not_silently_trusted() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    let head = chain::head_version(&chain::migrations());

    // Build a store with today's head DDL and `user_version` stamped
    // straight to head, but no `schema_migrations` table at all.
    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
        schema::create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        conn.query(&format!("PRAGMA user_version = {head}"), ())
            .await
            .unwrap();
    }

    match LibsqlDb::open(&path, 4, VectorEncoding::Float32).await {
        Err(Error::Internal { message, .. }) => {
            assert!(
                message.contains("missing a row"),
                "error should explain the bookkeeping is incomplete: {message}"
            );
        }
        Err(other) => panic!("expected Internal, got: {other:?}"),
        Ok(_) => panic!(
            "expected Internal error: an at-head store with no chain-entry bookkeeping \
             rows must not be silently trusted"
        ),
    }
}

// Fix 1 (adversarial review, track 4): the fabricated at-head store above
// (real chain head > BASELINE_VERSION, no `schema_migrations` table) must
// be refused WITHOUT `open` having created the table (or its baseline
// row) first. Before this fix, the `AtHead` branch unconditionally
// created the table and — because it was absent — backfilled the
// baseline row, then only afterward let `verify_checksums` refuse for the
// still-missing v{head} chain-entry row: a store `open` refuses had
// already been mutated. This pins that the table stays entirely absent
// and `user_version` is untouched.
#[tokio::test]
async fn at_head_store_with_no_migrations_table_is_refused_without_creating_it() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    let head = chain::head_version(&chain::migrations());
    assert!(
        head > chain::BASELINE_VERSION,
        "this test's premise requires a non-empty real chain, so a table-absent \
         at-head store is never legitimately backfillable"
    );

    // Build a store with today's head DDL and `user_version` stamped
    // straight to head, but no `schema_migrations` table at all.
    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
        schema::create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
        conn.query(&format!("PRAGMA user_version = {head}"), ())
            .await
            .unwrap();
    }

    let before = dump_db(&path).await;
    assert!(
        !before
            .master_rows
            .iter()
            .any(|(_, name, _)| name == "schema_migrations"),
        "precondition: schema_migrations must not exist yet"
    );

    let result = LibsqlDb::open(&path, 4, VectorEncoding::Float32).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::Internal { message, .. }) => {
            assert!(
                message.contains("missing a row"),
                "error should explain the bookkeeping is incomplete: {message}"
            );
        }
        Err(other) => panic!("expected Internal, got: {other:?}"),
        Ok(_) => panic!(
            "expected Internal error: an at-head store with no chain-entry bookkeeping \
             rows must not be silently trusted"
        ),
    }

    assert_eq!(
        before, after,
        "a refused open of a fabricated table-absent at-head store must not mutate it at \
         all"
    );
    assert!(
        !after
            .master_rows
            .iter()
            .any(|(_, name, _)| name == "schema_migrations"),
        "open must not have created schema_migrations while refusing this store: {:?}",
        after.master_rows
    );
    assert_eq!(
        after.user_version, head,
        "user_version must remain exactly as stamped, untouched by the refused open"
    );
}

// Checksum drift: a healthy at-head store whose baseline row's checksum
// has been tampered with must refuse to open (Error::Internal) without
// mutating anything further.
#[tokio::test]
async fn checksum_drift_on_healthy_store_returns_internal_error() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Build then close a healthy at-head store.
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    drop(db);

    // Corrupt the baseline row's checksum directly.
    {
        let raw_db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = raw_db.connect().unwrap();
        conn.execute(
            "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = ?",
            libsql::params![chain::BASELINE_VERSION],
        )
        .await
        .unwrap();
    }

    let before = dump_db(&path).await;
    let result = LibsqlDb::open(&path, 4, VectorEncoding::Float32).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::Internal { message, .. }) => {
            assert!(
                message.contains("checksum mismatch"),
                "error should mention checksum mismatch: {message}"
            );
        }
        Err(other) => panic!("expected Internal, got: {other:?}"),
        Ok(_) => panic!("expected Internal error due to checksum drift, but open succeeded"),
    }

    assert_eq!(
        before, after,
        "a refused open due to checksum drift must not mutate the store"
    );
}

// C3: `AtHead`'s bookkeeping backfill must only apply when
// `schema_migrations` was ABSENT before this open (the raw
// pre-framework case) — if the table already exists but its baseline row
// is missing (corrupt bookkeeping), `open` must refuse via
// `verify_checksums`'s missing-row error, not silently recreate the row
// and let the store pass as healthy.
#[tokio::test]
async fn at_head_open_refuses_and_does_not_backfill_baseline_row_when_table_present_but_row_missing(
) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Build then close a healthy at-head store (schema_migrations table
    // present, baseline + chain rows seeded).
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    drop(db);

    // Corrupt bookkeeping: the table exists, but its baseline row is
    // gone (as opposed to `checksum_drift_on_healthy_store_returns_
    // internal_error` above, which tampers the row's checksum instead of
    // deleting it).
    {
        let raw_db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = raw_db.connect().unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE version = ?",
            libsql::params![chain::BASELINE_VERSION],
        )
        .await
        .unwrap();
    }

    let before = dump_db(&path).await;
    let result = LibsqlDb::open(&path, 4, VectorEncoding::Float32).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::Internal { message, .. }) => {
            assert!(
                message.contains("missing a row"),
                "error should explain the bookkeeping is incomplete: {message}"
            );
            assert!(
                message.contains("baseline"),
                "error should name the missing baseline row: {message}"
            );
        }
        Err(other) => panic!("expected Internal, got: {other:?}"),
        Ok(_) => panic!(
            "expected Internal error: a store whose schema_migrations table exists but \
             whose baseline row is missing must not be silently trusted"
        ),
    }

    assert_eq!(
        before, after,
        "open must not backfill the baseline row (or otherwise mutate the store) when \
         schema_migrations already existed but was missing a required row"
    );
    assert!(
        !after
            .migration_rows
            .iter()
            .any(|r| r.version == chain::BASELINE_VERSION),
        "the baseline row must remain missing, not silently recreated: {:?}",
        after.migration_rows
    );
}
