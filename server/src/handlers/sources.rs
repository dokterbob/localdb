use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;

use localdb_core::auth::Principal;
use localdb_core::types::StoreVisibility;
use localdb_core::Error as CoreError;

use super::{parse_cursor, require_principal, PaginatedList, PaginationParams};
use crate::error::ApiError;
use crate::state::{AppState, SourceRecord};

/// `GET /v1/stores/{name}/sources`: readable like the parent store (D7) —
/// 403 (not 404) if the caller cannot read `store_name`, matching
/// `handlers::stores::get_store`'s documented trade-off.
pub async fn list_sources(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(store_name): Path<String>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedList<SourceRecord>>, ApiError> {
    let principal = require_principal(principal)?;
    let offset = parse_cursor(pagination.cursor.as_deref())?;

    let store = state.get_store_by_name(&store_name).await?;
    let visibility = StoreVisibility::parse(&store.visibility).unwrap_or(StoreVisibility::Private);
    if !principal.can_read_store(&store.name, visibility) {
        return Err(ApiError(CoreError::Forbidden {
            message: format!(
                "user '{}' cannot read sources of store '{store_name}'",
                principal.name
            ),
        }));
    }

    let all = state.list_sources(&store_name).await?;
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
pub struct CreateSourceRequest {
    pub kind: String,
    pub spec: serde_json::Value,
    #[serde(default = "default_prose")]
    pub preset: String,
    pub refresh: Option<String>,
}

fn default_prose() -> String {
    "prose".to_string()
}

/// `POST /v1/stores/{name}/sources`: admin-only mutation.
pub async fn create_source(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(store_name): Path<String>,
    Json(req): Json<CreateSourceRequest>,
) -> Result<(StatusCode, Json<SourceRecord>), ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    if req.kind != "path" && req.kind != "url" {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!(
                "unknown source kind '{}'; expected 'path' or 'url'",
                req.kind
            ),
        }));
    }

    let source = state
        .add_source(
            &store_name,
            &req.kind,
            req.spec,
            &req.preset,
            req.refresh.as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(source)))
}

/// `DELETE /v1/sources/{id}`: admin-only mutation.
pub async fn delete_source(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(source_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    state.remove_source(&source_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
