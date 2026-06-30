use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use localdb_core::Error as CoreError;

use super::{parse_cursor, PaginatedList, PaginationParams};
use crate::error::ApiError;
use crate::state::{AppState, SourceRecord};

pub async fn list_sources(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedList<SourceRecord>>, ApiError> {
    let offset = parse_cursor(pagination.cursor.as_deref())?;

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

pub async fn create_source(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<CreateSourceRequest>,
) -> Result<(StatusCode, Json<SourceRecord>), ApiError> {
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

pub async fn delete_source(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.remove_source(&source_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
