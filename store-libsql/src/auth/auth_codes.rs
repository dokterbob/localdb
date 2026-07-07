//! Persistence for OAuth2 authorization codes (`auth_codes` table, T4).
//!
//! Mirrors `tokens.rs`'s conventions: parameter binding, error mapping, and
//! row decoding via `sql::row_to_auth_code`.

use localdb_core::auth::AuthCodeRow;
use localdb_core::Error;

use super::sql::row_to_auth_code;
use crate::connection::{map_libsql_err, LibsqlDb};

const AUTH_CODE_COLUMNS: &str = "id, client_id, user_id, code_hash, code_challenge, \
    code_challenge_method, redirect_uri, expires_at, consumed_at, created_at";

pub(crate) async fn create_auth_code(db: &LibsqlDb, code: &AuthCodeRow) -> Result<(), Error> {
    let conn = db.conn().await;
    conn.execute(
        "INSERT INTO auth_codes
            (id, client_id, user_id, code_hash, code_challenge, code_challenge_method,
             redirect_uri, expires_at, consumed_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        libsql::params![
            code.id.clone(),
            code.client_id.clone(),
            code.user_id.clone(),
            code.code_hash.clone(),
            code.code_challenge.clone(),
            code.code_challenge_method.clone(),
            code.redirect_uri.clone(),
            code.expires_at.clone(),
            code.consumed_at.clone(),
            code.created_at.clone(),
        ],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn find_auth_code_by_hash(
    db: &LibsqlDb,
    code_hash: &str,
) -> Result<Option<AuthCodeRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            &format!("SELECT {AUTH_CODE_COLUMNS} FROM auth_codes WHERE code_hash = ?"),
            libsql::params![code_hash.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_auth_code(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn consume_auth_code(
    db: &LibsqlDb,
    id: &str,
    consumed_at: &str,
) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "UPDATE auth_codes SET consumed_at = ? WHERE id = ? AND consumed_at IS NULL",
            libsql::params![consumed_at.to_string(), id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}
