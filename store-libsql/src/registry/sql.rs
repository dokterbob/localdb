//! SQL ↔ Rust converters and row-to-DTO mapping shared by `stores` and
//! `sources` modules.

use localdb_core::types::{SourceKind, StoreVisibility};
use localdb_core::{Error, SourceRow, StoreRow};

use crate::connection::map_libsql_err;

pub(super) fn visibility_to_sql(v: &StoreVisibility) -> &'static str {
    match v {
        StoreVisibility::Private => "private",
        StoreVisibility::Shared => "shared",
    }
}

pub(super) fn visibility_from_sql(s: &str) -> Result<StoreVisibility, Error> {
    match s {
        "private" => Ok(StoreVisibility::Private),
        "shared" => Ok(StoreVisibility::Shared),
        other => Err(Error::Internal {
            message: format!("unknown visibility in DB: {other}"),
            correlation_id: "rt_visibility".to_string(),
        }),
    }
}

pub(super) fn kind_to_sql(k: &SourceKind) -> &'static str {
    match k {
        SourceKind::Path => "path",
        SourceKind::Url => "url",
    }
}

pub(super) fn kind_from_sql(s: &str) -> Result<SourceKind, Error> {
    match s {
        "path" => Ok(SourceKind::Path),
        "url" => Ok(SourceKind::Url),
        other => Err(Error::Internal {
            message: format!("unknown source kind in DB: {other}"),
            correlation_id: "rt_source_kind".to_string(),
        }),
    }
}

pub(super) fn row_to_store(row: &libsql::Row) -> Result<StoreRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let name: String = row.get(1).map_err(map_libsql_err)?;
    let visibility_str: String = row.get(2).map_err(map_libsql_err)?;
    let backend: String = row.get(3).map_err(map_libsql_err)?;
    let indexing_policy: String = row.get(4).map_err(map_libsql_err)?;
    let policy_version: String = row.get(5).map_err(map_libsql_err)?;
    let acl: String = row.get(6).map_err(map_libsql_err)?;
    let created_at: String = row.get(7).map_err(map_libsql_err)?;
    Ok(StoreRow {
        id,
        name,
        visibility: visibility_from_sql(&visibility_str)?,
        backend,
        indexing_policy,
        policy_version,
        acl,
        created_at,
    })
}

pub(super) fn row_to_source(row: &libsql::Row) -> Result<SourceRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let store_id: String = row.get(1).map_err(map_libsql_err)?;
    let kind_str: String = row.get(2).map_err(map_libsql_err)?;
    let root: Option<String> = row.get(3).map_err(map_libsql_err)?;
    let url: Option<String> = row.get(4).map_err(map_libsql_err)?;
    let include_json: String = row.get(5).map_err(map_libsql_err)?;
    let exclude_json: String = row.get(6).map_err(map_libsql_err)?;
    let preset: String = row.get(7).map_err(map_libsql_err)?;
    let refresh: Option<String> = row.get(8).map_err(map_libsql_err)?;
    let created_at: String = row.get(9).map_err(map_libsql_err)?;
    let include: Vec<String> =
        serde_json::from_str(&include_json).map_err(|e| Error::Internal {
            message: format!("invalid source.include JSON: {e}"),
            correlation_id: "rt_source_include_parse".to_string(),
        })?;
    let exclude: Vec<String> =
        serde_json::from_str(&exclude_json).map_err(|e| Error::Internal {
            message: format!("invalid source.exclude JSON: {e}"),
            correlation_id: "rt_source_exclude_parse".to_string(),
        })?;
    Ok(SourceRow {
        id,
        store_id,
        kind: kind_from_sql(&kind_str)?,
        root,
        url,
        include,
        exclude,
        preset,
        refresh,
        created_at,
    })
}
