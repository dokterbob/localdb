use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use localdb_core::Error as CoreError;
use localdb_core::StoreVisibility;

use super::{parse_cursor, PaginatedList, PaginationParams};
use crate::error::ApiError;
use crate::state::{AppState, StoreRecord};

pub async fn list_stores(
    State(state): State<AppState>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedList<StoreRecord>>, ApiError> {
    let effective = state.effective_config().await?;
    let offset = parse_cursor(pagination.cursor.as_deref())?;

    let all: Vec<StoreRecord> = effective
        .stores
        .iter()
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

pub async fn create_store(
    State(state): State<AppState>,
    Json(req): Json<CreateStoreRequest>,
) -> Result<(StatusCode, Json<StoreRecord>), ApiError> {
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

pub async fn get_store(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<StoreRecord>, ApiError> {
    let record = state.get_store_by_name(&name).await?;
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

pub async fn patch_store(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<PatchStoreRequest>,
) -> Result<Json<StoreRecord>, ApiError> {
    state.update_store(&name, req.visibility.as_deref()).await?;
    let record = state.get_store_by_name(&name).await?;
    Ok(Json(record))
}

pub async fn delete_store(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.remove_store(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}
