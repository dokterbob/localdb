//! Auth mutations whose invariants span multiple rows. Policy stays in core.
use localdb_core::auth::{AccessRequestRow, AuthTokenRow, RotateOutcome, StoreGrantRow, UserRow};
use localdb_core::Error;

use super::{
    grants::grant_store_on,
    invites::{create_access_request_on, mark_access_request_collected_on, reserve_invite_use_on},
    sql::row_to_user,
    tokens::insert_token_on,
    users::create_user_on,
};
use crate::connection::{map_libsql_err, LibsqlDb, WriteTx};

pub(super) async fn finish<T>(tx: WriteTx<'_>, result: Result<T, Error>) -> Result<T, Error> {
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

/// Every user name already in use that could collide with `base`'s
/// first-unused-suffix search (`base` itself, plus every `base-N`).
async fn names_colliding_with(
    conn: &libsql::Connection,
    base: &str,
) -> Result<std::collections::HashSet<String>, Error> {
    let mut rows = conn
        .query(
            "SELECT name FROM users WHERE name = ? OR name LIKE ?",
            libsql::params![base.to_string(), format!("{base}-%")],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut taken = std::collections::HashSet::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        let name: String = row.get(0).map_err(map_libsql_err)?;
        taken.insert(name);
    }
    Ok(taken)
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
        let taken = names_colliding_with(&tx, &user.name).await?;
        let name = localdb_core::auth::first_unused_name(&user.name, |n| taken.contains(n));
        let user = UserRow {
            name,
            ..user.clone()
        };
        create_user_on(&tx, &user).await?;
        tx.execute(
            "INSERT INTO pending_bootstrap (singleton, user_id) VALUES (1, ?)",
            [user.id.clone()],
        )
        .await
        .map_err(map_libsql_err)?;
        Ok(user)
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
) -> Result<RotateOutcome, Error> {
    rotate_tokens_at(
        db,
        old_id,
        access,
        refresh,
        &localdb_core::auth::rfc3339_from_now(0),
    )
    .await
}

/// `rotate_tokens` with `now` passed in rather than read from the wall
/// clock, so a test can pin the exact boundary between the row's
/// `expires_at` and the timestamp this call treats as "now" instead of
/// racing the clock across two separate `rfc3339_from_now(0)` calls.
pub(super) async fn rotate_tokens_at(
    db: &LibsqlDb,
    old_id: &str,
    access: &AuthTokenRow,
    refresh: &AuthTokenRow,
    now: &str,
) -> Result<RotateOutcome, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        // `>=`, not `>`: a token expiring in the exact same second as `now`
        // must be eligible here too, matching `core::auth::is_expired`'s
        // own boundary (`expires_at < now`, so `expires_at == now` is NOT
        // expired) — a stricter SQL boundary would reject a row the
        // service already treated as still-valid, reading as reuse.
        let changed = tx
            .execute(
                "UPDATE auth_tokens SET revoked_at = ? WHERE id = ? \
             AND revoked_at IS NULL AND kind = 'refresh' \
             AND (expires_at IS NULL OR expires_at >= ?)",
                libsql::params![now.to_string(), old_id, now.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;
        if changed == 0 {
            return classify_rotation_failure(&tx, old_id, now).await;
        }
        // A compromised ancestor revokes this whole family before marking
        // it, so the conditional revoke above normally fails first. A
        // refused insert still aborts the rotation (rolling back the
        // revoke) rather than committing a revoked token with no
        // replacement.
        if !insert_token_on(&tx, refresh).await? || !insert_token_on(&tx, access).await? {
            return Err(Error::Unauthorized {
                message: "session revoked".to_string(),
            });
        }
        Ok(RotateOutcome::Rotated)
    }
    .await;
    finish(tx, result).await
}

/// Classify why the conditional revoke in `rotate_tokens` affected zero
/// rows: unknown id / wrong kind / expired is `Ineligible` (not reuse); a
/// known, not-expired refresh token that is already revoked is
/// `AlreadyRotated` (reuse).
async fn classify_rotation_failure(
    conn: &libsql::Connection,
    old_id: &str,
    now: &str,
) -> Result<RotateOutcome, Error> {
    let mut rows = conn
        .query(
            "SELECT kind, revoked_at, expires_at FROM auth_tokens WHERE id = ?",
            [old_id],
        )
        .await
        .map_err(map_libsql_err)?;
    let Some(row) = rows.next().await.map_err(map_libsql_err)? else {
        return Ok(RotateOutcome::Ineligible);
    };
    let kind: String = row.get(0).map_err(map_libsql_err)?;
    let revoked_at: Option<String> = row.get(1).map_err(map_libsql_err)?;
    let expires_at: Option<String> = row.get(2).map_err(map_libsql_err)?;
    let expired = expires_at.as_deref().is_some_and(|e| e < now);
    if kind != "refresh" || expired {
        return Ok(RotateOutcome::Ineligible);
    }
    Ok(if revoked_at.is_some() {
        RotateOutcome::AlreadyRotated
    } else {
        // Not expired, right kind, not revoked, yet the UPDATE above
        // affected nothing — shouldn't happen; treat conservatively as no
        // evidence of reuse rather than revoking a family on a guess.
        RotateOutcome::Ineligible
    })
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

/// Commit an `open`-mode invite redemption's entire mint — reserve a use,
/// create `user`, apply `grants`, and insert the freshly minted `credential`
/// — as one transaction: any failure (most commonly `user.name` colliding
/// with `create_user`'s UNIQUE constraint) rolls the reservation back too,
/// so a redemption that doesn't commit never burns a use.
pub(super) async fn redeem_open_invite(
    db: &LibsqlDb,
    invite_id: &str,
    user: &UserRow,
    grants: &[StoreGrantRow],
    credential: &AuthTokenRow,
) -> Result<bool, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        let now = localdb_core::auth::rfc3339_from_now(0);
        if !reserve_invite_use_on(&tx, invite_id, &now).await? {
            return Ok(false);
        }
        create_user_on(&tx, user).await?;
        for grant in grants {
            grant_store_on(&tx, grant).await?;
        }
        if !insert_token_on(&tx, credential).await? {
            return Err(Error::Unauthorized {
                message: "session revoked".to_string(),
            });
        }
        Ok(true)
    }
    .await;
    finish(tx, result).await
}

/// The `closed`-mode counterpart to `redeem_open_invite`: reserve a use and
/// file `request`, committed together.
pub(super) async fn redeem_closed_invite(
    db: &LibsqlDb,
    invite_id: &str,
    request: &AccessRequestRow,
) -> Result<bool, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        let now = localdb_core::auth::rfc3339_from_now(0);
        if !reserve_invite_use_on(&tx, invite_id, &now).await? {
            return Ok(false);
        }
        create_access_request_on(&tx, request).await?;
        Ok(true)
    }
    .await;
    finish(tx, result).await
}

/// Mark an approved access request collected and insert the freshly minted
/// `credential`, committed together: a losing racer's conditional UPDATE
/// affects nothing, so nothing is minted for it either — there is no
/// leftover credential to revoke.
pub(super) async fn collect_access_request(
    db: &LibsqlDb,
    id: &str,
    collected_at: &str,
    credential: &AuthTokenRow,
) -> Result<bool, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        if !mark_access_request_collected_on(&tx, id, collected_at).await? {
            return Ok(false);
        }
        if !insert_token_on(&tx, credential).await? {
            return Err(Error::Unauthorized {
                message: "session revoked".to_string(),
            });
        }
        Ok(true)
    }
    .await;
    finish(tx, result).await
}
