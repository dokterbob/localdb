//! `validate_embedding_column` and the reopen-time embedding
//! dim/encoding-mismatch refusals that depend on it.

use tempfile::tempdir;

use localdb_core::{Error, VectorEncoding};

use super::common::dump_db;
use crate::connection::LibsqlDb;
use crate::schema;

#[tokio::test]
async fn open_rejects_encoding_mismatch_on_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");

    // Open as Float32
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    drop(db);

    // Reopen as Binary — should fail with InvalidConfig
    let result = LibsqlDb::open(&path, 4, VectorEncoding::Binary).await;
    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("mismatch"),
                "error should mention mismatch: {message}"
            );
        }
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!("expected InvalidConfig, but reopen succeeded"),
    }
}

/// The encoding-mismatch refusal must not be masked by migration-checksum
/// verification.
///
/// Since schema v6 (`chain::shrink_vector_index_up`) a migration's
/// rendered SQL depends on `ctx.encoding`, so its stored checksum does
/// too. Reopening a Float32 store as Binary therefore *also* trips a
/// checksum mismatch — and if `verify_checksums` runs first, the user gets
/// an `Internal` "migration drift" error blaming corrupt bookkeeping
/// instead of the `InvalidConfig` that names the actual problem. This pins
/// the ordering in `LibsqlDb::open`'s `AtHead` branch.
#[tokio::test]
async fn encoding_mismatch_beats_migration_checksum_mismatch() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");

    // 1024 dims: the production default, and the shape whose v6 up-SQL
    // actually differs between the two encodings.
    let db = LibsqlDb::open(&path, 1024, VectorEncoding::Binary)
        .await
        .unwrap();
    drop(db);

    match LibsqlDb::open(&path, 1024, VectorEncoding::Float32).await {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("embedding schema mismatch"),
                "must name the embedding mismatch, not migration drift: {message}"
            );
        }
        Err(other) => panic!(
            "expected InvalidConfig naming the embedding mismatch; \
             got {other:?} — checksum verification is running first again"
        ),
        Ok(_) => panic!("expected InvalidConfig, but reopen succeeded"),
    }
}

#[tokio::test]
async fn open_rejects_dim_mismatch_on_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");

    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    drop(db);

    match LibsqlDb::open(&path, 8, VectorEncoding::Float32).await {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("mismatch"),
                "error should mention mismatch: {message}"
            );
        }
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!("expected InvalidConfig, but reopen with different dim succeeded"),
    }
}

// C1: same latent bug as migrate.rs's v0 branch, but in `open`'s `Fresh`
// disposition — a store that only got as far as `create_schema` (chunks
// built with dim 4, user_version still 0, simulating an interrupted
// earlier fresh-create) must be refused when reopened with a mismatched
// dim, and refused BEFORE `seed_for_fresh_create` stamps user_version to
// head — not stamped-then-rejected on the next open.
#[tokio::test]
async fn open_refuses_and_leaves_store_unstamped_on_fresh_create_recovery_dim_mismatch() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");

    {
        let db = libsql::Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
        schema::create_schema(&conn, 4, VectorEncoding::Float32)
            .await
            .unwrap();
    }

    let before = dump_db(&path).await;
    let result = LibsqlDb::open(&path, 8, VectorEncoding::Float32).await;
    let after = dump_db(&path).await;

    match result {
        Err(Error::InvalidConfig { message }) => {
            assert!(
                message.contains("mismatch"),
                "error should mention mismatch: {message}"
            );
        }
        Err(other) => panic!("expected InvalidConfig, got: {other:?}"),
        Ok(_) => panic!("expected InvalidConfig, but reopen with mismatched dim succeeded"),
    }

    assert_eq!(
        before, after,
        "a refused fresh-create recovery due to an embedding shape mismatch must not \
         mutate the store — user_version must remain 0 and no schema_migrations rows \
         may be written"
    );
    assert_eq!(after.user_version, 0, "must remain unstamped at v0");
    assert!(
        after.migration_rows.is_empty(),
        "no schema_migrations rows should have been written: {:?}",
        after.migration_rows
    );
}
