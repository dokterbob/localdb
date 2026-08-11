//! Shared job-submission/attach machinery for the unified async job model
//! (issue #187 stage 3, maintainer decision D1).
//!
//! Both `cmds::index` (`localdb index`) and `cmds::source` (`source add`'s
//! auto-index) drive a single store's indexing work through exactly this
//! module, in both transports:
//!
//! - **Embedded** ([`run_embedded_store_job`]): a local [`JobQueue`] runs
//!   `job_exec::run_job` in-process — the same engine the daemon uses,
//!   scoped to one job — and this module subscribes to that job's own
//!   progress-event broadcast channel to drive the CLI's progress sink.
//! - **Daemon-routed** ([`run_daemon_store_job`]): `POST /v1/jobs` submits
//!   the job, then [`attach_daemon_job`] streams `GET /v1/jobs/{id}/events`
//!   (Server-Sent Events) to drive the same progress sink live, falling back
//!   to polling `GET /v1/jobs/{id}` every 500ms if the stream can't be
//!   established (an older daemon predating issue #83, or any other
//!   connect/route failure) or drops mid-stream.
//!
//! Both paths converge on the same `Result<IndexSummary, Error>` shape, fed
//! by the same `ProgressEvent` stream and the same [`IndexErrorMode`]
//! strict-vs-warn semantics — so `cmds::index`/`cmds::source` can loop over
//! resolved stores without caring which transport is underneath.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use localdb_core::{
    config::loader::ConfigLoader, DeletionPolicy, Embedder, Error, IndexJob, IndexJobScope,
    IndexJobState, IndexJobStats, ProgressEvent, ProgressSink, StoreRow,
};
use server::job_exec::{self, JobExecDeps, SourceError};
use server::JobQueue;

use crate::app_db::AppDb;
use crate::cmds::index::{IndexErrorMode, IndexSummary};
use crate::daemon_client::{daemon_request_async, encode_path_segment, CliContext};

// ---------------------------------------------------------------------------
// Embedded transport
// ---------------------------------------------------------------------------

/// Run one store's index job through the embedded engine: a local
/// [`JobQueue`] submission of `job_exec::run_job`, with this process's own
/// progress sink subscribed to the job's broadcast channel.
///
/// `embedder` is threaded in/out by the caller across a multi-store loop
/// (mirroring the pre-#187-stage-3 `run_embedded_index_with`'s threading):
/// `None` until the first store that actually has sources to index builds
/// one, `Some(..)` for the rest — reloading a ~706 MB local embedding model
/// per store would be wasteful. The embedder is built *outside* the queued
/// job (here, not inside `job_exec::run_job`) specifically so a build
/// failure — the one pre-flight failure integration tests pin an exact exit
/// code for (`index_embedder_creation_failure_exits_2`) — surfaces as a
/// precisely-typed `Error` rather than an opaque job-failure string.
///
/// `mode` controls two things: the wording of per-source diagnostic lines
/// (via [`SourceError`]/`emit_source_error`, reproducing the CLI's
/// historical `eprintln!` text — pinned by integration tests — through the
/// shared engine) and whether a job-level failure aborts the caller
/// (`StrictExit`, `index`) or is swallowed into a warning
/// (`WarnAndContinue`, `source add`'s auto-index).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_embedded_store_job(
    ctx: &CliContext,
    queue: &JobQueue,
    config_loader: &ConfigLoader,
    db: &AppDb,
    store_row: &StoreRow,
    scope: IndexJobScope,
    deletion: DeletionPolicy,
    mode: IndexErrorMode,
    embedder: &mut Option<Arc<dyn Embedder>>,
    progress_label: Option<&str>,
) -> Result<IndexSummary, Error> {
    let sources = match job_exec::resolve_job_sources(db.backend(), &store_row.id, &scope).await {
        Ok(s) => s,
        Err(e) => {
            return if mode.warn() {
                eprintln!("warning: cannot list sources for auto-index: {}", e);
                Ok(IndexSummary::default())
            } else {
                Err(e)
            };
        }
    };
    if sources.is_empty() {
        return Ok(IndexSummary::default());
    }

    let built_embedder = if let Some(e) = embedder.as_ref() {
        e.clone()
    } else {
        match embed::create_embedder(
            &config_loader.config.defaults.indexing.embedding,
            &config_loader.config.providers,
            Some(&config_loader.paths.models_dir),
        ) {
            Ok(built) => {
                #[cfg(test)]
                crate::cmds::index::EMBEDDER_BUILD_COUNT
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let arc: Arc<dyn Embedder> = Arc::from(built);
                *embedder = Some(arc.clone());
                arc
            }
            Err(e) => {
                let e = Error::from(e);
                return if mode.warn() {
                    eprintln!("warning: cannot create embedder for auto-index: {}", e);
                    Ok(IndexSummary::default())
                } else {
                    Err(e)
                };
            }
        }
    };

    let backend = db.backend_arc();
    let yaml = config_loader.config.clone();
    let models_dir = config_loader.paths.models_dir.clone();
    let store_row_owned = store_row.clone();
    let scope_for_job = scope.clone();
    let on_source_error: job_exec::OnSourceError =
        Arc::new(move |source_id, err| emit_source_error(mode, source_id, err));

    let job = queue
        .submit(&store_row.id, scope, move |progress| {
            let on_source_error = on_source_error.clone();
            async move {
                let deps = JobExecDeps {
                    backend: backend.as_ref(),
                    yaml: &yaml,
                    models_dir: &models_dir,
                    embedder: Some(built_embedder),
                    progress: Some(progress),
                    on_source_error: Some(on_source_error),
                };
                job_exec::run_job(&store_row_owned, scope_for_job, deletion, deps)
                    .await
                    .map(|(stats, _)| stats)
                    .map_err(|e| e.to_string())
            }
        })
        .await?;

    let final_job = drive_embedded_job(queue, &job.id, ctx.json, progress_label).await;
    finish_job(
        mode,
        "auto-index",
        final_job.state,
        final_job.stats,
        final_job.error,
    )
}

/// Subscribe to `job_id`'s live progress on the local queue, feeding every
/// event into the CLI's progress sink until the channel closes (the job has
/// gone terminal — see `JobQueue`'s `EventRegistry` doc comment), then read
/// back the terminal `IndexJob` from the registry.
async fn drive_embedded_job(
    queue: &JobQueue,
    job_id: &str,
    json_mode: bool,
    progress_label: Option<&str>,
) -> IndexJob {
    let sink = crate::progress::build_progress_sink(json_mode, progress_label);
    if let Some(mut rx) = queue.subscribe(job_id).await {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Some(s) = &sink {
                        s(event);
                    }
                }
                // Progress is lossy-tolerant by design (see `job_queue.rs`'s
                // `EVENT_CHANNEL_CAPACITY` doc comment) — a lagging
                // subscriber skips ahead rather than stalling.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    queue
        .get_job(job_id)
        .await
        .expect("a job just submitted to this process's own local queue must still be registered")
}

/// Render the CLI's historical per-source diagnostic text (pinned by
/// integration tests) for the two per-source failure cases `job_exec::run_job`
/// reports via [`JobExecDeps::on_source_error`]. Wording depends on `mode`:
/// `index` (`StrictExit`) prints "error indexing source ..."; `source add`'s
/// auto-index (`WarnAndContinue`) prints "warning: ...".
fn emit_source_error(mode: IndexErrorMode, source_id: &str, err: SourceError<'_>) {
    match err {
        SourceError::InvalidChunkerPreset { preset, error } => {
            if mode.warn() {
                eprintln!(
                    "warning: invalid chunker preset '{}' for source {}: {}",
                    preset, source_id, error
                );
            } else {
                eprintln!(
                    "error indexing source {}: invalid chunker preset '{}': {}",
                    source_id, preset, error
                );
            }
        }
        SourceError::Ingestion { error } => {
            if mode.warn() {
                eprintln!(
                    "warning: auto-index error for source {}: {}",
                    source_id, error
                );
            } else {
                eprintln!("error indexing source {}: {}", source_id, error);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon transport
// ---------------------------------------------------------------------------

/// Submit one index job to a running daemon for `store_name` and attach to
/// it to completion (SSE, falling back to polling), returning the resulting
/// `IndexSummary`.
///
/// Mirrors [`run_embedded_store_job`]'s `mode`-gated semantics exactly: a
/// submission failure, an attach failure, or a job that ends `Failed` is a
/// hard `Err` under `StrictExit` (`index`) and a warned, defaulted
/// `IndexSummary` under `WarnAndContinue` (`source add`'s auto-index, D3).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_daemon_store_job(
    ctx: &CliContext,
    base_url: &str,
    store_name: &str,
    source_id: Option<&str>,
    deletion: DeletionPolicy,
    mode: IndexErrorMode,
    progress_label: Option<&str>,
) -> Result<IndexSummary, Error> {
    let mut body = serde_json::json!({ "store_name": store_name });
    if let Some(sid) = source_id {
        body["source_id"] = serde_json::Value::String(sid.to_string());
    }
    // D6: the CLI no longer refuses `--delete` against a daemon — it sends
    // the real deletion policy and lets the daemon (which now runs real
    // ingestion, issue #187) honor it.
    body["deletion_policy"] = serde_json::Value::String(
        match deletion {
            DeletionPolicy::Prune => "delete",
            DeletionPolicy::Retain => "retain",
        }
        .to_string(),
    );

    let submit_url = format!("{}/v1/jobs", base_url);
    let job_json = match daemon_request_async(reqwest::Method::POST, &submit_url, Some(body)).await
    {
        Ok(v) => v,
        Err(e) => {
            return if mode.warn() {
                eprintln!(
                    "warning: cannot submit auto-index job for store '{}': {}",
                    store_name, e
                );
                Ok(IndexSummary::default())
            } else {
                Err(e)
            };
        }
    };
    let job_id = match job_json.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            let e = Error::Internal {
                message: "daemon job submission response missing 'id'".to_string(),
                correlation_id: "daemon_job_submit_shape".to_string(),
            };
            return if mode.warn() {
                eprintln!("warning: {}", e);
                Ok(IndexSummary::default())
            } else {
                Err(e)
            };
        }
    };

    let final_job = match attach_daemon_job(base_url, &job_id, ctx.json, progress_label).await {
        Ok(j) => j,
        Err(e) => {
            return if mode.warn() {
                eprintln!(
                    "warning: cannot attach to auto-index job '{}': {}",
                    job_id, e
                );
                Ok(IndexSummary::default())
            } else {
                Err(e)
            };
        }
    };

    finish_job(
        mode,
        &format!("auto-index job for store '{}'", store_name),
        final_job.state,
        final_job.stats,
        final_job.error,
    )
}

/// Attach to `job_id` on a running daemon until it reaches a terminal
/// state, driving `progress_label`'s progress sink live where possible.
///
/// Tries `GET /v1/jobs/{id}/events` (SSE) first; any failure to establish or
/// sustain that stream — connect failure, a non-2xx response (a 404 means an
/// older daemon predating issue #83), or the connection dropping before a
/// terminal `job` frame arrives — falls back to polling `GET
/// /v1/jobs/{id}` every 500ms. The job was already accepted by the earlier
/// `POST /v1/jobs`, so a failure to *watch* it live is never itself fatal to
/// the command; only a failure of the poll fallback itself propagates.
pub(crate) async fn attach_daemon_job(
    base_url: &str,
    job_id: &str,
    json_mode: bool,
    progress_label: Option<&str>,
) -> Result<IndexJob, Error> {
    let sink = crate::progress::build_progress_sink(json_mode, progress_label);
    match try_attach_via_sse(base_url, job_id, sink.as_ref()).await {
        Ok(job) => Ok(job),
        Err(SseAttachError::Fallback) => poll_job_until_terminal(base_url, job_id).await,
        Err(SseAttachError::Fatal(e)) => Err(e),
    }
}

enum SseAttachError {
    /// Connect failed, the route 404'd/errored, or the stream ended without
    /// ever delivering a terminal `job` frame — all fall back to polling.
    Fallback,
    /// A genuine, non-recoverable failure (currently unused but kept
    /// distinct from `Fallback` so a future caller can distinguish "give up
    /// entirely" from "try polling instead" without changing this enum's
    /// shape).
    #[allow(dead_code)]
    Fatal(Error),
}

/// Hand-rolled SSE line parser over `GET /v1/jobs/{id}/events`'s
/// `bytes_stream()`.
///
/// A dedicated `eventsource-stream`-style crate wasn't pulled in: the wire
/// format this endpoint emits (`server/src/handlers/jobs.rs`'s
/// `progress_sse_event`/`terminal_job_event`) is exactly two field types
/// (`event:`, `data:`) with one JSON value per event and no multi-line
/// `data:` folding in practice, so a ~40-line buffer-and-split parser covers
/// it without a new dependency.
async fn try_attach_via_sse(
    base_url: &str,
    job_id: &str,
    sink: Option<&ProgressSink>,
) -> Result<IndexJob, SseAttachError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|_| SseAttachError::Fallback)?;
    let url = format!(
        "{}/v1/jobs/{}/events",
        base_url,
        encode_path_segment(job_id)
    );
    let resp = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .map_err(|_| SseAttachError::Fallback)?;

    if !resp.status().is_success() {
        return Err(SseAttachError::Fallback);
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut current_event: Option<String> = None;
    let mut current_data = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| SseAttachError::Fallback)?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf.drain(..=nl);

            if line.is_empty() {
                if let Some(ev) = current_event.take() {
                    match ev.as_str() {
                        "job" => {
                            if let Ok(job) = serde_json::from_str::<IndexJob>(&current_data) {
                                return Ok(job);
                            }
                        }
                        "progress" => {
                            if let Ok(event) = serde_json::from_str::<ProgressEvent>(&current_data)
                            {
                                if let Some(s) = sink {
                                    s(event);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                current_data.clear();
                continue;
            }

            if let Some(v) = line.strip_prefix("data:") {
                if !current_data.is_empty() {
                    current_data.push('\n');
                }
                current_data.push_str(v.trim_start());
            } else if let Some(v) = line.strip_prefix("event:") {
                current_event = Some(v.trim().to_string());
            }
            // Other SSE fields (`id:`, `retry:`, `:comment`) are ignored.
        }
    }

    // Stream ended without ever delivering a terminal `job` frame.
    Err(SseAttachError::Fallback)
}

/// How often [`poll_job_until_terminal`] re-checks `GET /v1/jobs/{id}`.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// SSE-attach fallback: poll `GET /v1/jobs/{id}` until it reports a terminal
/// state. No incremental progress is available this way — only the eventual
/// terminal `IndexJob` — which is an accepted degradation for what is
/// already the degraded path (an older daemon, or a stream that dropped).
async fn poll_job_until_terminal(base_url: &str, job_id: &str) -> Result<IndexJob, Error> {
    let url = format!("{}/v1/jobs/{}", base_url, encode_path_segment(job_id));
    loop {
        let v = daemon_request_async(reqwest::Method::GET, &url, None).await?;
        let job: IndexJob = serde_json::from_value(v).map_err(|e| Error::Internal {
            message: format!("cannot parse job status from daemon: {}", e),
            correlation_id: "daemon_job_poll_parse".to_string(),
        })?;
        if matches!(job.state, IndexJobState::Done | IndexJobState::Failed) {
            return Ok(job);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Shared: terminal-state -> `IndexSummary` (both transports)
// ---------------------------------------------------------------------------

/// Fold a job's terminal state into an `IndexSummary`, applying `mode`'s
/// strict-vs-warn semantics to a `Failed` (or, defensively, any other
/// non-terminal) state. `context` is a short human-readable label used only
/// in the resulting diagnostic/error text.
fn finish_job(
    mode: IndexErrorMode,
    context: &str,
    state: IndexJobState,
    stats: IndexJobStats,
    error: Option<String>,
) -> Result<IndexSummary, Error> {
    match state {
        IndexJobState::Done => Ok(IndexSummary::from_job_stats(stats)),
        IndexJobState::Failed => {
            let msg = error.unwrap_or_else(|| "index job failed".to_string());
            if mode.warn() {
                eprintln!("warning: {context}: {msg}");
                Ok(IndexSummary::default())
            } else {
                Err(Error::Internal {
                    message: format!("{context}: {msg}"),
                    correlation_id: "index_job_failed".to_string(),
                })
            }
        }
        _ => Err(Error::Internal {
            message: format!("{context}: job ended in a non-terminal state"),
            correlation_id: "index_job_nonterminal".to_string(),
        }),
    }
}
