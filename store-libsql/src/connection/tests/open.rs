//! `LibsqlDb::open` basic behavior: creation, reopening, legacy-layout
//! refusal, and fresh-store seeding.

use tempfile::tempdir;

use localdb_core::{Error, VectorEncoding};

use crate::connection::LibsqlDb;
use crate::migrations::{chain, table};
use crate::schema;

#[tokio::test]
async fn open_creates_new_db() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    assert!(!path.exists());
    let _db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    assert!(path.exists(), "DB file should be created on open");
}

#[tokio::test]
async fn open_creates_parent_directory() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("subdir").join("nested").join("localdb.db");
    let _db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    assert!(
        path.exists(),
        "DB file should be created in new directories"
    );
}

#[tokio::test]
async fn second_open_succeeds_on_existing_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let _db1 = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let _db2 = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
}

#[tokio::test]
async fn refuses_to_open_with_legacy_stores_dir() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("stores").join("notes")).unwrap();
    let result = LibsqlDb::open(&dir.path().join("localdb.db"), 4, VectorEncoding::Float32).await;
    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(message.contains("legacy") || message.contains("stores"));
        }
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!("expected InvalidConfig"),
    }
}

#[tokio::test]
async fn fresh_db_and_reopen_both_succeed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32)
        .await
        .unwrap();
    LibsqlDb::open(&path, 4, localdb_core::VectorEncoding::Float32)
        .await
        .unwrap();
}

// Plan test 13: a brand-new store created via `LibsqlDb::open` seeds
// exactly one bookkeeping row per real chain entry plus the baseline row,
// and stamps user_version to head.
#[tokio::test]
async fn fresh_open_seeds_baseline_plus_chain_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");

    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let conn = db.conn().await;

    let user_version = schema::get_schema_version(&conn).await.unwrap();
    assert_eq!(user_version, chain::head_version(&chain::migrations()));
    assert_eq!(
        user_version,
        chain::BASELINE_VERSION + chain::migrations().len() as i64,
        "head == baseline + the real chain's length"
    );

    let rows = table::list_rows_desc_above(&conn, i64::MIN).await.unwrap();
    assert_eq!(
        rows.len(),
        1 + chain::migrations().len(),
        "baseline row plus one row per real chain entry should exist"
    );
    assert!(
        rows.iter()
            .any(|r| r.version == chain::BASELINE_VERSION && r.name == "baseline"),
        "baseline row missing: {rows:?}"
    );
}

// Codex review #152 fix 1: a database that only got as far as
// `create_schema` (no seeding, no stamp — simulating a crash between
// `create_schema` finishing and `seed_for_fresh_create` committing) must
// still open successfully: `user_version` is still 0, so `open`
// classifies it as `Fresh` and re-runs `create_schema` (idempotent) plus
// seeding, landing at head with the baseline row present.
#[tokio::test]
async fn crash_before_seeding_reclassifies_as_fresh_and_recovers_on_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Simulate the interrupted fresh-create: run create_schema directly,
    // bypassing LibsqlDb::open, and never seed schema_migrations or stamp
    // user_version — exactly what create_schema alone now leaves behind.
    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
        schema::create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
    }

    // Confirm the simulated crash point: schema exists, but user_version
    // is still 0.
    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(
            schema::get_schema_version(&conn).await.unwrap(),
            0,
            "create_schema alone must not stamp user_version"
        );
    }

    // Reopening via the normal path must succeed — classified as Fresh —
    // and end up healthy at head with the baseline row present.
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let conn = db.conn().await;

    let v = schema::get_schema_version(&conn).await.unwrap();
    assert_eq!(v, chain::head_version(&chain::migrations()));

    let rows = table::list_rows_desc_above(&conn, i64::MIN).await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.version == chain::BASELINE_VERSION && r.name == "baseline"),
        "recovered store should have the baseline row: {rows:?}"
    );
}
