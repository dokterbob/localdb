use axum::{extract::State, Extension, Json};
use serde::Serialize;

use localdb_core::auth::Principal;
use localdb_core::types::StoreVisibility;

use super::require_principal;
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub daemon: bool,
    pub store_count: usize,
    pub source_count: usize,
    pub job_count: usize,
}

/// Parse an `EffectiveStore`'s string `visibility` for a D7 read-access
/// check, treating an unrecognized value as `private` (deny by default) —
/// mirrors `handlers::stores::readable`.
fn readable(principal: &Principal, name: &str, visibility: &str) -> bool {
    let visibility = StoreVisibility::parse(visibility).unwrap_or(StoreVisibility::Private);
    principal.can_read_store(name, visibility)
}

/// `GET /v1/status`: `store_count`/`source_count` are scoped to the stores
/// `principal` can read (D7) — an admin sees the true totals, a member sees
/// counts only for `shared` stores they hold a grant for, exactly matching
/// what `list_stores`/`search` would show them. Without this scoping, a
/// member could infer the existence and size of private/ungranted stores
/// purely from the aggregate counts (finding #6).
pub async fn get_status(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
) -> Result<Json<StatusResponse>, ApiError> {
    let principal = require_principal(principal)?;
    let effective = state.effective_config().await?;
    let readable_stores: Vec<_> = effective
        .stores
        .iter()
        .filter(|s| readable(&principal, &s.name, &s.visibility))
        .collect();
    let store_count = readable_stores.len();

    let mut source_count = 0;
    for store in &readable_stores {
        let sources = state.list_sources(&store.name).await?;
        source_count += sources.len();
    }

    // Jobs are keyed by store ID; scope the count the same way as
    // `store_count`/`source_count` — the job-queue HTTP surface
    // (`create_job`/`get_job`) is admin-only, but `/v1/status` is not, so
    // this is the one place an unscoped count would otherwise leak indexing
    // activity on a private/ungranted store to a member.
    let readable_store_ids: std::collections::HashSet<&str> =
        readable_stores.iter().map(|s| s.id.as_str()).collect();
    let jobs = state.job_queue().list_jobs().await;
    let job_count = jobs
        .iter()
        .filter(|j| readable_store_ids.contains(j.store_id.as_str()))
        .count();

    Ok(Json(StatusResponse {
        daemon: true,
        store_count,
        source_count,
        job_count,
    }))
}
