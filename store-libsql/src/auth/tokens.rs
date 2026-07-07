use localdb_core::auth::AuthTokenRow;
use localdb_core::Error;

use super::sql::{row_to_token, token_kind_to_sql};
use crate::connection::{map_libsql_err, LibsqlDb};

const TOKEN_COLUMNS: &str = "id, user_id, kind, secret_hash, expires_at, last_used_at, \
    revoked_at, created_at, family_id, rotated_from";

pub(crate) async fn insert_token(db: &LibsqlDb, token: &AuthTokenRow) -> Result<(), Error> {
    let conn = db.conn().await;
    conn.execute(
        "INSERT INTO auth_tokens
            (id, user_id, kind, secret_hash, expires_at, last_used_at, revoked_at,
             created_at, family_id, rotated_from)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        libsql::params![
            token.id.clone(),
            token.user_id.clone(),
            token_kind_to_sql(token.kind).to_string(),
            token.secret_hash.clone(),
            token.expires_at.clone(),
            token.last_used_at.clone(),
            token.revoked_at.clone(),
            token.created_at.clone(),
            token.family_id.clone(),
            token.rotated_from.clone(),
        ],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn find_token_by_hash(
    db: &LibsqlDb,
    secret_hash: &str,
) -> Result<Option<AuthTokenRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            &format!("SELECT {TOKEN_COLUMNS} FROM auth_tokens WHERE secret_hash = ?"),
            libsql::params![secret_hash.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_token(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn revoke_token(db: &LibsqlDb, id: &str, revoked_at: &str) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "UPDATE auth_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
            libsql::params![revoked_at.to_string(), id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

pub(crate) async fn revoke_token_family(
    db: &LibsqlDb,
    family_id: &str,
    revoked_at: &str,
) -> Result<u64, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "UPDATE auth_tokens SET revoked_at = ? \
             WHERE family_id = ? AND revoked_at IS NULL",
            libsql::params![revoked_at.to_string(), family_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n as u64)
}

pub(crate) async fn mark_token_used(db: &LibsqlDb, id: &str, used_at: &str) -> Result<(), Error> {
    let conn = db.conn().await;
    conn.execute(
        "UPDATE auth_tokens SET last_used_at = ? WHERE id = ?",
        libsql::params![used_at.to_string(), id.to_string()],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn list_tokens_for_user(
    db: &LibsqlDb,
    user_id: &str,
) -> Result<Vec<AuthTokenRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            &format!(
                "SELECT {TOKEN_COLUMNS} FROM auth_tokens WHERE user_id = ? ORDER BY created_at"
            ),
            libsql::params![user_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_token(&row)?);
    }
    Ok(out)
}
