//! Store-table CRUD on `RuntimeStateApi`.

use localdb_core::Error;

use crate::db::map_libsql_err;

use super::rows::StoreRow;
use super::sql::{row_to_store, visibility_to_sql};
use super::RuntimeStateApi;

impl RuntimeStateApi {
    pub async fn upsert_store(&self, store: &StoreRow) -> Result<(), Error> {
        let conn = self.db.conn().await;
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

    pub async fn delete_store(&self, id: &str) -> Result<bool, Error> {
        let conn = self.db.conn().await;
        let n = conn
            .execute(
                "DELETE FROM stores WHERE id = ?",
                libsql::params![id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;
        Ok(n > 0)
    }

    pub async fn get_store(&self, id: &str) -> Result<Option<StoreRow>, Error> {
        let conn = self.db.conn().await;
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

    pub async fn get_store_by_name(&self, name: &str) -> Result<Option<StoreRow>, Error> {
        let conn = self.db.conn().await;
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

    pub async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
        let conn = self.db.conn().await;
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
}
