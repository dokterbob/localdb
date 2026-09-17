//! Auth mutations whose invariants span multiple rows. Policy stays in core.
use localdb_core::auth::{AuthTokenRow, StoreGrantRow, UserRow};
use localdb_core::Error;

use super::{
    grants::grant_store_on, sql::row_to_user, tokens::insert_token_on, users::create_user_on,
};
use crate::connection::{map_libsql_err, LibsqlDb, WriteTx};

async fn finish<T>(tx: WriteTx<'_>, result: Result<T, Error>) -> Result<T, Error> {
    match result {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

pub(super) async fn pending_bootstrap_user(
    conn: &libsql::Connection,
) -> Result<Option<UserRow>, Error> {
    let mut rows = conn
        .query(
            "SELECT u.id, u.name, u.role, u.created_at FROM users u \
         JOIN pending_bootstrap p ON p.user_id = u.id WHERE u.role = 'admin'",
            (),
        )
        .await
        .map_err(map_libsql_err)?;
    rows.next()
        .await
        .map_err(map_libsql_err)?
        .map(|row| row_to_user(&row))
        .transpose()
}

pub(super) async fn begin_bootstrap(db: &LibsqlDb, user: &UserRow) -> Result<UserRow, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        if let Some(pending) = pending_bootstrap_user(&tx).await? {
            return Ok(pending);
        }
        let mut admins = tx
            .query("SELECT 1 FROM users WHERE role = 'admin' LIMIT 1", ())
            .await
            .map_err(map_libsql_err)?;
        if admins.next().await.map_err(map_libsql_err)?.is_some() {
            return Err(Error::Unauthorized {
                message: "setup is already complete".into(),
            });
        }
        create_user_on(&tx, user).await?;
        tx.execute(
            "INSERT INTO pending_bootstrap (singleton, user_id) VALUES (1, ?)",
            [user.id.clone()],
        )
        .await
        .map_err(map_libsql_err)?;
        Ok(user.clone())
    }
    .await;
    finish(tx, result).await
}

pub(super) async fn complete_bootstrap(db: &LibsqlDb, user_id: &str) -> Result<(), Error> {
    db.writer()
        .await
        .execute("DELETE FROM pending_bootstrap WHERE user_id = ?", [user_id])
        .await
        .map_err(map_libsql_err)?;
    Ok(())
}

pub(super) async fn rotate_tokens(
    db: &LibsqlDb,
    old_id: &str,
    access: &AuthTokenRow,
    refresh: &AuthTokenRow,
) -> Result<bool, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        let now = localdb_core::auth::rfc3339_from_now(0);
        let changed = tx
            .execute(
                "UPDATE auth_tokens SET revoked_at = ? WHERE id = ? \
             AND revoked_at IS NULL AND kind = 'refresh' \
             AND (expires_at IS NULL OR expires_at > ?)",
                libsql::params![now.clone(), old_id, now],
            )
            .await
            .map_err(map_libsql_err)?;
        if changed == 0 {
            return Ok(false);
        }
        insert_token_on(&tx, refresh).await?;
        insert_token_on(&tx, access).await?;
        Ok(true)
    }
    .await;
    finish(tx, result).await
}

pub(super) async fn approve_access_request(
    db: &LibsqlDb,
    id: &str,
    user: &UserRow,
    grants: &[StoreGrantRow],
) -> Result<bool, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        let mut rows = tx
            .query(
                "SELECT 1 FROM access_requests WHERE id = ? AND state = 'pending'",
                [id],
            )
            .await
            .map_err(map_libsql_err)?;
        if rows.next().await.map_err(map_libsql_err)?.is_none() {
            return Ok(false);
        }
        create_user_on(&tx, user).await?;
        for grant in grants {
            grant_store_on(&tx, grant).await?;
        }
        tx.execute(
            "UPDATE access_requests SET state = 'approved', resulting_user_id = ?, \
             decided_at = ? WHERE id = ? AND state = 'pending'",
            libsql::params![user.id.clone(), user.created_at.clone(), id],
        )
        .await
        .map_err(map_libsql_err)?;
        Ok(true)
    }
    .await;
    finish(tx, result).await
}
