use localdb_core::auth::StoreGrantRow;
use localdb_core::Error;

use super::sql::row_to_store_grant;
use crate::connection::{map_libsql_err, LibsqlDb};

pub(crate) async fn grant_store(db: &LibsqlDb, grant: &StoreGrantRow) -> Result<(), Error> {
    let conn = db.conn().await;
    conn.execute(
        "INSERT INTO store_grants (store_name, user_id, granted_by, created_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(store_name, user_id) DO UPDATE SET
             granted_by = excluded.granted_by,
             created_at = excluded.created_at",
        libsql::params![
            grant.store_name.clone(),
            grant.user_id.clone(),
            grant.granted_by.clone(),
            grant.created_at.clone(),
        ],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn revoke_store_grant(
    db: &LibsqlDb,
    store_name: &str,
    user_id: &str,
) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "DELETE FROM store_grants WHERE store_name = ? AND user_id = ?",
            libsql::params![store_name.to_string(), user_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

pub(crate) async fn list_grants_for_user(
    db: &LibsqlDb,
    user_id: &str,
) -> Result<Vec<StoreGrantRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT store_name, user_id, granted_by, created_at \
             FROM store_grants WHERE user_id = ? ORDER BY created_at",
            libsql::params![user_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_store_grant(&row)?);
    }
    Ok(out)
}

pub(crate) async fn list_grants_for_store(
    db: &LibsqlDb,
    store_name: &str,
) -> Result<Vec<StoreGrantRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT store_name, user_id, granted_by, created_at \
             FROM store_grants WHERE store_name = ? ORDER BY created_at",
            libsql::params![store_name.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_store_grant(&row)?);
    }
    Ok(out)
}
