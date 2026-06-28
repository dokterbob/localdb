use localdb_core::Error;

use localdb_core::SourceRow;

use super::sql::{kind_to_sql, row_to_source};
use crate::connection::{map_libsql_err, LibsqlDb};

pub(crate) async fn upsert_source(db: &LibsqlDb, source: &SourceRow) -> Result<(), Error> {
    let conn = db.conn().await;
    let include_json = serde_json::to_string(&source.include).map_err(|e| Error::Internal {
        message: format!("source include serialize: {e}"),
        correlation_id: "rt_source_include".to_string(),
    })?;
    let exclude_json = serde_json::to_string(&source.exclude).map_err(|e| Error::Internal {
        message: format!("source exclude serialize: {e}"),
        correlation_id: "rt_source_exclude".to_string(),
    })?;
    conn.execute(
        "INSERT INTO sources (id, store_id, kind, root, url, include, exclude,
                preset, refresh, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 store_id = excluded.store_id,
                 kind = excluded.kind,
                 root = excluded.root,
                 url = excluded.url,
                 include = excluded.include,
                 exclude = excluded.exclude,
                 preset = excluded.preset,
                 refresh = excluded.refresh",
        libsql::params![
            source.id.clone(),
            source.store_id.clone(),
            kind_to_sql(&source.kind).to_string(),
            source.root.clone(),
            source.url.clone(),
            include_json,
            exclude_json,
            source.preset.clone(),
            source.refresh.clone(),
            source.created_at.clone(),
        ],
    )
    .await
    .map_err(|e| {
        let msg = format!("{e}");
        if msg.contains("UNIQUE constraint failed") {
            Error::InvalidRequest {
                message: "a source with the same path or URL already exists in this store".into(),
            }
        } else {
            map_libsql_err(e)
        }
    })?;
    Ok(())
}

pub(crate) async fn delete_source(db: &LibsqlDb, id: &str) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "DELETE FROM sources WHERE id = ?",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

#[cfg(test)]
pub(crate) async fn delete_sources_for_store(db: &LibsqlDb, store_id: &str) -> Result<u64, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "DELETE FROM sources WHERE store_id = ?",
            libsql::params![store_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n)
}

pub(crate) async fn get_source(db: &LibsqlDb, id: &str) -> Result<Option<SourceRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, store_id, kind, root, url, include, exclude, preset, refresh, created_at
                 FROM sources WHERE id = ?",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_source(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn list_sources(db: &LibsqlDb, store_id: &str) -> Result<Vec<SourceRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, store_id, kind, root, url, include, exclude, preset, refresh, created_at
                 FROM sources WHERE store_id = ? ORDER BY created_at",
            libsql::params![store_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_source(&row)?);
    }
    Ok(out)
}

pub(crate) async fn find_source_by_root_or_url(
    db: &LibsqlDb,
    value: &str,
    store_id: &str,
) -> Result<Option<SourceRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, store_id, kind, root, url, include, exclude, preset, refresh, created_at
                 FROM sources WHERE store_id = ? AND (root = ? OR url = ?) LIMIT 1",
            libsql::params![store_id.to_string(), value.to_string(), value.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_source(&row).map(Some),
        None => Ok(None),
    }
}
