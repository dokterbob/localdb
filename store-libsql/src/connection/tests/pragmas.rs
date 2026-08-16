//! PRAGMA settings applied by `LibsqlDb::open`: `foreign_keys` and
//! `journal_mode`.

use tempfile::tempdir;

use localdb_core::VectorEncoding;

use crate::connection::LibsqlDb;

#[tokio::test]
async fn foreign_keys_pragma_is_enabled() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let conn = db.reader();
    let mut rows = conn.query("PRAGMA foreign_keys", ()).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let on: i64 = row.get(0).unwrap();
    assert_eq!(on, 1, "PRAGMA foreign_keys should be ON after open");
}

#[tokio::test]
async fn wal_pragma_is_enabled() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let db = LibsqlDb::open(&path, 4, VectorEncoding::Float32)
        .await
        .unwrap();
    let conn = db.reader();
    let mut rows = conn.query("PRAGMA journal_mode", ()).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let mode: String = row.get(0).unwrap();
    assert_eq!(
        mode.to_ascii_lowercase(),
        "wal",
        "journal_mode should be WAL after open"
    );
}
