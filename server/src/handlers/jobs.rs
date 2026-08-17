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
use crate::job_queue::{JobEvent, JobQueue};
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
            // Shared with `UrlRefreshScheduler::tick` via
            // `AppState::run_scoped_job` (#187 review, DRY finding): resolve
            // sources, build/reuse the cached embedder only if there's
            // something to index, and run the job. Only `deletion` differs
            // between the two callers.
            state
                .run_scoped_job(&store_row, job_scope_for_closure, deletion, progress)
                .await
        })
        .await?;

    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// `GET /v1/jobs` — list every job on the daemon's queue, regardless of
/// state or store.
///
/// Returns the raw `IndexJob[]` array directly (no pagination envelope,
/// unlike `/v1/stores`/`/v1/sources` — `JobQueue::list_jobs` is already an
/// in-memory snapshot with no unbounded-growth concern the other list
/// endpoints paginate against). Order is whatever `JobQueue::list_jobs`
/// returns (registry iteration order — not guaranteed stable), same as
/// every other consumer of that method.
pub async fn list_jobs(State(state): State<AppState>) -> Json<Vec<IndexJob>> {
    Json(state.job_queue().list_jobs().await)
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

/// `DELETE /v1/jobs/{id}` — request cancellation of a queued or running job
/// (issue #218).
///
/// `202` + the job's snapshot at the moment cancellation was requested (not
/// a guarantee it has already stopped — poll `GET /v1/jobs/{id}` or watch
/// `GET /v1/jobs/{id}/events` for the eventual `failed`/`job_cancelled`
/// terminal state). `404` for an unknown job id; `409 job_already_terminal`
/// for a job that already reached `done` or `failed` — a cancel landing
/// after normal completion must never overwrite the recorded outcome, which
/// is exactly what `JobQueue::cancel` guarantees by checking the registry's
/// terminal state before ever touching the job's cancellation token.
pub async fn cancel_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<(StatusCode, Json<IndexJob>), ApiError> {
    let job = state.job_queue().cancel(&job_id).await?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// The state machine driving `GET /v1/jobs/{id}/events`'s SSE stream
/// (issue #83).
///
/// `Live(rx)` streams `progress` events off the job's broadcast channel
/// until the in-band `JobEvent::Terminal` snapshot arrives, yields it as the
/// final `job` frame, and transitions to
/// `Finished`. `Terminal(job)` is the "already done at subscribe time" and
/// "channel-already-torn-down" fast paths: it yields the given job's
/// terminal event immediately and transitions to `Finished`. `Finished` ends
/// the stream.
enum JobEventState {
    Live(broadcast::Receiver<JobEvent>),
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
                Ok(JobEvent::Progress(event)) => {
                    return Some((progress_sse_event(&event), JobEventState::Live(rx)))
                }
                // The job's final state, delivered in-band as the channel's
                // guaranteed last message — never
                // dropped by lag, and immune to terminal-job eviction: no
                // registry read involved, so a burst of later completions
                // evicting this job's registry entry can't cost an attached
                // subscriber its terminal frame.
                Ok(JobEvent::Terminal(job)) => {
                    return Some((terminal_job_event(&job), JobEventState::Finished));
                }
                // Progress is lossy-tolerant by design: a lagging subscriber
                // skips ahead rather than buffering unboundedly or stalling
                // the stream. Only the terminal event (above) is guaranteed.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                // Defensive only: `subscribe`'s contract (see its doc
                // comment) is that a successfully-obtained receiver always
                // yields `JobEvent::Terminal` before the channel closes, so
                // this arm should be unreachable. Falling back to the
                // registry keeps the stream correct even if that invariant
                // ever regresses — though the job may already be evicted
                // (`None` ends the stream without a terminal frame, the
                // pre-round-3 behavior).
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

    // Keep-alive (A5, issue #207): under retry/backoff pacing, minutes can
    // pass between real `ProgressEvent`s. Without an explicit keep-alive,
    // nothing on this stream detects a dead peer and nothing holds
    // intermediaries (proxies, load balancers) open in the meantime. The
    // client side already tolerates this fine either way — `job_attach.rs`'s
    // SSE client sets no `.timeout()` and degrades to polling
    // (`SseAttachError::Fallback`) on any stream failure — but a
    // keep-alive is what lets the *server* notice a genuinely dead
    // connection promptly instead of leaving the stream (and the resources
    // behind it) open indefinitely.
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}
