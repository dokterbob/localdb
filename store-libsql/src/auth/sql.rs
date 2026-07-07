//! SQL <-> Rust converters for the auth tables (mirrors `registry/sql.rs`).

use localdb_core::auth::{
    AccessRequestRow, AccessRequestState, AuthCodeRow, AuthTokenRow, InviteMode, InviteRow, Role,
    StoreGrantRow, TokenKind, UserRow,
};
use localdb_core::Error;

use crate::connection::map_libsql_err;

pub(super) fn role_to_sql(r: Role) -> &'static str {
    match r {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

pub(super) fn role_from_sql(s: &str) -> Result<Role, Error> {
    match s {
        "admin" => Ok(Role::Admin),
        "member" => Ok(Role::Member),
        other => Err(Error::Internal {
            message: format!("unknown role in DB: {other}"),
            correlation_id: "auth_role".to_string(),
        }),
    }
}

pub(super) fn token_kind_to_sql(k: TokenKind) -> &'static str {
    match k {
        TokenKind::Access => "access",
        TokenKind::Refresh => "refresh",
        TokenKind::ApiKey => "api_key",
    }
}

pub(super) fn token_kind_from_sql(s: &str) -> Result<TokenKind, Error> {
    match s {
        "access" => Ok(TokenKind::Access),
        "refresh" => Ok(TokenKind::Refresh),
        "api_key" => Ok(TokenKind::ApiKey),
        other => Err(Error::Internal {
            message: format!("unknown token kind in DB: {other}"),
            correlation_id: "auth_token_kind".to_string(),
        }),
    }
}

pub(super) fn invite_mode_to_sql(m: InviteMode) -> &'static str {
    match m {
        InviteMode::Open => "open",
        InviteMode::Closed => "closed",
    }
}

pub(super) fn invite_mode_from_sql(s: &str) -> Result<InviteMode, Error> {
    match s {
        "open" => Ok(InviteMode::Open),
        "closed" => Ok(InviteMode::Closed),
        other => Err(Error::Internal {
            message: format!("unknown invite mode in DB: {other}"),
            correlation_id: "auth_invite_mode".to_string(),
        }),
    }
}

pub(super) fn access_request_state_to_sql(s: AccessRequestState) -> &'static str {
    match s {
        AccessRequestState::Pending => "pending",
        AccessRequestState::Approved => "approved",
        AccessRequestState::Denied => "denied",
    }
}

pub(super) fn access_request_state_from_sql(s: &str) -> Result<AccessRequestState, Error> {
    match s {
        "pending" => Ok(AccessRequestState::Pending),
        "approved" => Ok(AccessRequestState::Approved),
        "denied" => Ok(AccessRequestState::Denied),
        other => Err(Error::Internal {
            message: format!("unknown access request state in DB: {other}"),
            correlation_id: "auth_access_request_state".to_string(),
        }),
    }
}

pub(super) fn row_to_user(row: &libsql::Row) -> Result<UserRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let name: String = row.get(1).map_err(map_libsql_err)?;
    let role_str: String = row.get(2).map_err(map_libsql_err)?;
    let created_at: String = row.get(3).map_err(map_libsql_err)?;
    Ok(UserRow {
        id,
        name,
        role: role_from_sql(&role_str)?,
        created_at,
    })
}

pub(super) fn row_to_token(row: &libsql::Row) -> Result<AuthTokenRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let user_id: String = row.get(1).map_err(map_libsql_err)?;
    let kind_str: String = row.get(2).map_err(map_libsql_err)?;
    let secret_hash: String = row.get(3).map_err(map_libsql_err)?;
    let expires_at: Option<String> = row.get(4).map_err(map_libsql_err)?;
    let last_used_at: Option<String> = row.get(5).map_err(map_libsql_err)?;
    let revoked_at: Option<String> = row.get(6).map_err(map_libsql_err)?;
    let created_at: String = row.get(7).map_err(map_libsql_err)?;
    let family_id: Option<String> = row.get(8).map_err(map_libsql_err)?;
    let rotated_from: Option<String> = row.get(9).map_err(map_libsql_err)?;
    Ok(AuthTokenRow {
        id,
        user_id,
        kind: token_kind_from_sql(&kind_str)?,
        secret_hash,
        expires_at,
        last_used_at,
        revoked_at,
        created_at,
        family_id,
        rotated_from,
    })
}

pub(super) fn row_to_auth_code(row: &libsql::Row) -> Result<AuthCodeRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let client_id: String = row.get(1).map_err(map_libsql_err)?;
    let user_id: String = row.get(2).map_err(map_libsql_err)?;
    let code_hash: String = row.get(3).map_err(map_libsql_err)?;
    let code_challenge: String = row.get(4).map_err(map_libsql_err)?;
    let code_challenge_method: String = row.get(5).map_err(map_libsql_err)?;
    let redirect_uri: String = row.get(6).map_err(map_libsql_err)?;
    let expires_at: String = row.get(7).map_err(map_libsql_err)?;
    let consumed_at: Option<String> = row.get(8).map_err(map_libsql_err)?;
    let created_at: String = row.get(9).map_err(map_libsql_err)?;
    Ok(AuthCodeRow {
        id,
        client_id,
        user_id,
        code_hash,
        code_challenge,
        code_challenge_method,
        redirect_uri,
        expires_at,
        consumed_at,
        created_at,
    })
}

pub(super) fn row_to_store_grant(row: &libsql::Row) -> Result<StoreGrantRow, Error> {
    let store_name: String = row.get(0).map_err(map_libsql_err)?;
    let user_id: String = row.get(1).map_err(map_libsql_err)?;
    let granted_by: String = row.get(2).map_err(map_libsql_err)?;
    let created_at: String = row.get(3).map_err(map_libsql_err)?;
    Ok(StoreGrantRow {
        store_name,
        user_id,
        granted_by,
        created_at,
    })
}

pub(super) fn row_to_invite(row: &libsql::Row) -> Result<InviteRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let token_hash: String = row.get(1).map_err(map_libsql_err)?;
    let mode_str: String = row.get(2).map_err(map_libsql_err)?;
    let store_grants_json: String = row.get(3).map_err(map_libsql_err)?;
    let max_uses: i64 = row.get(4).map_err(map_libsql_err)?;
    let uses: i64 = row.get(5).map_err(map_libsql_err)?;
    let expires_at: Option<String> = row.get(6).map_err(map_libsql_err)?;
    let revoked_at: Option<String> = row.get(7).map_err(map_libsql_err)?;
    let created_by: String = row.get(8).map_err(map_libsql_err)?;
    let created_at: String = row.get(9).map_err(map_libsql_err)?;
    let store_grants: Vec<String> =
        serde_json::from_str(&store_grants_json).map_err(|e| Error::Internal {
            message: format!("invalid invites.store_grants JSON: {e}"),
            correlation_id: "auth_invite_store_grants_parse".to_string(),
        })?;
    Ok(InviteRow {
        id,
        token_hash,
        mode: invite_mode_from_sql(&mode_str)?,
        store_grants,
        max_uses: max_uses as u32,
        uses: uses as u32,
        expires_at,
        revoked_at,
        created_by,
        created_at,
    })
}

pub(super) fn row_to_access_request(row: &libsql::Row) -> Result<AccessRequestRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let invite_id: String = row.get(1).map_err(map_libsql_err)?;
    let requested_name: String = row.get(2).map_err(map_libsql_err)?;
    let secret_hash: String = row.get(3).map_err(map_libsql_err)?;
    let state_str: String = row.get(4).map_err(map_libsql_err)?;
    let resulting_user_id: Option<String> = row.get(5).map_err(map_libsql_err)?;
    let created_at: String = row.get(6).map_err(map_libsql_err)?;
    let decided_at: Option<String> = row.get(7).map_err(map_libsql_err)?;
    Ok(AccessRequestRow {
        id,
        invite_id,
        requested_name,
        secret_hash,
        state: access_request_state_from_sql(&state_str)?,
        resulting_user_id,
        created_at,
        decided_at,
    })
}
