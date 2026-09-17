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

/// The job-shaped parameters [`run_embedded_store_job`] needs, bundled to
/// keep its own parameter list under clippy's arg-count lint — mirrors the
/// `JobExecDeps`/`SourceIngestionDeps` precedent (`server::job_exec`,
/// `core::ingestion::deps`) rather than growing yet another positional
/// argument. Transport-specific handles the function borrows or mutates
/// (`queue`, `config_loader`, `db`, `store_row`, `embedder`) stay separate
/// arguments — this struct is only "what job to run," not "with what."
pub(crate) struct RunEmbeddedStoreJobArgs<'a> {
    pub scope: IndexJobScope,
    pub deletion: DeletionPolicy,
    pub refetch: bool,
    pub mode: IndexErrorMode,
    pub progress_label: Option<&'a str>,
}

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
/// `refetch` is threaded straight into `job_exec::run_job`, unchanged — see
/// its doc comment. `source add`'s auto-index always passes `false`: a
/// newly-added source has no recheck-floor history to bypass.
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
pub(crate) async fn run_embedded_store_job(
    ctx: &CliContext,
    queue: &JobQueue,
    config_loader: &ConfigLoader,
    db: &AppDb,
    store_row: &StoreRow,
    embedder: &mut Option<Arc<dyn Embedder>>,
    request: RunEmbeddedStoreJobArgs<'_>,
) -> Result<(IndexSummary, Option<String>), Error> {
    let RunEmbeddedStoreJobArgs {
        scope,
        deletion,
        refetch,
        mode,
        progress_label,
    } = request;

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
                job_exec::run_job(&store_row_owned, scope_for_job, deletion, refetch, deps)
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

/// Fail unless the daemon at `base_url` advertises `refetch` support
/// (specs/05-surfaces.md "Daemon capability advertisement").
///
/// Mirrors `cli::cmds::search::require_daemon_search_filter_support` exactly:
/// absence is treated as unsupported — a daemon predating the `features`
/// field omits it entirely — because an older daemon would otherwise
/// silently drop the request's unknown `refetch` key (`CreateJobRequest` has
/// no `deny_unknown_fields`) and run an ordinary gated job while reporting
/// success, indistinguishable from a real `--refetch` run. Exits 5
/// (unavailable) — the daemon is running and healthy, it just cannot do what
/// was asked — and names the fix, since restarting it resolves this
/// permanently.
async fn require_daemon_refetch_support(ctx: &CliContext, base_url: &str) -> Result<(), Error> {
    let url = format!("{base_url}/v1/status");
    let status = daemon_request_async(ctx, reqwest::Method::GET, &url, None).await?;
    let supported = status
        .get("features")
        .and_then(|f| f.as_array())
        .is_some_and(|features| features.iter().any(|f| f.as_str() == Some("refetch")));

    if supported {
        return Ok(());
    }
    Err(Error::DaemonCapabilityUnavailable {
        message: "the running daemon predates --refetch and would silently run an ordinary \
                  gated job while reporting success; restart it (`localdb serve`) to use \
                  --refetch against a daemon, or stop it to index in embedded mode"
            .to_string(),
    })
}

/// The job-shaped parameters [`run_daemon_store_job`] needs, bundled for the
/// same reason as [`RunEmbeddedStoreJobArgs`] — see its doc comment.
/// Transport-specific handles (`ctx`, `base_url`, `store_name`) stay separate
/// arguments.
pub(crate) struct RunDaemonStoreJobArgs<'a> {
    pub source_id: Option<&'a str>,
    pub deletion: DeletionPolicy,
    pub refetch: bool,
    pub mode: IndexErrorMode,
    pub progress_label: Option<&'a str>,
}

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
pub(crate) async fn run_daemon_store_job(
    ctx: &CliContext,
    base_url: &str,
    store_name: &str,
    request: RunDaemonStoreJobArgs<'_>,
) -> Result<(IndexSummary, Option<String>), Error> {
    let RunDaemonStoreJobArgs {
        source_id,
        deletion,
        refetch,
        mode,
        progress_label,
    } = request;

    // Only paid for when `--refetch` is actually set — an ordinary gated run
    // never touches `/v1/status` for this. A daemon predating `refetch`
    // would otherwise silently ignore the request's unknown key and run a
    // normal gated job while reporting success, which is worse than
    // refusing outright: the caller believes the recheck floor was bypassed
    // when it never was.
    if refetch {
        if let Err(e) = require_daemon_refetch_support(ctx, base_url).await {
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
    }

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
    // Sent unconditionally (even `false`), mirroring `deletion_policy`
    // above: an explicit `false` is indistinguishable from the server's own
    // default, but sending it either way keeps this request body a
    // complete, self-describing snapshot of what the CLI asked for.
    body["refetch"] = serde_json::Value::Bool(refetch);

    let submit_url = format!("{}/v1/jobs", base_url);
    let job_json =
        match daemon_request_async(ctx, reqwest::Method::POST, &submit_url, Some(body)).await {
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

    let final_job = match attach_daemon_job(ctx, base_url, &job_id, ctx.json, progress_label).await
    {
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
    ctx: &CliContext,
    base_url: &str,
    job_id: &str,
    json_mode: bool,
    progress_label: Option<&str>,
) -> Result<IndexJob, Error> {
    let sink = crate::progress::build_progress_sink(json_mode, progress_label);
    match try_attach_via_sse(ctx, base_url, job_id, sink.as_ref()).await {
        Ok(job) => Ok(job),
        Err(SseAttachError::Fallback) => poll_job_until_terminal(ctx, base_url, job_id).await,
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
    ctx: &CliContext,
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
    let bearer = crate::daemon_client::ensure_fresh_bearer(ctx, base_url).await;
    let mut request = client.get(&url).header("Accept", "text/event-stream");
    if let Some(secret) = bearer {
        request = request.bearer_auth(secret);
    }
    let resp = request.send().await.map_err(|_| SseAttachError::Fallback)?;

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
async fn poll_job_until_terminal(
    ctx: &CliContext,
    base_url: &str,
    job_id: &str,
) -> Result<IndexJob, Error> {
    let url = format!("{}/v1/jobs/{}", base_url, encode_path_segment(job_id));
    loop {
        let v = daemon_request_async(ctx, reqwest::Method::GET, &url, None).await?;
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
mod tests;
