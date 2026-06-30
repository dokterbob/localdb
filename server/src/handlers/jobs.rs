use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use localdb_core::{Error as CoreError, IndexJob, IndexJobScope};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub store_name: String,
    #[serde(default)]
    pub source_id: Option<String>,
}

pub async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<IndexJob>), ApiError> {
    let effective = state.effective_config().await?;
    let _store = effective
        .stores
        .iter()
        .find(|s| s.name == req.store_name)
        .ok_or_else(|| CoreError::StoreNotFound {
            id: req.store_name.clone(),
        })?;

    let scope = if let Some(source_id) = &req.source_id {
        IndexJobScope::Source {
            source_id: source_id.clone(),
        }
    } else {
        IndexJobScope::Store
    };

    let job = state
        .job_queue()
        .submit(&req.store_name, scope, || {
            Ok(localdb_core::IndexJobStats::default())
        })
        .await;

    Ok((StatusCode::ACCEPTED, Json(job)))
}

pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<IndexJob>, ApiError> {
    state
        .job_queue()
        .get_job(&job_id)
        .await
        .map(Json)
        .ok_or(ApiError(CoreError::JobNotFound { id: job_id }))
}
