//! `localdb job cancel`/`localdb job list` — manage jobs on a daemon's job
//! queue (issue #218).
//!
//! Daemon-only, unlike every other dual-transport command in this crate:
//! there is no meaningful embedded equivalent. The CLI's embedded indexing
//! path (`cli::job_attach::run_embedded_store_job`) spins up a throwaway
//! `JobQueue` that lives and dies inside a single `localdb index`
//! invocation — there is no separate, longer-lived process a `job cancel`/
//! `job list` command could ever reach. Both require a running daemon and
//! exit 5 (`daemon_unreachable`) without one, the same outcome every other
//! daemon-only path in this crate gives.

use localdb_core::{Error, IndexJob};

use crate::app_db::{load_config_scaffolded, reject_store_flag};
use crate::daemon_client::{
    daemon_request_async, encode_path_segment, probe_daemon, CliContext, DaemonState,
};
use crate::normalize::{exit_err, print_json};

/// `--store` is rejected outright: a job id is already globally unique, and
/// unlike a write such as `store add` there is no "which store does this
/// land in" ambiguity a default could resolve — the flag would just be
/// silently ignored, which is worse than refusing it.
const JOB_CANCEL_REJECT_MESSAGE: &str =
    "`job cancel` operates on a job by ID, not by store; --store is not applicable";

/// `job list` spans every job on the daemon's queue regardless of store —
/// same reasoning as `JOB_CANCEL_REJECT_MESSAGE`, `--store` would just be
/// silently ignored.
const JOB_LIST_REJECT_MESSAGE: &str =
    "`job list` shows every job regardless of store; --store is not applicable";

/// Resolve the daemon's base URL for a daemon-only command (`job cancel`,
/// `job list`).
///
/// When `ctx.daemon_url` is set (`LOCALDB_DAEMON_URL`), `probe_daemon`
/// already treats it as authoritative and never touches `data_dir` at all
/// (see its doc comment) — so this skips loading the local config entirely
/// in that case, going straight to the daemon client. Config is loaded only
/// when socket discovery is actually needed (no override), to get
/// `paths.data_dir`.
///
/// Before this, both commands called `load_config_scaffolded` first,
/// unconditionally — so a broken local `config.yaml` (unwritable, invalid,
/// wrong schema version) could `exit_err` before the daemon override was
/// ever consulted, blocking a command whose whole point was reaching a
/// *remote* daemon that never needed the local config at all.
async fn resolve_daemon_base_url_or_exit(ctx: &CliContext) -> String {
    if let Some(url) = ctx.daemon_url.as_deref() {
        return url.to_string();
    }
    let config_loader = load_config_scaffolded(ctx).await;
    // `ctx.daemon_url` is `None` here (handled above), so this call is
    // purely the socket-discovery path.
    match probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref()) {
        DaemonState::Running { base_url } => base_url,
        DaemonState::NotRunning => exit_err(&Error::DaemonUnreachable, ctx.json),
    }
}

/// `DELETE /v1/jobs/{id}` against a running daemon, parsing its response
/// back into the job's cancel-time snapshot. Factored out of
/// [`run_job_cancel_async`] so it's directly unit-testable against a real
/// `server::build_router` instance (mirroring
/// `cli::job_attach::attach_daemon_job`'s testing style) without going
/// through `exit_err`'s process-exiting error path.
pub(crate) async fn cancel_daemon_job(
    ctx: &CliContext,
    base_url: &str,
    id: &str,
) -> Result<IndexJob, Error> {
    // `id` is percent-encoded before it's interpolated into the URL path
    // segment — see `encode_path_segment`'s doc comment; same class of bug
    // as `store remove`/`source remove`'s DELETE call sites
    // (`cli/src/cmds/store.rs`, `cli/src/cmds/source.rs`), which this
    // mirrors.
    let url = format!("{base_url}/v1/jobs/{}", encode_path_segment(id));
    let v = daemon_request_async(ctx, reqwest::Method::DELETE, &url, None).await?;
    serde_json::from_value(v).map_err(|e| Error::Internal {
        message: format!("cannot parse job from daemon: {}", e),
        correlation_id: "daemon_job_cancel_parse".to_string(),
    })
}

/// `localdb job cancel <id>`
pub fn run_job_cancel(ctx: &CliContext, id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_job_cancel_async(ctx, id));
}

pub(crate) async fn run_job_cancel_async(ctx: &CliContext, id: &str) {
    reject_store_flag(ctx, JOB_CANCEL_REJECT_MESSAGE);

    let base_url = resolve_daemon_base_url_or_exit(ctx).await;

    match cancel_daemon_job(ctx, &base_url, id).await {
        Ok(job) => {
            if ctx.json {
                print_json(&serde_json::json!({
                    "status": "cancellation_requested",
                    "id": job.id,
                    "state": job.state,
                }));
            } else {
                println!(
                    "cancellation requested for job '{}' (state: {:?})",
                    job.id, job.state
                );
            }
        }
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// `GET /v1/jobs` against a running daemon, parsing the full job list.
pub(crate) async fn list_daemon_jobs(
    ctx: &CliContext,
    base_url: &str,
) -> Result<Vec<IndexJob>, Error> {
    let url = format!("{base_url}/v1/jobs");
    let v = daemon_request_async(ctx, reqwest::Method::GET, &url, None).await?;
    serde_json::from_value(v).map_err(|e| Error::Internal {
        message: format!("cannot parse job list from daemon: {}", e),
        correlation_id: "daemon_job_list_parse".to_string(),
    })
}

/// `localdb job list`
pub fn run_job_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_job_list_async(ctx));
}

pub(crate) async fn run_job_list_async(ctx: &CliContext) {
    reject_store_flag(ctx, JOB_LIST_REJECT_MESSAGE);

    let base_url = resolve_daemon_base_url_or_exit(ctx).await;

    match list_daemon_jobs(ctx, &base_url).await {
        Ok(jobs) => {
            if ctx.json {
                print_json(
                    &serde_json::to_value(&jobs)
                        .expect("Vec<IndexJob> is always JSON-serializable"),
                );
            } else {
                print_job_list_table(&jobs);
            }
        }
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// Column widths for [`print_job_list_table`], computed from the actual
/// rows so short ids/stores/states don't leave excess padding while long
/// ones still line up.
struct JobListWidths {
    id: usize,
    store: usize,
    state: usize,
    error_code: usize,
}

impl JobListWidths {
    fn compute(jobs: &[IndexJob]) -> Self {
        let mut w = JobListWidths {
            id: "ID".len(),
            store: "STORE".len(),
            state: "STATE".len(),
            error_code: "ERROR_CODE".len(),
        };
        for job in jobs {
            w.id = w.id.max(job.id.len());
            w.store = w.store.max(job.store_id.len());
            w.state = w.state.max(job_state_str(&job.state).len());
            w.error_code = w
                .error_code
                .max(job.error_code.as_deref().unwrap_or("-").len());
        }
        // A trailing gap after every column but the last (CREATED_AT),
        // matching the rest of this crate's plain-table conventions (e.g.
        // `cmds::source::store_column_width`).
        w.id += 2;
        w.store += 2;
        w.state += 2;
        w.error_code += 2;
        w
    }
}

/// Render `state` the same way it round-trips over JSON
/// (`#[serde(rename_all = "lowercase")]` on `IndexJobState`) rather than
/// Rust's `Debug` capitalization — keeps the table's `STATE` column
/// consistent with `--json` output and every other surface that renders
/// this field.
fn job_state_str(state: &localdb_core::IndexJobState) -> &'static str {
    use localdb_core::IndexJobState::*;
    match state {
        Pending => "pending",
        Running => "running",
        Done => "done",
        Failed => "failed",
    }
}

/// `localdb job list`'s plain-text table: id, store, state, error_code,
/// created_at.
fn print_job_list_table(jobs: &[IndexJob]) {
    if jobs.is_empty() {
        println!("No jobs.");
        return;
    }
    let w = JobListWidths::compute(jobs);
    println!(
        "{:<id_w$}{:<store_w$}{:<state_w$}{:<err_w$}CREATED_AT",
        "ID",
        "STORE",
        "STATE",
        "ERROR_CODE",
        id_w = w.id,
        store_w = w.store,
        state_w = w.state,
        err_w = w.error_code,
    );
    for job in jobs {
        println!(
            "{:<id_w$}{:<store_w$}{:<state_w$}{:<err_w$}{}",
            job.id,
            job.store_id,
            job_state_str(&job.state),
            job.error_code.as_deref().unwrap_or("-"),
            job.created_at,
            id_w = w.id,
            store_w = w.store,
            state_w = w.state,
            err_w = w.error_code,
        );
    }
}

#[cfg(test)]
mod tests;
