use axum::{extract::State, Json};
use serde::Serialize;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub daemon: bool,
    pub store_count: usize,
    pub source_count: usize,
    pub job_count: usize,
}

pub async fn get_status(State(state): State<AppState>) -> Result<Json<StatusResponse>, ApiError> {
    let effective = state.effective_config().await?;
    let store_count = effective.stores.len();

    let mut source_count = 0;
    for store in &effective.stores {
        let sources = state.list_sources(&store.name).await?;
        source_count += sources.len();
    }

    let jobs = state.job_queue().list_jobs().await;

    Ok(Json(StatusResponse {
        daemon: true,
        store_count,
        source_count,
        job_count: jobs.len(),
    }))
}
