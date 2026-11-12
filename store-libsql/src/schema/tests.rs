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
        "idx_chunks_store_resource_pos",
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

/// `create_schema` alone must NOT stamp `user_version` — only
/// `runner::seed_for_fresh_create` does, as the last step of its own
/// seeding transaction (see `create_schema`'s doc comment for why the
/// ordering matters: it's what makes an interruption between the two
/// re-classify as `Fresh` on the next open, instead of landing at "head
/// but missing bookkeeping rows").
#[tokio::test]
async fn create_schema_leaves_user_version_untouched() {
    let (_dir, conn) = open_test_db().await;
    create_schema(&conn, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let v = get_schema_version(&conn).await.unwrap();
    assert_eq!(
        v, 0,
        "create_schema must not stamp user_version; seeding does"
    );
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

/// drop_all_tables leaves the DB empty and with user_version=0.
#[tokio::test]
async fn drop_all_tables_resets_db() {
    let (_dir, conn) = open_test_db().await;
    create_schema(&conn, 4, VectorEncoding::Float32)
        .await
        .unwrap();

    drop_all_tables(&conn).await.unwrap();

    // No user tables remain.
    let names = table_names(&conn).await;
    assert!(
        names.is_empty(),
        "all tables should be dropped; remaining: {names:?}"
    );

    // user_version is reset.
    let v = get_schema_version(&conn).await.unwrap();
    assert_eq!(v, 0, "user_version should be 0 after drop_all_tables");
}

/// After drop_all_tables, create_schema succeeds again (full
/// reinitialisation). `user_version` stays at 0 throughout — neither
/// `drop_all_tables` (it resets to 0) nor `create_schema` (it never
/// stamps) touches it; only seeding would.
#[tokio::test]
async fn drop_and_recreate_schema_succeeds() {
    let (_dir, conn) = open_test_db().await;
    create_schema(&conn, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    drop_all_tables(&conn).await.unwrap();
    create_schema(&conn, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let v = get_schema_version(&conn).await.unwrap();
    assert_eq!(
        v, 0,
        "create_schema alone never stamps user_version, even after a drop+recreate cycle"
    );
}
