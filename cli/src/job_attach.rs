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
///
/// Returns the job id alongside the summary —
/// `Some(job.id)` whenever a job actually got submitted to the local queue,
/// `None` on every early-return path above that (no sources to index, or a
/// pre-flight embedder-build failure warned away under `WarnAndContinue`)
/// where no job ever existed to have an id. Included unconditionally rather
/// than gated behind daemon-only cancellability: it's freely available here
/// (the local `JobQueue::submit` call already returns it) and useful for
/// tracing/correlating a run's own log lines even though `localdb job
/// cancel` itself only ever targets a *daemon's* queue, never this
/// throwaway embedded one.
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
) -> Result<(IndexSummary, Option<String>), Error> {
    let sources = match job_exec::resolve_job_sources(db.backend(), &store_row.id, &scope).await {
        Ok(s) => s,
        Err(e) => {
            return if mode.warn() {
                eprintln!("warning: cannot list sources for auto-index: {}", e);
                Ok((IndexSummary::default(), None))
            } else {
                Err(e)
            };
        }
    };
    if sources.is_empty() {
        return Ok((IndexSummary::default(), None));
    }

    let built_embedder = if let Some(e) = embedder.as_ref() {
        e.clone()
    } else {
        match embed::create_embedder(
            &config_loader.config.defaults.indexing.embedding,
            &config_loader.config.providers,
            Some(&config_loader.paths.models_dir),
            &(&config_loader.config.http).into(),
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
                    Ok((IndexSummary::default(), None))
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
                    // The embedded CLI path runs one job at a time (its own
                    // local, single-worker `JobQueue`) — there's no second
                    // concurrent job to share a fetcher pair with, so this
                    // always falls back to `run_job`'s own fresh
                    // `HttpUrlFetcher::new_pair` build, identical to this
                    // field not existing (issue #208 PR #227 review; see
                    // `JobExecDeps::fetchers`'s doc comment).
                    fetchers: None,
                    progress: Some(progress),
                    on_source_error: Some(on_source_error),
                };
                job_exec::run_job(&store_row_owned, scope_for_job, deletion, deps)
                    .await
                    .map(|(stats, _)| stats)
            }
        })
        .await?;
    let job_id = job.id.clone();

    let final_job = drive_embedded_job(queue, &job.id, ctx.json, progress_label).await;
    let summary = finish_job(
        mode,
        "auto-index",
        final_job.state,
        final_job.stats,
        final_job.error,
        final_job.error_code,
    )?;
    Ok((summary, Some(job_id)))
}

/// Subscribe to `job_id`'s live events on the local queue, feeding every
/// progress event into the CLI's progress sink until the in-band terminal
/// snapshot arrives ([`server::JobEvent::Terminal`]), and return that
/// snapshot. Falls back to a registry read only
/// when no channel exists anymore (the job raced to terminal before this
/// subscribed) or on the defensive channel-closed-without-terminal path.
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
                Ok(server::JobEvent::Progress(event)) => {
                    if let Some(s) = &sink {
                        s(event);
                    }
                }
                // The job's final state, delivered through the channel
                // itself — no registry read, so terminal-job eviction
                // (`MAX_TERMINAL_JOBS`) can never cost an attached CLI its
                // result.
                Ok(server::JobEvent::Terminal(job)) => return *job,
                // Progress is lossy-tolerant by design (see `job_queue.rs`'s
                // `EVENT_CHANNEL_CAPACITY` doc comment) — a lagging
                // subscriber skips ahead rather than stalling.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // Defensive: `subscribe`'s contract guarantees `Terminal`
                // arrives before the close, so this should be unreachable.
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
///
/// Prints the job id as soon as it's known —
/// before attaching — since this is exactly the case
/// `localdb job cancel <id>` can reach (unlike
/// [`run_embedded_store_job`]'s throwaway local queue). Always to stderr
/// (stdout must stay clean JSON under `--json`, matching every other
/// progress-ish line this module emits via `crate::progress`), in a
/// mode-appropriate shape: human mode gets `job <id> (cancel with: localdb
/// job cancel <id>)`, `[label] `-prefixed when `progress_label` is `Some`
/// (multi-store runs); `--json` mode gets one JSON line
/// `{"job_id": "<id>"}` (plus a `"store"` field when `progress_label` is
/// `Some`) — suppressing it entirely left `--json`
/// callers with no way to learn the id until the job was already terminal.
/// Also returned alongside the summary so the final `--json` document can
/// surface it too — `None` only on the two early-return paths before a job
/// id is ever known (a submission failure, or a malformed submission
/// response).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_daemon_store_job(
    ctx: &CliContext,
    base_url: &str,
    store_name: &str,
    source_id: Option<&str>,
    deletion: DeletionPolicy,
    mode: IndexErrorMode,
    progress_label: Option<&str>,
) -> Result<(IndexSummary, Option<String>), Error> {
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
                Ok((IndexSummary::default(), None))
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
                Ok((IndexSummary::default(), None))
            } else {
                Err(e)
            };
        }
    };

    eprintln!(
        "{}",
        pre_attach_job_id_line(ctx.json, &job_id, progress_label)
    );

    let final_job = match attach_daemon_job(base_url, &job_id, ctx.json, progress_label).await {
        Ok(j) => j,
        Err(e) => {
            return if mode.warn() {
                eprintln!(
                    "warning: cannot attach to auto-index job '{}': {}",
                    job_id, e
                );
                // The job id itself is known even though attaching to it
                // failed — unlike the two earlier early-return paths,
                // where no job (and so no id) exists at all yet.
                Ok((IndexSummary::default(), Some(job_id)))
            } else {
                Err(e)
            };
        }
    };

    let summary = finish_job(
        mode,
        &format!("auto-index job for store '{}'", store_name),
        final_job.state,
        final_job.stats,
        final_job.error,
        final_job.error_code,
    )?;
    Ok((summary, Some(job_id)))
}

/// The stderr line announcing a freshly-submitted daemon job's id, emitted
/// before attaching blocks — the one moment
/// `localdb job cancel <id>` is actionable.
///
/// Human mode: `job <id> (cancel with: localdb job cancel <id>)`,
/// `[label] `-prefixed for multi-store runs. `--json` mode: one JSON line
/// `{"job_id": "<id>"}`, plus a `"store"` field when
/// a label is present — previously the id was suppressed entirely under
/// `--json`, so a machine caller couldn't learn it until the job was
/// already terminal (and possibly not at all, on the attach-failure paths
/// where the final document carries no id). Stderr in both modes: stdout
/// must stay one clean JSON document under `--json` (specs/05 §2.1).
fn pre_attach_job_id_line(json_mode: bool, job_id: &str, progress_label: Option<&str>) -> String {
    if json_mode {
        let mut line = serde_json::json!({ "job_id": job_id });
        if let Some(label) = progress_label {
            line["store"] = serde_json::Value::String(label.to_string());
        }
        line.to_string()
    } else {
        let hint = format!("job {job_id} (cancel with: localdb job cancel {job_id})");
        match progress_label {
            Some(label) => format!("[{label}] {hint}"),
            None => hint,
        }
    }
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
    let mut buf: Vec<u8> = Vec::new();
    let mut current_event: Option<String> = None;
    let mut current_data = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| SseAttachError::Fallback)?;
        buf.extend_from_slice(&chunk);

        while let Some(line) = split_next_line(&mut buf) {
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

/// Pop and decode the next completed line (up to but not including the
/// `\n`) out of `buf`, if one is present; a trailing `\r` (CRLF) is
/// stripped, matching the wire format's line endings either way.
///
/// Returns `None` — leaving `buf` untouched — when no `\n` has arrived yet;
/// the caller should wait for the next chunk and try again. Decoding via
/// `String::from_utf8_lossy` runs only once a full line's bytes are in
/// hand, so a multi-byte UTF-8 character split across two network chunks
/// (e.g. `é`'s `0xC3` arriving in one `bytes_stream()` item and `0xA9` in
/// the next) reassembles correctly instead of each half being lossily
/// decoded — and replaced with `U+FFFD` — on its own chunk.
fn split_next_line(buf: &mut Vec<u8>) -> Option<String> {
    let nl = buf.iter().position(|&b| b == b'\n')?;
    let mut line_bytes: Vec<u8> = buf.drain(..=nl).collect();
    line_bytes.pop(); // the '\n' itself
    if line_bytes.last() == Some(&b'\r') {
        line_bytes.pop();
    }
    Some(String::from_utf8_lossy(&line_bytes).into_owned())
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
///
/// `error_code`, when present, is the failed job's `Error::code()` string
/// (`IndexJob::error_code` — set by `fail_index_job_with_error` on the
/// engine side of either transport). Under `StrictExit` this is threaded
/// through `Error::from_code` to reconstruct the *original* typed error
/// (e.g. `Error::InvalidConfig`, exit 2) instead of always collapsing to
/// `Error::Internal` (exit 1) — the transport-parity fix for issue #187
/// review finding 3: an embedded pre-flight failure (e.g. embedder
/// construction in `run_embedded_store_job`, caught before the job is even
/// submitted) already surfaced its typed error directly; a daemon-attached
/// job reached this function with only a stringified message and lost that
/// classification. `error_code: None` (a synthetic queue-level failure, or
/// an older daemon predating this field) falls back to the historical
/// `Error::Internal` behavior unchanged.
fn finish_job(
    mode: IndexErrorMode,
    context: &str,
    state: IndexJobState,
    stats: IndexJobStats,
    error: Option<String>,
    error_code: Option<String>,
) -> Result<IndexSummary, Error> {
    match state {
        IndexJobState::Done => Ok(IndexSummary::from_job_stats(stats)),
        IndexJobState::Failed => {
            let msg = error.unwrap_or_else(|| "index job failed".to_string());
            if mode.warn() {
                eprintln!("warning: {context}: {msg}");
                Ok(IndexSummary::default())
            } else {
                let typed = error_code
                    .as_deref()
                    .and_then(|code| Error::from_code(code, msg.clone()));
                Err(typed.unwrap_or_else(|| Error::Internal {
                    message: format!("{context}: {msg}"),
                    correlation_id: "index_job_failed".to_string(),
                }))
            }
        }
        _ => Err(Error::Internal {
            message: format!("{context}: job ended in a non-terminal state"),
            correlation_id: "index_job_nonterminal".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    use axum::extract::{Path as AxumPath, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::Router;
    use tempfile::TempDir;

    use localdb_core::config::schema::RawConfig;
    use localdb_core::{SourceKind, SourceRow};

    use crate::app_db::{load_config_scaffolded, open_app_db_or_exit};
    use crate::cmds::store::run_store_add_async;

    /// `--json` mode must expose the submitted job
    /// id on stderr *before* attach blocks — as one parseable JSON line —
    /// or a machine caller can never reach `localdb job cancel <id>` in
    /// time. Human mode keeps the pre-existing cancel hint verbatim.
    #[test]
    fn pre_attach_job_id_line_shapes() {
        // JSON, single store: exactly {"job_id": ...}, parseable.
        let line = pre_attach_job_id_line(true, "job-1", None);
        let v: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert_eq!(v, serde_json::json!({ "job_id": "job-1" }));

        // JSON, multi-store: a `store` field disambiguates, mirroring the
        // human hint's `[label]` prefix.
        let line = pre_attach_job_id_line(true, "job-2", Some("books"));
        let v: serde_json::Value = serde_json::from_str(&line).expect("must be valid JSON");
        assert_eq!(
            v,
            serde_json::json!({ "job_id": "job-2", "store": "books" })
        );

        // Human, both label shapes: pinned wording.
        assert_eq!(
            pre_attach_job_id_line(false, "job-3", None),
            "job job-3 (cancel with: localdb job cancel job-3)"
        );
        assert_eq!(
            pre_attach_job_id_line(false, "job-3", Some("books")),
            "[books] job job-3 (cancel with: localdb job cancel job-3)"
        );
    }

    fn test_ctx() -> CliContext {
        CliContext {
            config: None,
            json: false,
            stores: vec![],
            yes: false,
            daemon_url: None,
            config_env: None,
        }
    }

    fn sample_job(id: &str, state: IndexJobState) -> IndexJob {
        IndexJob {
            id: id.to_string(),
            store_id: "store-x".to_string(),
            scope: IndexJobScope::Store,
            state,
            stats: IndexJobStats::default(),
            error: None,
            error_code: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            started_at: None,
            completed_at: None,
        }
    }

    // -----------------------------------------------------------------
    // Embedded transport: a real AppDb/ConfigLoader, `provider: fake`
    // (offline, no model download) — mirrors
    // `cmds::source::tests::source_add_across_two_stores_builds_embedder_once`.
    // -----------------------------------------------------------------

    async fn test_config_and_db() -> (TempDir, ConfigLoader, crate::app_db::AppDb, CliContext) {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
                dir.path().display()
            ),
        )
        .unwrap();
        let ctx = CliContext {
            config: Some(config_path),
            json: false,
            stores: vec![],
            yes: false,
            daemon_url: None,
            config_env: None,
        };
        let config_loader = load_config_scaffolded(&ctx).await;
        let db = open_app_db_or_exit(&ctx, &config_loader).await;
        (dir, config_loader, db, ctx)
    }

    /// `run_embedded_store_job`'s own `resolve_job_sources` call — not the
    /// caller-side pre-filter `cmds::index::IndexCmd::run_embedded` does for
    /// `localdb index --source` — must surface an unresolvable scope
    /// (unknown source id) as `Err` under `StrictExit` and as a warned,
    /// defaulted `IndexSummary` under `WarnAndContinue`. Reachable in
    /// practice via `cmds::source`'s auto-index, which narrows its scope to
    /// exactly the source id it just created.
    #[tokio::test]
    async fn run_embedded_store_job_reports_an_unresolvable_scope_strict_and_warn() {
        let (_dir, config_loader, db, ctx) = test_config_and_db().await;
        run_store_add_async(&ctx, "docs").await;
        let store = db
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();
        let queue = JobQueue::new();
        let mut embedder: Option<Arc<dyn Embedder>> = None;

        let unknown_scope = IndexJobScope::Source {
            source_id: "01HRQHB7FN3WMX4AZDV3S9VCTZ".to_string(),
        };

        let err = run_embedded_store_job(
            &ctx,
            &queue,
            &config_loader,
            &db,
            &store,
            unknown_scope.clone(),
            DeletionPolicy::Retain,
            IndexErrorMode::StrictExit,
            &mut embedder,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::SourceNotFound { .. }),
            "expected SourceNotFound, got: {err:?}"
        );

        let (summary, job_id) = run_embedded_store_job(
            &ctx,
            &queue,
            &config_loader,
            &db,
            &store,
            unknown_scope,
            DeletionPolicy::Retain,
            IndexErrorMode::WarnAndContinue,
            &mut embedder,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            summary,
            IndexSummary::default(),
            "WarnAndContinue must swallow the same failure into a defaulted summary"
        );
        assert_eq!(
            job_id, None,
            "no job was ever submitted for an unresolvable scope"
        );
    }

    /// A source row with a preset the CLI itself never writes (always
    /// `"prose"` — only reachable by inserting the row directly, exactly as
    /// `index_reports_error_for_source_with_invalid_chunker_preset` in
    /// `localdb/tests/cli_integration.rs` does for the `StrictExit` side of
    /// this same failure) must be counted as a per-source error and
    /// otherwise ignored under `source add`'s `WarnAndContinue` mode —
    /// exercising `emit_source_error`'s warn-mode `InvalidChunkerPreset`
    /// branch, which nothing else in this crate's test suite reaches (every
    /// other invalid-preset test drives `index`'s `StrictExit` mode
    /// instead).
    #[tokio::test]
    async fn run_embedded_store_job_warns_and_continues_on_an_invalid_chunker_preset() {
        // Held for the rest of this test:
        // this test drives a real `embed::create_embedder` build as a side
        // effect of the call below, incrementing the same process-wide
        // `EMBEDDER_BUILD_COUNT` that
        // `cmds::source::tests::source_add_across_two_stores_builds_embedder_once`
        // measures — without this lock, `cargo test`'s default parallel
        // execution can interleave that increment into the other test's
        // measurement window. See `EMBEDDER_BUILD_COUNT_TEST_LOCK`'s doc
        // comment in `cmds::index`.
        let _embedder_count_guard = crate::cmds::index::EMBEDDER_BUILD_COUNT_TEST_LOCK
            .lock()
            .await;

        let (_dir, config_loader, db, ctx) = test_config_and_db().await;
        run_store_add_async(&ctx, "docs").await;
        let store = db
            .backend()
            .get_store_by_name("docs")
            .await
            .unwrap()
            .unwrap();

        db.backend()
            .upsert_source(&SourceRow {
                id: "01HRQHB7FN3WMX4AZDV3S9VCTZ".to_string(),
                store_id: store.id.clone(),
                kind: SourceKind::Path,
                root: Some("/nonexistent-root".to_string()),
                url: None,
                include: vec![],
                exclude: vec![],
                preset: "not-a-real-preset".to_string(),
                refresh: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                config_json: None,
                feed_etag: None,
                feed_last_modified: None,
            })
            .await
            .unwrap();

        let queue = JobQueue::new();
        let mut embedder: Option<Arc<dyn Embedder>> = None;
        let (summary, job_id) = run_embedded_store_job(
            &ctx,
            &queue,
            &config_loader,
            &db,
            &store,
            IndexJobScope::Store,
            DeletionPolicy::Retain,
            IndexErrorMode::WarnAndContinue,
            &mut embedder,
            None,
        )
        .await
        .unwrap();

        let expected = IndexSummary::from_job_stats(IndexJobStats {
            sources_count: 1,
            error_count: 1,
            ..Default::default()
        });
        assert_eq!(
            summary, expected,
            "the invalid preset must count as exactly one source error, never abort the run"
        );
        assert!(
            job_id.is_some(),
            "a job did get submitted here (it just reported a per-source error)"
        );
    }

    /// A task that floods far more than one broadcast channel's worth of
    /// progress events with no `.await` in between must not deadlock or
    /// panic `drive_embedded_job` — the receiver's first `recv()` observes
    /// `RecvError::Lagged` and skips ahead (issue #83's lossy-tolerant
    /// progress contract), rather than every individual event.
    #[tokio::test]
    async fn drive_embedded_job_skips_ahead_past_a_lagging_progress_receiver() {
        let queue = JobQueue::new();
        let job = queue
            .submit("store-1", IndexJobScope::Store, |progress| async move {
                for i in 0..2000u32 {
                    progress(ProgressEvent::Discovered { total: i as usize });
                }
                Ok(IndexJobStats::default())
            })
            .await
            .unwrap();

        let final_job = drive_embedded_job(&queue, &job.id, true, None).await;
        assert_eq!(final_job.state, IndexJobState::Done);
    }

    // -----------------------------------------------------------------
    // Daemon transport: a real `server::build_router` instance on a real
    // ephemeral loopback listener (mirrors `server/tests/mcp_route.rs`).
    // -----------------------------------------------------------------

    fn fake_daemon_yaml() -> RawConfig {
        RawConfig {
            defaults: localdb_core::config::schema::DefaultsConfig {
                indexing: localdb_core::config::schema::IndexingPolicyConfig {
                    chunking: Default::default(),
                    embedding: localdb_core::config::schema::EmbeddingPolicy {
                        provider: "fake".to_string(),
                        model: "default".to_string(),
                    },
                    ..Default::default()
                },
            },
            ..Default::default()
        }
    }

    async fn spawn_real_daemon() -> (TempDir, server::AppState, String) {
        let dir = tempfile::tempdir().unwrap();
        let queue = JobQueue::new();
        let state = server::AppState::new(
            fake_daemon_yaml(),
            dir.path().to_path_buf(),
            dir.path().join("models"),
            queue.clone(),
            server::UrlRefreshScheduler::new(queue),
        )
        .await
        .unwrap();
        let embedder: Arc<dyn Embedder> = Arc::new(localdb_core::FakeEmbedder::new(128));
        let router = server::build_router(state.clone(), vec![], embedder, vec![]);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        (dir, state, format!("http://{addr}"))
    }

    #[tokio::test]
    async fn run_daemon_store_job_reports_submission_failure_strict_and_warn() {
        let (_dir, _state, base_url) = spawn_real_daemon().await;
        let ctx = test_ctx();

        let err = run_daemon_store_job(
            &ctx,
            &base_url,
            "nonexistent-store",
            None,
            DeletionPolicy::Retain,
            IndexErrorMode::StrictExit,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::StoreNotFound { ref id } if id.contains("nonexistent-store")),
            "expected StoreNotFound, got: {err:?}"
        );

        let (summary, job_id) = run_daemon_store_job(
            &ctx,
            &base_url,
            "nonexistent-store",
            None,
            DeletionPolicy::Retain,
            IndexErrorMode::WarnAndContinue,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary, IndexSummary::default());
        assert_eq!(
            job_id, None,
            "submission itself failed, so no job id was ever assigned"
        );
    }

    #[tokio::test]
    async fn attach_daemon_job_follows_a_real_job_via_sse_to_completion() {
        let (_dir, state, base_url) = spawn_real_daemon().await;
        let job = state
            .job_queue()
            .submit("store-x", IndexJobScope::Store, |_progress| async {
                Ok(IndexJobStats {
                    docs_indexed: 3,
                    ..Default::default()
                })
            })
            .await
            .unwrap();

        let final_job = attach_daemon_job(&base_url, &job.id, false, None)
            .await
            .unwrap();
        assert_eq!(final_job.state, IndexJobState::Done);
        assert_eq!(final_job.stats.docs_indexed, 3);
    }

    #[tokio::test]
    async fn try_attach_via_sse_forwards_live_progress_events_to_the_sink() {
        let (_dir, state, base_url) = spawn_real_daemon().await;
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        let job = state
            .job_queue()
            .submit(
                "store-x",
                IndexJobScope::Store,
                move |progress| async move {
                    // Wait for the release signal *before* emitting progress —
                    // otherwise the task (picked up by the worker almost
                    // immediately) can send its event before this test's SSE
                    // client has finished the connect+subscribe handshake,
                    // silently dropping it (broadcast channels don't replay to
                    // late subscribers).
                    let _ = release_rx.await;
                    progress(ProgressEvent::Discovered { total: 7 });
                    Ok(IndexJobStats::default())
                },
            )
            .await
            .unwrap();

        let recorded: Arc<StdMutex<Vec<ProgressEvent>>> = Arc::new(StdMutex::new(Vec::new()));
        let recorded_for_sink = recorded.clone();
        let sink: ProgressSink = Arc::new(move |e: ProgressEvent| {
            recorded_for_sink.lock().unwrap().push(e);
        });

        let job_id = job.id.clone();
        let attach_task =
            tokio::spawn(async move { try_attach_via_sse(&base_url, &job_id, Some(&sink)).await });

        // Give the SSE stream a moment to connect and receive the progress
        // event before releasing the task to complete.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        release_tx.send(()).unwrap();

        let final_job = match attach_task.await.unwrap() {
            Ok(job) => job,
            Err(_) => panic!("expected the terminal job frame, not a fallback"),
        };
        assert_eq!(final_job.state, IndexJobState::Done);

        let recorded = recorded.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "expected exactly one forwarded progress event"
        );
        match &recorded[0] {
            ProgressEvent::Discovered { total } => assert_eq!(*total, 7),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Daemon transport: a small hand-rolled mock router for wire-shape
    // edge cases the real daemon can't be coaxed into producing (a
    // malformed `/v1/jobs` response, a dropped SSE stream, a malformed
    // poll body).
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct MockResponses {
        jobs_post: VecDeque<(u16, String)>,
        events: VecDeque<(u16, String)>,
        poll: VecDeque<(u16, String)>,
    }

    type SharedMockResponses = Arc<tokio::sync::Mutex<MockResponses>>;

    async fn mock_jobs_post(
        State(state): State<SharedMockResponses>,
        _body: axum::body::Bytes,
    ) -> (StatusCode, String) {
        let (code, body) = state
            .lock()
            .await
            .jobs_post
            .pop_front()
            .unwrap_or((500, "{}".to_string()));
        (StatusCode::from_u16(code).unwrap(), body)
    }

    async fn mock_events(
        State(state): State<SharedMockResponses>,
        AxumPath(_id): AxumPath<String>,
    ) -> (StatusCode, String) {
        let (code, body) = state
            .lock()
            .await
            .events
            .pop_front()
            .unwrap_or((404, String::new()));
        (StatusCode::from_u16(code).unwrap(), body)
    }

    async fn mock_poll(
        State(state): State<SharedMockResponses>,
        AxumPath(_id): AxumPath<String>,
    ) -> (StatusCode, String) {
        let (code, body) = state
            .lock()
            .await
            .poll
            .pop_front()
            .unwrap_or((500, "{}".to_string()));
        (StatusCode::from_u16(code).unwrap(), body)
    }

    async fn spawn_mock_daemon(responses: MockResponses) -> String {
        let state: SharedMockResponses = Arc::new(tokio::sync::Mutex::new(responses));
        let router = Router::new()
            .route("/v1/jobs", post(mock_jobs_post))
            .route("/v1/jobs/{id}/events", get(mock_events))
            .route("/v1/jobs/{id}", get(mock_poll))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn run_daemon_store_job_reports_a_malformed_submit_response_strict_and_warn() {
        let ctx = test_ctx();

        let mut strict_responses = MockResponses::default();
        strict_responses
            .jobs_post
            .push_back((202, r#"{"status":"accepted"}"#.to_string()));
        let base_url = spawn_mock_daemon(strict_responses).await;
        let err = run_daemon_store_job(
            &ctx,
            &base_url,
            "store-x",
            None,
            DeletionPolicy::Retain,
            IndexErrorMode::StrictExit,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::Internal { ref message, .. } if message.contains("missing 'id'")),
            "expected the missing-id internal error, got: {err:?}"
        );

        let mut warn_responses = MockResponses::default();
        warn_responses
            .jobs_post
            .push_back((202, r#"{"status":"accepted"}"#.to_string()));
        let base_url = spawn_mock_daemon(warn_responses).await;
        let (summary, job_id) = run_daemon_store_job(
            &ctx,
            &base_url,
            "store-x",
            None,
            DeletionPolicy::Retain,
            IndexErrorMode::WarnAndContinue,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary, IndexSummary::default());
        assert_eq!(
            job_id, None,
            "the malformed submission response never yielded a usable job id"
        );
    }

    #[tokio::test]
    async fn attach_daemon_job_falls_back_to_polling_when_the_sse_route_404s() {
        let done_job = sample_job("job-1", IndexJobState::Done);
        let mut responses = MockResponses::default();
        responses.events.push_back((404, String::new()));
        responses
            .poll
            .push_back((200, serde_json::to_string(&done_job).unwrap()));
        let base_url = spawn_mock_daemon(responses).await;

        let job = attach_daemon_job(&base_url, "job-1", false, None)
            .await
            .unwrap();
        assert_eq!(job.state, IndexJobState::Done);
    }

    // -----------------------------------------------------------------
    // split_next_line: pure helper, unit-tested directly (Codex review
    // finding F3 — the SSE byte stream must be buffered as `Vec<u8>` and
    // decoded only on completed lines, so a multi-byte UTF-8 character
    // split across two network chunks doesn't get corrupted into
    // replacement characters).
    // -----------------------------------------------------------------

    /// `é` (U+00E9) encodes as the two UTF-8 bytes `0xC3 0xA9`. Pushing them
    /// via two separate `extend_from_slice` calls (mirroring two arriving
    /// network chunks) before the terminating `\n` must still decode to an
    /// intact `é` once the line completes — decoding only happens after the
    /// full line is buffered, never per-partial-chunk.
    #[test]
    fn split_next_line_reassembles_a_multibyte_char_split_across_chunks() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&[0xC3]); // first byte of 'é', arriving alone
        buf.extend_from_slice(&[0xA9]); // second byte, arriving in the next chunk
        buf.extend_from_slice(b"\n");

        let line = split_next_line(&mut buf).expect("a completed line");
        assert_eq!(
            line, "é",
            "the split multi-byte character must decode intact"
        );
        assert!(
            buf.is_empty(),
            "the consumed line (including its newline) must be drained from buf"
        );
    }

    /// A buffer with no `\n` yet — the tail of a line still arriving —
    /// yields no line and is left untouched, so a later chunk can complete
    /// it.
    #[test]
    fn split_next_line_returns_none_for_a_trailing_partial_line_with_no_newline() {
        let mut buf: Vec<u8> = b"event: job".to_vec();
        assert!(
            split_next_line(&mut buf).is_none(),
            "a line with no terminating newline yet must not be considered complete"
        );
        assert_eq!(
            buf, b"event: job",
            "buf must be left untouched when no complete line is available"
        );
    }

    /// CRLF line endings strip the trailing `\r`, matching the parser's
    /// pre-existing `trim_end_matches('\r')` behavior.
    #[test]
    fn split_next_line_strips_a_trailing_carriage_return() {
        let mut buf: Vec<u8> = b"data: hello\r\n".to_vec();
        let line = split_next_line(&mut buf).expect("a completed line");
        assert_eq!(line, "data: hello");
    }

    /// Pins the trailing-partial-line-at-EOF behavior at the full
    /// `attach_daemon_job` flow level: an SSE stream that ends mid-line,
    /// with no trailing newline at all, must not panic and must degrade to
    /// the polling fallback exactly like any other stream that ends without
    /// a terminal `job` frame.
    #[tokio::test]
    async fn attach_daemon_job_falls_back_to_polling_when_the_sse_stream_ends_mid_line_without_a_trailing_newline(
    ) {
        let running_job = sample_job("job-7", IndexJobState::Running);
        let done_job = sample_job("job-7", IndexJobState::Done);
        let mut responses = MockResponses::default();
        // The connection ends mid-line, with no trailing "\n" at all.
        responses
            .events
            .push_back((200, "event: progress\ndata: {\"type\":\"disc".to_string()));
        responses
            .poll
            .push_back((200, serde_json::to_string(&running_job).unwrap()));
        responses
            .poll
            .push_back((200, serde_json::to_string(&done_job).unwrap()));
        let base_url = spawn_mock_daemon(responses).await;

        let job = attach_daemon_job(&base_url, "job-7", false, None)
            .await
            .unwrap();
        assert_eq!(job.state, IndexJobState::Done);
    }

    #[tokio::test]
    async fn attach_daemon_job_falls_back_to_polling_when_the_sse_stream_ends_without_a_terminal_frame(
    ) {
        let running_job = sample_job("job-2", IndexJobState::Running);
        let done_job = sample_job("job-2", IndexJobState::Done);
        let mut responses = MockResponses::default();
        // A 200 response, but the connection just ends without ever
        // sending an `event: job` frame.
        responses.events.push_back((
            200,
            "event: progress\ndata: {\"type\":\"discovered\",\"total\":2}\n\n".to_string(),
        ));
        responses
            .poll
            .push_back((200, serde_json::to_string(&running_job).unwrap()));
        responses
            .poll
            .push_back((200, serde_json::to_string(&done_job).unwrap()));
        let base_url = spawn_mock_daemon(responses).await;

        let job = attach_daemon_job(&base_url, "job-2", false, None)
            .await
            .unwrap();
        assert_eq!(job.state, IndexJobState::Done);
    }

    #[tokio::test]
    async fn attach_daemon_job_poll_fallback_surfaces_a_malformed_job_status_as_an_error() {
        let mut responses = MockResponses::default();
        responses.events.push_back((404, String::new()));
        responses.poll.push_back((200, "not json".to_string()));
        let base_url = spawn_mock_daemon(responses).await;

        let err = attach_daemon_job(&base_url, "job-3", false, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Internal { ref message, .. } if message.contains("cannot parse job status")),
            "expected the poll-parse internal error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn run_daemon_store_job_reports_an_attach_failure_strict_and_warn() {
        let ctx = test_ctx();

        let mut strict_responses = MockResponses::default();
        strict_responses
            .jobs_post
            .push_back((202, serde_json::json!({ "id": "job-4" }).to_string()));
        strict_responses.events.push_back((404, String::new()));
        strict_responses.poll.push_back((500, "boom".to_string()));
        let base_url = spawn_mock_daemon(strict_responses).await;
        let err = run_daemon_store_job(
            &ctx,
            &base_url,
            "store-x",
            None,
            DeletionPolicy::Retain,
            IndexErrorMode::StrictExit,
            None,
        )
        .await
        .unwrap_err();

        let mut warn_responses = MockResponses::default();
        warn_responses
            .jobs_post
            .push_back((202, serde_json::json!({ "id": "job-5" }).to_string()));
        warn_responses.events.push_back((404, String::new()));
        warn_responses.poll.push_back((500, "boom".to_string()));
        let base_url = spawn_mock_daemon(warn_responses).await;
        let (summary, job_id) = run_daemon_store_job(
            &ctx,
            &base_url,
            "store-x",
            None,
            DeletionPolicy::Retain,
            IndexErrorMode::WarnAndContinue,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary, IndexSummary::default());
        assert_eq!(
            job_id.as_deref(),
            Some("job-5"),
            "the job id is known even though attaching to it failed"
        );
        // Both instances must have propagated the same underlying attach
        // failure (a daemon error surfaced from the failed poll), just
        // under different mode semantics.
        assert!(!matches!(err, Error::StoreNotFound { .. }));
    }

    #[tokio::test]
    async fn try_attach_via_sse_folds_multiline_data_and_ignores_unknown_event_types() {
        let done_job = sample_job("job-6", IndexJobState::Done);
        let body = format!(
            "event: heartbeat\ndata: {{\"ping\":true}}\n\nevent: progress\ndata: {{\"type\":\"discovered\",\ndata: \"total\":9}}\n\nevent: job\ndata: {}\n\n",
            serde_json::to_string(&done_job).unwrap()
        );
        let mut responses = MockResponses::default();
        responses.events.push_back((200, body));
        let base_url = spawn_mock_daemon(responses).await;

        let recorded: Arc<StdMutex<Vec<ProgressEvent>>> = Arc::new(StdMutex::new(Vec::new()));
        let recorded_for_sink = recorded.clone();
        let sink: ProgressSink = Arc::new(move |e: ProgressEvent| {
            recorded_for_sink.lock().unwrap().push(e);
        });

        let job = match try_attach_via_sse(&base_url, "job-6", Some(&sink)).await {
            Ok(job) => job,
            Err(_) => panic!("expected the terminal job frame, not a fallback"),
        };
        assert_eq!(job.state, IndexJobState::Done);

        let recorded = recorded.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "the unrecognized 'heartbeat' event must be ignored; only the \
             folded multi-line progress event should be recorded"
        );
        match &recorded[0] {
            ProgressEvent::Discovered { total } => assert_eq!(*total, 9),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // finish_job: pure function, unit-tested directly.
    // -----------------------------------------------------------------

    #[test]
    fn finish_job_failed_under_warn_mode_prints_and_defaults() {
        let summary = finish_job(
            IndexErrorMode::WarnAndContinue,
            "auto-index",
            IndexJobState::Failed,
            IndexJobStats {
                docs_indexed: 5,
                ..Default::default()
            },
            Some("boom".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(
            summary,
            IndexSummary::default(),
            "a warned failure must fold to a defaulted summary, discarding partial stats"
        );
    }

    #[test]
    fn finish_job_failed_under_strict_mode_errors_with_context_and_message() {
        let err = finish_job(
            IndexErrorMode::StrictExit,
            "auto-index job for store 'docs'",
            IndexJobState::Failed,
            IndexJobStats::default(),
            Some("boom".to_string()),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Internal { ref message, .. }
                if message.contains("auto-index job for store 'docs'") && message.contains("boom")),
            "expected a contextualized failure message, got: {err:?}"
        );
    }

    #[test]
    fn finish_job_failed_with_no_error_text_falls_back_to_a_generic_message() {
        let err = finish_job(
            IndexErrorMode::StrictExit,
            "auto-index",
            IndexJobState::Failed,
            IndexJobStats::default(),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Internal { ref message, .. } if message.contains("index job failed")),
            "expected the generic fallback message, got: {err:?}"
        );
    }

    /// Issue #187 review, finding 3: a failed job carrying a recognized
    /// `error_code` (e.g. `invalid_config`, the code an embedder-construction
    /// failure classifies as) must reconstruct the *original* typed error
    /// under `StrictExit` — not collapse to `Error::Internal` — so a
    /// daemon-attached failure exits with the same code (2) an embedded
    /// pre-flight failure of the same kind already does (see
    /// `run_embedded_store_job`'s doc comment and
    /// `index_embedder_creation_failure_exits_2` in
    /// `localdb/tests/cli_integration.rs`).
    #[test]
    fn finish_job_failed_with_a_recognized_error_code_reconstructs_the_typed_error() {
        let err = finish_job(
            IndexErrorMode::StrictExit,
            "auto-index job for store 'docs'",
            IndexJobState::Failed,
            IndexJobStats::default(),
            Some("unconfigured embedder provider".to_string()),
            Some("invalid_config".to_string()),
        )
        .unwrap_err();
        assert_eq!(
            err,
            Error::InvalidConfig {
                message: "unconfigured embedder provider".to_string()
            },
            "expected the original typed error reconstructed via Error::from_code, got: {err:?}"
        );
        assert_eq!(err.exit_code(), 2);
    }

    /// Round-trip through the *real* producer, not a hand-typed bare
    /// message: `localdb_core::fail_index_job_with_error` populates
    /// `job.error`/`job.error_code` exactly as the daemon worker does
    /// (`server::job_queue::run_worker`), and this feeds that output
    /// straight into `finish_job`'s reconstruction. Guards against the
    /// producer/consumer prefix-doubling regression (issue #187 review,
    /// finding F4): before the fix, the producer stored `error.to_string()`
    /// ("invalid config: unconfigured embedder provider"), and
    /// `Error::from_code` re-added the same prefix on reconstruction,
    /// doubling it in the final `Display`ed error.
    #[test]
    fn finish_job_round_trips_fail_index_job_with_error_output_without_doubling_the_prefix() {
        use localdb_core::fail_index_job_with_error;

        let mut job = IndexJob {
            id: "job-1".to_string(),
            store_id: "store-1".to_string(),
            scope: IndexJobScope::Store,
            state: IndexJobState::Running,
            stats: IndexJobStats::default(),
            error: None,
            error_code: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            started_at: None,
            completed_at: None,
        };
        let source_err = Error::InvalidConfig {
            message: "unconfigured embedder provider".to_string(),
        };
        fail_index_job_with_error(&mut job, &source_err);

        let err = finish_job(
            IndexErrorMode::StrictExit,
            "auto-index job for store 'docs'",
            job.state,
            job.stats,
            job.error,
            job.error_code,
        )
        .unwrap_err();
        assert_eq!(err, source_err);
        let rendered = err.to_string();
        assert_eq!(
            rendered.matches("invalid config:").count(),
            1,
            "the \"invalid config: \" prefix must appear exactly once, got: {rendered:?}"
        );
    }

    /// An `error_code` this binary doesn't recognize (e.g. a newer daemon)
    /// must fall back to the historical contextualized `Error::Internal`,
    /// exactly like `error_code: None` — never panic or silently drop the
    /// message.
    #[test]
    fn finish_job_failed_with_an_unrecognized_error_code_falls_back_to_internal() {
        let err = finish_job(
            IndexErrorMode::StrictExit,
            "auto-index job for store 'docs'",
            IndexJobState::Failed,
            IndexJobStats::default(),
            Some("boom".to_string()),
            Some("some_future_code".to_string()),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Internal { ref message, .. }
                if message.contains("auto-index job for store 'docs'") && message.contains("boom")),
            "expected the Internal fallback, got: {err:?}"
        );
    }

    /// `finish_job`'s defensive catch-all: a job somehow observed in a
    /// non-terminal state (`Pending`/`Running`) by the time a caller folds
    /// it into a summary is always an `Err`, regardless of `mode` — this
    /// state never occurs from either real call site (`attach_daemon_job`/
    /// `drive_embedded_job` only ever return a job once it's `Done` or
    /// `Failed`), so it's exercised here directly rather than through a
    /// contrived end-to-end scenario.
    #[test]
    fn finish_job_non_terminal_state_is_always_an_error_even_under_warn_mode() {
        let err = finish_job(
            IndexErrorMode::WarnAndContinue,
            "auto-index",
            IndexJobState::Running,
            IndexJobStats::default(),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::Internal { ref message, .. } if message.contains("non-terminal state")),
            "expected the defensive non-terminal-state error, got: {err:?}"
        );
    }
}
