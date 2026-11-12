//! `GET/POST /v1/users/{id}/keys`, `DELETE /v1/keys/{id}` — API key
//! management (specs/05-surfaces.md §3.1, §2's `key create|list|revoke`
//! CLI counterpart in `cli/src/cmds/auth.rs`). Only `kind = api_key` tokens
//! are exposed here — access/refresh session tokens are an implementation
//! detail of the OAuth2 flow, not something a caller manages directly.
//!
//! Listing never returns a secret (only metadata — id, timestamps); a
//! secret is shown exactly once, in the response body of `POST
//! .../keys` itself (D1).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Serialize;

use localdb_core::auth::{AuthStore as _, Principal, SessionOrigin, TokenKind};
use localdb_core::Error as CoreError;

use super::require_principal;
use crate::error::ApiError;
use crate::state::AppState;

/// API key metadata — never the secret (D1: shown once, at creation, never
/// persisted in retrievable form).
#[derive(Debug, Serialize)]
pub struct KeyView {
    pub id: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// `GET /v1/users/{id}/keys`: admin-only, lists the named user's API keys
/// (metadata only).
pub async fn list_keys(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(user_id): Path<String>,
) -> Result<Json<Vec<KeyView>>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    if state.auth_store().get_user(&user_id).await?.is_none() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!("user '{user_id}' not found"),
        }));
    }
    let keys = state
        .auth_store()
        .list_tokens_for_user(&user_id)
        .await?
        .into_iter()
        .filter(|t| t.kind == TokenKind::ApiKey)
        .map(|t| KeyView {
            id: t.id,
            created_at: t.created_at,
            last_used_at: t.last_used_at,
            expires_at: t.expires_at,
            revoked_at: t.revoked_at,
        })
        .collect();
    Ok(Json(keys))
}

/// The show-once response to `POST /v1/users/{id}/keys`: the plaintext
/// secret is never persisted or retrievable again after this response (D1).
#[derive(Debug, Serialize)]
pub struct CreateKeyResponse {
    pub id: String,
    pub secret: String,
}

/// `POST /v1/users/{id}/keys`: admin-only for another user, but a caller
/// may always mint a key for *themselves* — the one self-service carve-out
/// among the admin-only management routes (specs/05-surfaces.md §3.1).
///
/// The caller's `SessionOrigin` (absent in open mode, or when the
/// caller's own credential carries no session ancestry) is threaded into
/// the minted key as `minted_by_family`, so a key minted while authenticated
/// by a session — or by a key already tainted by one — is revoked alongside
/// it on reuse-detected family revocation. This also covers an admin
/// minting a key for *another* user from a stolen session: that key is
/// tainted too, deliberately (the session itself is compromised either
/// way).
pub async fn create_key(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    origin: Option<Extension<SessionOrigin>>,
    Path(user_id): Path<String>,
) -> Result<(StatusCode, Json<CreateKeyResponse>), ApiError> {
    let principal = require_principal(principal)?;
    if principal.user_id != user_id {
        principal.require_admin().map_err(ApiError)?;
    }
    if state.auth_store().get_user(&user_id).await?.is_none() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!("user '{user_id}' not found"),
        }));
    }
    let minted_by_family = origin.and_then(|Extension(SessionOrigin(family))| family);
    let issued = state
        .auth()
        .issue_api_key_minted_by(&user_id, minted_by_family)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreateKeyResponse {
            id: issued.row.id,
            secret: issued.secret,
        }),
    ))
}

/// `DELETE /v1/keys/{id}`: admin-only, revokes any token by its own ID
/// (not its secret) — used to revoke a lost/compromised API key without
/// needing the secret itself.
pub async fn revoke_key(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(key_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    if state.auth_store().find_token(&key_id).await?.is_none() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!("key '{key_id}' not found"),
        }));
    }
    state.auth_store().revoke_token(&key_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
