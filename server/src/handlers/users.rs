//! `GET/POST /v1/users`, `PATCH/DELETE /v1/users/{id}` — user account
//! management (specs/05-surfaces.md §3.1). Every route here is admin-only:
//! handlers call `Principal::require_admin` up front, mirroring the CLI's
//! break-glass `user add`/`user set-role`/`user remove` (`cli/src/cmds/auth.rs`)
//! but routed through `AuthService` over HTTP instead of writing the
//! database directly.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use localdb_core::auth::{AuthStore as _, Principal, Role, UserRow};
use localdb_core::Error as CoreError;

use super::require_principal;
use crate::error::ApiError;
use crate::state::AppState;

/// A user account as returned by the API. Never carries credential
/// material — see `handlers::keys` for API keys, which are managed
/// separately and only ever show their secret once, at creation.
#[derive(Debug, Serialize)]
pub struct UserView {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub created_at: String,
}

impl From<UserRow> for UserView {
    fn from(u: UserRow) -> Self {
        UserView {
            id: u.id,
            name: u.name,
            role: u.role,
            created_at: u.created_at,
        }
    }
}

/// Parse the wire role string ("admin" | "member") used by
/// `CreateUserRequest`/`PatchUserRequest`, matching `Role`'s serde
/// `rename_all = "lowercase"` shape.
pub(crate) fn parse_role(s: &str) -> Result<Role, ApiError> {
    match s {
        "admin" => Ok(Role::Admin),
        "member" => Ok(Role::Member),
        other => Err(ApiError(CoreError::InvalidRequest {
            message: format!("unknown role '{other}'; expected 'admin' or 'member'"),
        })),
    }
}

/// `GET /v1/users`: admin-only, lists every user.
pub async fn list_users(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
) -> Result<Json<Vec<UserView>>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let users = state.auth_store().list_users().await?;
    Ok(Json(users.into_iter().map(UserView::from).collect()))
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub role: String,
}

/// `POST /v1/users`: admin-only. No passwords (D1) — the caller mints an
/// API key for the new user afterwards via `POST /v1/users/{id}/keys`.
pub async fn create_user(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserView>), ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    if req.name.trim().is_empty() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: "user name must not be empty".to_string(),
        }));
    }
    let role = parse_role(&req.role)?;
    let user = state.auth().create_user(&req.name, role).await?;
    Ok((StatusCode::CREATED, Json(UserView::from(user))))
}

#[derive(Debug, Deserialize)]
pub struct PatchUserRequest {
    pub role: String,
}

/// `PATCH /v1/users/{id}`: admin-only, sets the user's role. Refuses to
/// demote the last remaining admin (`AuthService::set_user_role`'s guard
/// rail, D7) — `conflict`-shaped in intent, mapped to `invalid_request`
/// (400) like every other "this request is well-formed but not allowed
/// given current state" case in this codebase (e.g. duplicate user names).
pub async fn patch_user(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
    Json(req): Json<PatchUserRequest>,
) -> Result<Json<UserView>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let role = parse_role(&req.role)?;
    state.auth().set_user_role(&id, role).await?;
    let user =
        state
            .auth_store()
            .get_user(&id)
            .await?
            .ok_or_else(|| CoreError::InvalidRequest {
                message: format!("user '{id}' not found"),
            })?;
    Ok(Json(UserView::from(user)))
}

/// `DELETE /v1/users/{id}`: admin-only. Refuses to delete the last
/// remaining admin (`AuthService::delete_user`'s guard rail, D7); deleting
/// any other user cascades to their tokens and store grants at the schema
/// level (`ON DELETE CASCADE`).
pub async fn delete_user(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let deleted = state.auth().delete_user(&id).await?;
    if !deleted {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!("user '{id}' not found"),
        }));
    }
    Ok(StatusCode::NO_CONTENT)
}
