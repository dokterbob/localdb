use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use localdb_core::{DeletionPolicy, Error as CoreError, IndexJob, IndexJobScope};

use crate::error::ApiError;
use crate::job_exec::{self, JobExecDeps};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub store_name: String,
    #[serde(default)]
    pub source_id: Option<String>,
    /// `"retain"` (default) never removes documents; `"delete"` prunes
    /// documents no longer present at their source. Any other value is a
    /// 400 `invalid_request`. See `localdb_core::ingestion::DeletionPolicy`
    /// and issues #156/#185 for why deletion is opt-in rather than the
    /// default.
    #[serde(default)]
    pub deletion_policy: Option<String>,
}

fn parse_deletion_policy(raw: Option<&str>) -> Result<DeletionPolicy, ApiError> {
    match raw {
        None | Some("retain") => Ok(DeletionPolicy::Retain),
        Some("delete") => Ok(DeletionPolicy::Prune),
        Some(other) => Err(ApiError(CoreError::InvalidRequest {
            message: format!("invalid deletion_policy '{other}'; expected 'retain' or 'delete'"),
        })),
    }
}

pub async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<IndexJob>), ApiError> {
    let deletion = parse_deletion_policy(req.deletion_policy.as_deref())?;

    let store_row = state
        .backend()
        .get_store_by_name(&req.store_name)
        .await?
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

    let job_scope_for_closure = scope.clone();
    // Clone the queue handle (cheap: Arc-based) before moving `state` into
    // the closure below — `state.job_queue()` borrows `state`, which would
    // otherwise conflict with the closure's move of `state` in the same
    // statement.
    let queue = state.job_queue().clone();
    let job = queue
        .submit(&req.store_name, scope, move || async move {
            let yaml = state.yaml_config().await;
            let deps = JobExecDeps {
                backend: state.backend(),
                yaml: &yaml,
                models_dir: state.models_dir(),
                embedder: None,
                progress: None,
            };
            job_exec::run_job(&store_row, job_scope_for_closure, deletion, deps)
                .await
                .map(|(stats, _embedder)| stats)
                .map_err(|e| e.to_string())
        })
        .await?;

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
