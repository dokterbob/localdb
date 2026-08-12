use std::convert::Infallible;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    Json,
};
use futures::stream::{self, Stream};
use serde::Deserialize;
use tokio::sync::broadcast;

use localdb_core::{
    DeletionPolicy, Error as CoreError, IndexJob, IndexJobScope, IndexJobState, ProgressEvent,
};

use crate::error::ApiError;
use crate::job_exec::{self, JobExecDeps};
use crate::job_queue::JobQueue;
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
        .submit(&req.store_name, scope, move |progress| async move {
            let yaml = state.yaml_config().await;
            // Codex review finding F2 (#187): reuse the daemon's cached
            // embedder instead of building one from scratch for every job —
            // for the default local ONNX/CoreML provider that's a
            // ~model-load per job avoided.
            let embedder = state.get_or_build_embedder(&yaml).await?;
            let deps = JobExecDeps {
                backend: state.backend(),
                yaml: &yaml,
                models_dir: state.models_dir(),
                embedder: Some(embedder),
                progress: Some(progress),
                on_source_error: None,
            };
            job_exec::run_job(&store_row, job_scope_for_closure, deletion, deps)
                .await
                .map(|(stats, _embedder)| stats)
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

/// The state machine driving `GET /v1/jobs/{id}/events`'s SSE stream
/// (issue #83).
///
/// `Live(rx)` streams `progress` events off the job's broadcast channel
/// until it closes, then (in the same poll) fetches the terminal `IndexJob`
/// from the registry and transitions to `Finished` after yielding it.
/// `Terminal(job)` is the "already done at subscribe time" and
/// "channel-already-torn-down" fast paths: it yields the given job's
/// terminal event immediately and transitions to `Finished`. `Finished` ends
/// the stream.
enum JobEventState {
    Live(broadcast::Receiver<ProgressEvent>),
    Terminal(Box<IndexJob>),
    Finished,
}

fn is_terminal(state: &IndexJobState) -> bool {
    matches!(state, IndexJobState::Done | IndexJobState::Failed)
}

/// Build the stream's final SSE item: the terminal `IndexJob`, as an `event:
/// job` frame with a JSON `data:` payload. `IndexJob` is a plain struct of
/// strings/enums/numbers, so JSON encoding it cannot fail in practice —
/// `expect` documents that assumption rather than silently swallowing a
/// serialization bug.
fn terminal_job_event(job: &IndexJob) -> Result<Event, Infallible> {
    Ok(Event::default()
        .event("job")
        .json_data(job)
        .expect("IndexJob is always JSON-serializable"))
}

/// Build a `progress` SSE frame from a [`ProgressEvent`]. Serialization
/// cannot fail for the same reason as [`terminal_job_event`] — `ProgressEvent`
/// is composed entirely of strings/enums/numbers.
fn progress_sse_event(event: &ProgressEvent) -> Result<Event, Infallible> {
    Ok(Event::default()
        .event("progress")
        .json_data(event)
        .expect("ProgressEvent is always JSON-serializable"))
}

async fn next_job_event(
    state: JobEventState,
    queue: JobQueue,
    job_id: String,
) -> Option<(Result<Event, Infallible>, JobEventState)> {
    match state {
        JobEventState::Live(mut rx) => loop {
            match rx.recv().await {
                Ok(event) => return Some((progress_sse_event(&event), JobEventState::Live(rx))),
                // Progress is lossy-tolerant by design: a lagging subscriber
                // skips ahead rather than buffering unboundedly or stalling
                // the stream. Only the terminal event (below) is guaranteed.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                // The channel only closes after `run_worker` has already
                // committed the job's terminal state to the registry (see
                // `EventRegistry`'s doc comment in `job_queue.rs`) — so this
                // `get_job` is guaranteed to see it.
                Err(broadcast::error::RecvError::Closed) => {
                    let job = queue.get_job(&job_id).await?;
                    return Some((terminal_job_event(&job), JobEventState::Finished));
                }
            }
        },
        JobEventState::Terminal(job) => {
            let event = terminal_job_event(&job);
            Some((event, JobEventState::Finished))
        }
        JobEventState::Finished => None,
    }
}

/// `GET /v1/jobs/{id}/events` — stream a job's live progress as
/// Server-Sent Events (issue #83).
///
/// Semantics:
/// - Unknown job id: 404 `job_not_found`, matching `get_job`.
/// - Job already terminal at subscribe time: exactly one `event: job` frame
///   carrying the terminal `IndexJob`, then the stream ends.
/// - Job still running: zero or more `event: progress` frames (one per
///   `ProgressEvent`), followed by exactly one final `event: job` frame,
///   then the stream ends.
///
/// Order of operations matters for correctness: the registry is read
/// *first* (`get_job`); only if the job isn't already terminal does this
/// subscribe to the broadcast channel. If the job raced to completion
/// between those two steps, `subscribe` finds no channel (already torn
/// down by `run_worker`) and this falls back to a fresh registry read —
/// so the terminal event is never missed, only ever raced into being
/// delivered via one path or the other.
pub async fn job_events(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let queue = state.job_queue().clone();

    let job = queue
        .get_job(&job_id)
        .await
        .ok_or_else(|| ApiError(CoreError::JobNotFound { id: job_id.clone() }))?;

    let initial_state = if is_terminal(&job.state) {
        JobEventState::Terminal(Box::new(job))
    } else {
        match queue.subscribe(&job_id).await {
            Some(rx) => JobEventState::Live(rx),
            None => {
                // The job's channel was already torn down — it must have
                // reached a terminal state between the `get_job` above and
                // this `subscribe`. Re-read the (now terminal) job.
                let job = queue
                    .get_job(&job_id)
                    .await
                    .ok_or_else(|| ApiError(CoreError::JobNotFound { id: job_id.clone() }))?;
                JobEventState::Terminal(Box::new(job))
            }
        }
    };

    let stream = stream::unfold(initial_state, move |state| {
        let queue = queue.clone();
        let job_id = job_id.clone();
        next_job_event(state, queue, job_id)
    });

    Ok(Sse::new(stream))
}
