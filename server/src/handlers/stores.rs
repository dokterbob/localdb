use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;

use localdb_core::auth::Principal;
use localdb_core::Error as CoreError;
use localdb_core::StoreVisibility;

use super::{parse_cursor, require_principal, PaginatedList, PaginationParams};
use crate::error::ApiError;
use crate::state::{AppState, StoreRecord};

/// Parse an `EffectiveStore`/`StoreRecord`'s string `visibility` for a D7
/// read-access check, treating an unrecognized value as `private` (deny by
/// default) — see `StoreVisibility::parse`'s doc comment.
fn readable(principal: &Principal, name: &str, visibility: &str) -> bool {
    let visibility = StoreVisibility::parse(visibility).unwrap_or(StoreVisibility::Private);
    principal.can_read_store(name, visibility)
}

/// `GET /v1/stores`: admins see every store; members see only the `shared`
/// stores they hold a grant for (D7) — non-readable stores are silently
/// omitted from the list rather than erroring.
pub async fn list_stores(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedList<StoreRecord>>, ApiError> {
    let principal = require_principal(principal)?;
    let effective = state.effective_config().await?;
    let offset = parse_cursor(pagination.cursor.as_deref())?;

    let all: Vec<StoreRecord> = effective
        .stores
        .iter()
        .filter(|s| readable(&principal, &s.name, &s.visibility))
        .map(|s| StoreRecord {
            name: s.name.clone(),
            visibility: s.visibility.clone(),
            backend: s.backend.clone(),
        })
        .collect();

    let total = all.len();
    let page = all.into_iter().skip(offset).collect::<Vec<_>>();
    Ok(Json(PaginatedList::new(
        page,
        offset,
        pagination.limit,
        total,
    )))
}

#[derive(Debug, Deserialize)]
pub struct CreateStoreRequest {
    pub name: String,
    #[serde(default = "default_private")]
    pub visibility: String,
}

fn default_private() -> String {
    "private".to_string()
}

/// `POST /v1/stores`: admin-only mutation (specs/05-surfaces.md §3.1 — write
/// routes remain admin-only in this phase; members are readers).
pub async fn create_store(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Json(req): Json<CreateStoreRequest>,
) -> Result<(StatusCode, Json<StoreRecord>), ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    if req.name.is_empty() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: "store name cannot be empty".to_string(),
        }));
    }

    let store = state.add_store(&req.name, &req.visibility).await?;
    let visibility = match store.visibility {
        StoreVisibility::Private => "private".to_string(),
        StoreVisibility::Shared => "shared".to_string(),
    };
    let record = StoreRecord {
        name: store.name.clone(),
        visibility,
        backend: store.backend.kind.clone(),
    };
    Ok((StatusCode::CREATED, Json(record)))
}

/// `GET /v1/stores/{name}`: 403 (not 404) when the store exists but the
/// caller cannot read it — a member probing store names cannot distinguish
/// "unknown" from "private/ungranted" this way, but the alternative (404)
/// would make an explicit-name lookup behave inconsistently with the
/// filtered `list_stores`/`search` results, which is the trade-off
/// specs/05-surfaces.md §3.1 documents as the chosen consistency point.
pub async fn get_store(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(name): Path<String>,
) -> Result<Json<StoreRecord>, ApiError> {
    let principal = require_principal(principal)?;
    let record = state.get_store_by_name(&name).await?;
    if !readable(&principal, &record.name, &record.visibility) {
        return Err(ApiError(CoreError::Forbidden {
            message: format!("user '{}' cannot read store '{name}'", principal.name),
        }));
    }
    Ok(Json(record))
}

/// Request body for PATCH /stores/{name}.
///
/// All fields are optional — only provided fields are updated.
#[derive(Debug, Deserialize)]
pub struct PatchStoreRequest {
    /// New visibility value ("private" | "shared").
    pub visibility: Option<String>,
}

/// `PATCH /v1/stores/{name}`: admin-only mutation.
pub async fn patch_store(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(name): Path<String>,
    Json(req): Json<PatchStoreRequest>,
) -> Result<Json<StoreRecord>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    state.update_store(&name, req.visibility.as_deref()).await?;
    let record = state.get_store_by_name(&name).await?;
    Ok(Json(record))
}

/// `DELETE /v1/stores/{name}`: admin-only mutation.
pub async fn delete_store(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    state.remove_store(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}
