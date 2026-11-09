use localdb_core::auth::AuthTokenRow;
use localdb_core::Error;

use super::atomic::finish;
use super::sql::{row_to_token, token_kind_to_sql};
use crate::connection::{map_libsql_err, LibsqlDb};

const TOKEN_COLUMNS: &str = "id, user_id, kind, secret_hash, expires_at, last_used_at, \
    revoked_at, created_at, family_id, rotated_from, minted_by_family";

/// Insert `token`, refusing with `Error::Unauthorized` — persisting
/// nothing — if its `minted_by_family` names a family already recorded in
/// `auth_compromised_families` (see [`insert_token_on`]).
pub(crate) async fn insert_token(db: &LibsqlDb, token: &AuthTokenRow) -> Result<(), Error> {
    let conn = db.writer().await;
    if insert_token_on(&conn, token).await? {
        Ok(())
    } else {
        Err(Error::Unauthorized {
            message: "session revoked".to_string(),
        })
    }
}

/// Insert `token`, reporting whether it actually happened: `false` means
/// `token.minted_by_family` named a family already in
/// `auth_compromised_families` and nothing was persisted — the atomic guard
/// against minting a key or session from a credential whose session is
/// revoked concurrently (`revoke_compromised_family`) or between an OAuth
/// authorization and the token exchange that redeems it. A `NULL`
/// `minted_by_family` never matches any row in that table, so an untainted
/// token always inserts.
pub(super) async fn insert_token_on(
    conn: &libsql::Connection,
    token: &AuthTokenRow,
) -> Result<bool, Error> {
    let n = conn
        .execute(
            "INSERT INTO auth_tokens
                (id, user_id, kind, secret_hash, expires_at, last_used_at, revoked_at,
                 created_at, family_id, rotated_from, minted_by_family)
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
             WHERE NOT EXISTS (
                 SELECT 1 FROM auth_compromised_families WHERE family_id = ?
             )",
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
                token.minted_by_family.clone(),
                token.minted_by_family.clone(),
            ],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

pub(crate) async fn find_token_by_hash(
    db: &LibsqlDb,
    secret_hash: &str,
) -> Result<Option<AuthTokenRow>, Error> {
    let conn = db.reader();
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

pub(crate) async fn find_token(db: &LibsqlDb, id: &str) -> Result<Option<AuthTokenRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!("SELECT {TOKEN_COLUMNS} FROM auth_tokens WHERE id = ?"),
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_token(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn revoke_token(db: &LibsqlDb, id: &str, revoked_at: &str) -> Result<bool, Error> {
    let conn = db.writer().await;
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
    let conn = db.writer().await;
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

/// The transitive compromise closure of a family: itself, plus the
/// `family_id` of every token whose `minted_by_family` is already in the
/// closure — a session created by authorizing with another session's
/// credential, repeated down any chain of such sessions. Re-stated in each
/// statement that needs it (a `WITH RECURSIVE` clause scopes to one
/// statement in SQLite, so it can't be shared across the two below), but
/// both see the same, fully-committed `auth_tokens` state: both run inside
/// one write transaction, which this connection serializes against every
/// other writer.
const CLOSURE_CTE: &str = "WITH RECURSIVE closure(family_id) AS ( \
    SELECT ? \
    UNION \
    SELECT t.family_id FROM auth_tokens t \
    JOIN closure c ON t.minted_by_family = c.family_id \
    WHERE t.family_id IS NOT NULL \
)";

pub(crate) async fn revoke_compromised_family(
    db: &LibsqlDb,
    family_id: &str,
    revoked_at: &str,
) -> Result<u64, Error> {
    let tx = db.write_tx().await?;
    let result: Result<u64, Error> = async {
        // Durably mark every closure member compromised *before* revoking
        // their rows, so a token insert racing this same transaction's
        // commit (necessarily serialized after it, on this single writer)
        // is refused by `insert_token_on` rather than slipping in once the
        // sweep has already passed it by.
        tx.execute(
            &format!(
                "{CLOSURE_CTE} \
                 INSERT OR IGNORE INTO auth_compromised_families (family_id, compromised_at) \
                 SELECT family_id, ? FROM closure"
            ),
            libsql::params![family_id.to_string(), revoked_at.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;

        let n = tx
            .execute(
                &format!(
                    "{CLOSURE_CTE} \
                     UPDATE auth_tokens SET revoked_at = ? \
                     WHERE revoked_at IS NULL \
                       AND (family_id IN (SELECT family_id FROM closure) \
                            OR minted_by_family IN (SELECT family_id FROM closure))"
                ),
                libsql::params![family_id.to_string(), revoked_at.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;
        Ok(n as u64)
    }
    .await;
    finish(tx, result).await
}

pub(crate) async fn mark_token_used(db: &LibsqlDb, id: &str, used_at: &str) -> Result<(), Error> {
    let conn = db.writer().await;
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
    let conn = db.reader();
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
