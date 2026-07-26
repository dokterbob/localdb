//! `GET/POST /v1/stores/{name}/grants`, `DELETE /v1/stores/{name}/grants/{user}`
//! — D7 store-grant management (specs/05-surfaces.md §3.1). Admin-only:
//! only admins may grant/revoke a member's read access to a `shared` store.
//! Grants against a `private` store are rejected by `core` itself
//! (`AuthService::grant_store`) — this layer just resolves the store's
//! visibility to pass in and surfaces whatever `core` decides.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use localdb_core::auth::{AuthStore as _, Principal, UserRow};
use localdb_core::types::StoreVisibility;
use localdb_core::Error as CoreError;

use super::require_principal;
use crate::error::ApiError;
use crate::state::AppState;

/// A store grant as returned by the API, with the user's name resolved
/// alongside its ID for display convenience.
#[derive(Debug, Serialize)]
pub struct GrantView {
    pub store_name: String,
    pub user_id: String,
    pub user_name: String,
    pub granted_by: String,
    pub created_at: String,
}

/// Resolve a user identifier that may be either a name or an ID (the shape
/// `POST .../grants`'s body and `DELETE .../grants/{user}`'s path segment
/// both accept, per specs/05-surfaces.md §3.1): try by name first (the
/// common case for a human-typed CLI/API call), falling back to ID.
async fn resolve_user(state: &AppState, ident: &str) -> Result<UserRow, ApiError> {
    if let Some(user) = state.auth_store().get_user_by_name(ident).await? {
        return Ok(user);
    }
    if let Some(user) = state.auth_store().get_user(ident).await? {
        return Ok(user);
    }
    Err(ApiError(CoreError::InvalidRequest {
        message: format!("no user named or with id '{ident}'"),
    }))
}

/// `GET /v1/stores/{name}/grants`: admin-only, lists every grant on the
/// named store.
pub async fn list_grants(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(store_name): Path<String>,
) -> Result<Json<Vec<GrantView>>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    // Confirm the store exists so an unknown name is a 404, not an empty list.
    state.get_store_by_name(&store_name).await?;

    let grants = state
        .auth_store()
        .list_grants_for_store(&store_name)
        .await?;
    let mut out = Vec::with_capacity(grants.len());
    for g in grants {
        let user_name = state
            .auth_store()
            .get_user(&g.user_id)
            .await?
            .map(|u| u.name)
            .unwrap_or_else(|| g.user_id.clone());
        out.push(GrantView {
            store_name: g.store_name,
            user_id: g.user_id,
            user_name,
            granted_by: g.granted_by,
            created_at: g.created_at,
        });
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct CreateGrantRequest {
    /// The grantee, by user name or ID (see `resolve_user`).
    pub user: String,
}

/// `POST /v1/stores/{name}/grants`: admin-only, grants `req.user` read
/// access to `store_name`. Rejected (`forbidden`) if the store is
/// `private` — only `shared` stores are grantable (D7).
pub async fn create_grant(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(store_name): Path<String>,
    Json(req): Json<CreateGrantRequest>,
) -> Result<(StatusCode, Json<GrantView>), ApiError> {
    let principal = require_principal(principal)?;
    principal.require_admin().map_err(ApiError)?;

    let store = state.get_store_by_name(&store_name).await?;
    let visibility = StoreVisibility::parse(&store.visibility).unwrap_or(StoreVisibility::Private);
    let user = resolve_user(&state, &req.user).await?;

    state
        .auth()
        .grant_store(&store_name, visibility, &user.id, &principal.user_id)
        .await?;

    // Read the row back rather than fabricating `created_at`/`granted_by`
    // locally, so the response reflects exactly what was persisted.
    let grant = state
        .auth_store()
        .list_grants_for_user(&user.id)
        .await?
        .into_iter()
        .find(|g| g.store_name == store_name)
        .ok_or_else(|| CoreError::Internal {
            message: "grant_store succeeded but the row cannot be found".to_string(),
            correlation_id: "grants_create_readback".to_string(),
        })?;

    Ok((
        StatusCode::CREATED,
        Json(GrantView {
            store_name: grant.store_name,
            user_id: user.id,
            user_name: user.name,
            granted_by: grant.granted_by,
            created_at: grant.created_at,
        }),
    ))
}

/// `DELETE /v1/stores/{name}/grants/{user}`: admin-only, revokes `user`'s
/// (name or ID) grant on `store_name`.
pub async fn delete_grant(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path((store_name, user_ident)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let user = resolve_user(&state, &user_ident).await?;
    let revoked = state.auth().revoke_store(&store_name, &user.id).await?;
    if !revoked {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!("no grant for user '{user_ident}' on store '{store_name}'"),
        }));
    }
    Ok(StatusCode::NO_CONTENT)
}
