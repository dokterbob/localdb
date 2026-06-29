use axum::{extract::State, Json};

use crate::error::ApiError;
use crate::search_service::{SearchRequest, SearchResponse, SearchService};
use crate::state::AppState;

pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let svc = SearchService::new(state);
    svc.query(req).await.map(Json)
}
