use localdb_core::auth::{AccessRequestRow, AccessRequestState, InviteRow};
use localdb_core::Error;

use super::sql::{
    access_request_state_to_sql, invite_mode_to_sql, row_to_access_request, row_to_invite,
};
use crate::connection::{map_libsql_err, LibsqlDb};

const INVITE_COLUMNS: &str = "id, token_hash, mode, store_grants, max_uses, uses, \
    expires_at, revoked_at, created_by, created_at";

pub(crate) async fn create_invite(db: &LibsqlDb, invite: &InviteRow) -> Result<(), Error> {
    let conn = db.writer().await;
    let store_grants_json =
        serde_json::to_string(&invite.store_grants).map_err(|e| Error::Internal {
            message: format!("failed to serialize invite.store_grants: {e}"),
            correlation_id: "auth_invite_store_grants_serialize".to_string(),
        })?;
    conn.execute(
        &format!("INSERT INTO invites ({INVITE_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"),
        libsql::params![
            invite.id.clone(),
            invite.token_hash.clone(),
            invite_mode_to_sql(invite.mode).to_string(),
            store_grants_json,
            invite.max_uses as i64,
            invite.uses as i64,
            invite.expires_at.clone(),
            invite.revoked_at.clone(),
            invite.created_by.clone(),
            invite.created_at.clone(),
        ],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn find_invite_by_hash(
    db: &LibsqlDb,
    token_hash: &str,
) -> Result<Option<InviteRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!("SELECT {INVITE_COLUMNS} FROM invites WHERE token_hash = ?"),
            libsql::params![token_hash.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_invite(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn find_invite(db: &LibsqlDb, id: &str) -> Result<Option<InviteRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!("SELECT {INVITE_COLUMNS} FROM invites WHERE id = ?"),
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_invite(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn list_invites(db: &LibsqlDb) -> Result<Vec<InviteRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!("SELECT {INVITE_COLUMNS} FROM invites ORDER BY created_at"),
            (),
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_invite(&row)?);
    }
    Ok(out)
}

pub(crate) async fn revoke_invite(
    db: &LibsqlDb,
    id: &str,
    revoked_at: &str,
) -> Result<bool, Error> {
    let conn = db.writer().await;
    let n = conn
        .execute(
            "UPDATE invites SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
            libsql::params![revoked_at.to_string(), id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

/// Atomic "reserve a use iff eligible and one remains" (mirrors
/// `collect_access_request`'s and `consume_auth_code`'s
/// single-condition-in-the-WHERE-clause convention): a single UPDATE with
/// the eligibility check baked into the WHERE clause, so concurrent callers
/// racing the same invite can never together push `uses` past `max_uses`.
/// Shared by `redeem_open_invite`/`redeem_closed_invite` (`atomic.rs`), which
/// run it as the first statement of their own transaction — a failure later
/// in that transaction rolls this reservation back too, so there is no
/// separate "release" step.
///
/// `>=`, not `>`: an invite expiring in the exact same second as `now` must
/// still be eligible, matching `core::auth::is_expired`'s own boundary
/// (`expires_at < now`, so `expires_at == now` is NOT expired) — the fake
/// store's `invite_use_index_if_eligible` applies the same `is_expired`
/// call, so a stricter SQL boundary here would refuse an invite the fake
/// store (and the service layer) still treat as live.
pub(super) async fn reserve_invite_use_on(
    conn: &libsql::Connection,
    invite_id: &str,
    now: &str,
) -> Result<bool, Error> {
    let n = conn
        .execute(
            "UPDATE invites SET uses = uses + 1 WHERE id = ? AND uses < max_uses \
             AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at >= ?)",
            libsql::params![invite_id.to_string(), now.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

const ACCESS_REQUEST_COLUMNS: &str = "id, invite_id, requested_name, secret_hash, state, \
    resulting_user_id, created_at, decided_at, collected_at";

pub(crate) async fn create_access_request(
    db: &LibsqlDb,
    req: &AccessRequestRow,
) -> Result<(), Error> {
    let conn = db.writer().await;
    create_access_request_on(&conn, req).await
}

pub(super) async fn create_access_request_on(
    conn: &libsql::Connection,
    req: &AccessRequestRow,
) -> Result<(), Error> {
    conn.execute(
        &format!(
            "INSERT INTO access_requests ({ACCESS_REQUEST_COLUMNS}) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ),
        libsql::params![
            req.id.clone(),
            req.invite_id.clone(),
            req.requested_name.clone(),
            req.secret_hash.clone(),
            access_request_state_to_sql(req.state).to_string(),
            req.resulting_user_id.clone(),
            req.created_at.clone(),
            req.decided_at.clone(),
            req.collected_at.clone(),
        ],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn find_access_request(
    db: &LibsqlDb,
    id: &str,
) -> Result<Option<AccessRequestRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!("SELECT {ACCESS_REQUEST_COLUMNS} FROM access_requests WHERE id = ?"),
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_access_request(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn list_access_requests_for_invite(
    db: &LibsqlDb,
    invite_id: &str,
) -> Result<Vec<AccessRequestRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!(
                "SELECT {ACCESS_REQUEST_COLUMNS} FROM access_requests \
                 WHERE invite_id = ? ORDER BY created_at"
            ),
            libsql::params![invite_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_access_request(&row)?);
    }
    Ok(out)
}

pub(crate) async fn list_access_requests(db: &LibsqlDb) -> Result<Vec<AccessRequestRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!("SELECT {ACCESS_REQUEST_COLUMNS} FROM access_requests ORDER BY created_at"),
            (),
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_access_request(&row)?);
    }
    Ok(out)
}

/// Atomic "mark collected iff currently `approved` and not yet collected"
/// (mirrors `consume_auth_code`'s single-use guard): a single UPDATE with
/// both conditions in the WHERE clause, so two concurrent callers can never
/// both observe success. Shared by `collect_access_request` (`atomic.rs`),
/// which runs it alongside the credential insert in one transaction.
pub(super) async fn mark_access_request_collected_on(
    conn: &libsql::Connection,
    id: &str,
    collected_at: &str,
) -> Result<bool, Error> {
    let n = conn
        .execute(
            "UPDATE access_requests SET collected_at = ? \
             WHERE id = ? AND state = 'approved' AND collected_at IS NULL",
            libsql::params![collected_at.to_string(), id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

/// Atomic "decide iff currently pending": the transition to a terminal
/// `state` (`approved`/`denied`) is conditioned on `state = 'pending'` in
/// the same UPDATE, the same single-condition-in-the-WHERE-clause
/// convention as `mark_access_request_collected_on`/`reserve_invite_use_on`
/// — so two concurrent approve/deny calls on the same request can never
/// both take effect (last-writer-wins overwrite is impossible; the second
/// racer's UPDATE simply affects zero rows).
pub(crate) async fn try_decide_access_request(
    db: &LibsqlDb,
    id: &str,
    state: AccessRequestState,
    resulting_user_id: Option<&str>,
    decided_at: &str,
) -> Result<bool, Error> {
    let conn = db.writer().await;
    let n = conn
        .execute(
            "UPDATE access_requests \
             SET state = ?, resulting_user_id = ?, decided_at = ? \
             WHERE id = ? AND state = 'pending'",
            libsql::params![
                access_request_state_to_sql(state).to_string(),
                resulting_user_id.map(|s| s.to_string()),
                decided_at.to_string(),
                id.to_string(),
            ],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}
