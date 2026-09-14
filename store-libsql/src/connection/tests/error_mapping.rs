//! `map_libsql_err`: mapping libsql errors onto the crate's error taxonomy.

use localdb_core::Error;

use crate::connection::map_libsql_err;

#[tokio::test]
async fn map_libsql_err_lock_strings_become_runtime_state_locked() {
    let busy = libsql::Error::SqliteFailure(5, "database is locked".to_string());
    assert!(matches!(map_libsql_err(busy), Error::RuntimeStateLocked));

    let busy2 = libsql::Error::SqliteFailure(5, "SQLITE_BUSY: writer".to_string());
    assert!(matches!(map_libsql_err(busy2), Error::RuntimeStateLocked));
}

#[tokio::test]
async fn map_libsql_err_other_becomes_internal() {
    let other = libsql::Error::SqliteFailure(1, "no such table: foo".to_string());
    match map_libsql_err(other) {
        Error::Internal { message, .. } => {
            assert!(message.contains("no such table"));
        }
        e => panic!("expected Internal, got {e:?}"),
    }
}
