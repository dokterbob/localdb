use axum::{extract::State, Extension, Json};

use localdb_core::auth::Principal;

use super::require_principal;
use crate::error::ApiError;
use crate::search_service::{SearchRequest, SearchResponse, SearchService};
use crate::state::AppState;

/// `POST /v1/search`: results are scoped by the caller's store access (D7).
/// An explicit `store_filter` naming a store the caller cannot read is a 403
/// (the caller asked for something specific and was refused), not a silent
/// drop — see `SearchService::query`'s doc comment for the full rule,
/// including the "search all visible stores" (no filter) case.
pub async fn search(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let principal = require_principal(principal)?;
    let svc = SearchService::new(state);
    svc.query(req, &principal).await.map(Json)
}
