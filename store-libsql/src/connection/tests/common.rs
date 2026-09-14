//! Shared test helpers for connection tests.

use crate::migrations::table;
use crate::schema;

/// Everything about a database file's on-disk schema state that `open`
/// must leave untouched when it refuses a version-mismatched store:
/// `sqlite_master`'s DDL rows, `PRAGMA user_version`, and — if the
/// bookkeeping table happens to exist — its rows. Used to prove several
/// refusal branches are pure reads.
#[derive(Debug, PartialEq)]
pub(in crate::connection) struct DbDump {
    pub(in crate::connection) master_rows: Vec<(String, String, String)>,
    pub(in crate::connection) user_version: i64,
    pub(in crate::connection) migration_rows: Vec<table::MigrationRow>,
}

pub(in crate::connection) async fn dump_db(path: &std::path::Path) -> DbDump {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();

    let mut rows = conn
        .query(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            (),
        )
        .await
        .unwrap();
    let mut master_rows = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        master_rows.push((
            row.get::<String>(0).unwrap(),
            row.get::<String>(1).unwrap(),
            row.get::<String>(2).unwrap(),
        ));
    }

    let user_version = schema::get_schema_version(&conn).await.unwrap();

    let mut exists = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            (),
        )
        .await
        .unwrap();
    let migration_rows = if exists.next().await.unwrap().is_some() {
        table::list_rows_desc_above(&conn, i64::MIN).await.unwrap()
    } else {
        Vec::new()
    };

    DbDump {
        master_rows,
        user_version,
        migration_rows,
    }
}
