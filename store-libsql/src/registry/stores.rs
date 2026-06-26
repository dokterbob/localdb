use localdb_core::Error;

use localdb_core::StoreRow;

use super::sql::{row_to_store, visibility_to_sql};
use crate::connection::{map_libsql_err, LibsqlDb};

pub(crate) async fn upsert_store(db: &LibsqlDb, store: &StoreRow) -> Result<(), Error> {
    let conn = db.conn().await;
    conn.execute(
        "INSERT INTO stores (id, name, visibility, backend, indexing_policy,
                policy_version, acl, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 visibility = excluded.visibility,
                 backend = excluded.backend,
                 indexing_policy = excluded.indexing_policy,
                 policy_version = excluded.policy_version,
                 acl = excluded.acl",
        libsql::params![
            store.id.clone(),
            store.name.clone(),
            visibility_to_sql(&store.visibility).to_string(),
            store.backend.clone(),
            store.indexing_policy.clone(),
            store.policy_version.clone(),
            store.acl.clone(),
            store.created_at.clone(),
        ],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn delete_store(db: &LibsqlDb, id: &str) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "DELETE FROM stores WHERE id = ?",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

pub(crate) async fn get_store(db: &LibsqlDb, id: &str) -> Result<Option<StoreRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, name, visibility, backend, indexing_policy,
                        policy_version, acl, created_at
                 FROM stores WHERE id = ?",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_store(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn get_store_by_name(
    db: &LibsqlDb,
    name: &str,
) -> Result<Option<StoreRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, name, visibility, backend, indexing_policy,
                        policy_version, acl, created_at
                 FROM stores WHERE name = ?",
            libsql::params![name.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_store(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn list_stores(db: &LibsqlDb) -> Result<Vec<StoreRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, name, visibility, backend, indexing_policy,
                        policy_version, acl, created_at
                 FROM stores ORDER BY name",
            (),
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_store(&row)?);
    }
    Ok(out)
}
