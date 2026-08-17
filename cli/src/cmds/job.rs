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
pub(crate) async fn cancel_daemon_job(base_url: &str, id: &str) -> Result<IndexJob, Error> {
    // `id` is percent-encoded before it's interpolated into the URL path
    // segment — see `encode_path_segment`'s doc comment; same class of bug
    // as `store remove`/`source remove`'s DELETE call sites
    // (`cli/src/cmds/store.rs`, `cli/src/cmds/source.rs`), which this
    // mirrors.
    let url = format!("{base_url}/v1/jobs/{}", encode_path_segment(id));
    let v = daemon_request_async(reqwest::Method::DELETE, &url, None).await?;
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

    match cancel_daemon_job(&base_url, id).await {
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
pub(crate) async fn list_daemon_jobs(base_url: &str) -> Result<Vec<IndexJob>, Error> {
    let url = format!("{base_url}/v1/jobs");
    let v = daemon_request_async(reqwest::Method::GET, &url, None).await?;
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

    match list_daemon_jobs(&base_url).await {
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
mod tests {
    use super::*;

    use std::sync::Arc;

    use localdb_core::{Embedder, IndexJobScope, IndexJobState, IndexJobStats};
    use server::JobQueue;

    /// Mirrors `cli::job_attach::tests::spawn_real_daemon`: a real
    /// `server::AppState`/`build_router` on an ephemeral loopback listener,
    /// so these tests exercise the actual HTTP wire round-trip (status
    /// codes, JSON error bodies) rather than calling `JobQueue::cancel`
    /// directly.
    async fn spawn_real_daemon() -> (tempfile::TempDir, server::AppState, String) {
        let dir = tempfile::tempdir().unwrap();
        let queue = JobQueue::new();
        let yaml = localdb_core::config::schema::RawConfig {
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
        };
        let state = server::AppState::new(
            yaml,
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
    async fn cancel_daemon_job_unknown_id_returns_job_not_found() {
        let (_dir, _state, base_url) = spawn_real_daemon().await;
        let err = cancel_daemon_job(&base_url, "nonexistent-job-id")
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::JobNotFound { ref id } if id == "nonexistent-job-id"),
            "expected JobNotFound, got: {err:?}"
        );
        assert_eq!(err.exit_code(), 3);
    }

    #[tokio::test]
    async fn cancel_daemon_job_on_a_running_job_succeeds_and_it_eventually_reaches_job_cancelled() {
        let (_dir, state, base_url) = spawn_real_daemon().await;

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (_never_tx, never_rx) = tokio::sync::oneshot::channel::<()>();
        let job = state
            .job_queue()
            .submit(
                "running-store",
                IndexJobScope::Store,
                move |_progress| async move {
                    let _ = started_tx.send(());
                    let _ = never_rx.await;
                    Ok(IndexJobStats::default())
                },
            )
            .await
            .unwrap();
        started_rx.await.unwrap();

        let snapshot = cancel_daemon_job(&base_url, &job.id).await.unwrap();
        assert_eq!(snapshot.id, job.id);

        // Poll until the job reaches its eventual terminal state — no wall
        // clock assertion, just a bounded poll (mirrors
        // `server::job_queue::tests::common::wait_for_done`).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let current = state.job_queue().get_job(&job.id).await.unwrap();
            if current.state == IndexJobState::Failed {
                assert_eq!(current.error_code.as_deref(), Some("job_cancelled"));
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("job did not reach a terminal state in time: {current:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn cancel_daemon_job_on_a_completed_job_returns_job_already_terminal() {
        let (_dir, state, base_url) = spawn_real_daemon().await;

        let job = state
            .job_queue()
            .submit("store-1", IndexJobScope::Store, |_progress| async {
                Ok(IndexJobStats::default())
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let current = state.job_queue().get_job(&job.id).await.unwrap();
            if current.state == IndexJobState::Done {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("job did not complete in time");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let err = cancel_daemon_job(&base_url, &job.id).await.unwrap_err();
        assert!(
            matches!(err, Error::JobAlreadyTerminal),
            "expected JobAlreadyTerminal, got: {err:?}"
        );
        assert_eq!(err.exit_code(), 4);
    }

    /// `job cancel --store` must be rejected before any daemon probe — a
    /// pure function of `ctx.stores`, so this only needs
    /// `reject_store_flag`'s underlying check, not a real daemon.
    #[test]
    fn job_cancel_reject_message_is_used_for_a_nonempty_store_scope() {
        use crate::app_db::reject_store_flag_inner;

        let ctx = CliContext {
            config: None,
            json: false,
            stores: vec!["notes".to_string()],
            yes: false,
            daemon_url: None,
            config_env: None,
        };
        let err = reject_store_flag_inner(&ctx, JOB_CANCEL_REJECT_MESSAGE).unwrap_err();
        assert_eq!(
            err,
            Error::InvalidRequest {
                message: JOB_CANCEL_REJECT_MESSAGE.to_string(),
            }
        );
        assert_eq!(err.exit_code(), 2);
    }

    // -----------------------------------------------------------------
    // `job list`
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn list_daemon_jobs_returns_every_job_across_stores() {
        let (_dir, state, base_url) = spawn_real_daemon().await;

        let job_a = state
            .job_queue()
            .submit("store-a", IndexJobScope::Store, |_progress| async {
                Ok(IndexJobStats::default())
            })
            .await
            .unwrap();
        let job_b = state
            .job_queue()
            .submit("store-b", IndexJobScope::Store, |_progress| async {
                Ok(IndexJobStats::default())
            })
            .await
            .unwrap();

        let jobs = list_daemon_jobs(&base_url).await.unwrap();
        let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
        assert!(
            ids.contains(&job_a.id.as_str()) && ids.contains(&job_b.id.as_str()),
            "expected both submitted jobs in the list, got: {ids:?}"
        );
    }

    #[test]
    fn job_state_str_matches_the_lowercase_serde_rename() {
        // `IndexJobState` serializes as lowercase (`#[serde(rename_all =
        // "lowercase")]`) — the table's STATE column must render the same
        // words, not Rust's `Debug` capitalization.
        assert_eq!(job_state_str(&IndexJobState::Pending), "pending");
        assert_eq!(job_state_str(&IndexJobState::Running), "running");
        assert_eq!(job_state_str(&IndexJobState::Done), "done");
        assert_eq!(job_state_str(&IndexJobState::Failed), "failed");
    }

    fn sample_job_for_list(id: &str, store_id: &str, error_code: Option<&str>) -> IndexJob {
        IndexJob {
            id: id.to_string(),
            store_id: store_id.to_string(),
            scope: IndexJobScope::Store,
            state: IndexJobState::Failed,
            stats: IndexJobStats::default(),
            error: None,
            error_code: error_code.map(str::to_string),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            started_at: None,
            completed_at: None,
        }
    }

    #[test]
    fn job_list_widths_use_the_longest_value_per_column_plus_two() {
        let jobs = vec![
            sample_job_for_list("01HRQHB7FN3WMX4AZDV3S9VCTZ", "books", Some("job_cancelled")),
            sample_job_for_list("short-id", "a-much-longer-store-name", None),
        ];
        let w = JobListWidths::compute(&jobs);
        assert_eq!(w.id, "01HRQHB7FN3WMX4AZDV3S9VCTZ".len() + 2);
        assert_eq!(w.store, "a-much-longer-store-name".len() + 2);
        assert_eq!(w.error_code, "job_cancelled".len() + 2);
    }

    #[test]
    fn job_list_widths_fall_back_to_header_lengths_when_empty() {
        let w = JobListWidths::compute(&[]);
        assert_eq!(w.id, "ID".len() + 2);
        assert_eq!(w.store, "STORE".len() + 2);
        assert_eq!(w.state, "STATE".len() + 2);
        assert_eq!(w.error_code, "ERROR_CODE".len() + 2);
    }

    /// `job list --store` must be rejected before any daemon probe, same as
    /// `job cancel --store`.
    #[test]
    fn job_list_reject_message_is_used_for_a_nonempty_store_scope() {
        use crate::app_db::reject_store_flag_inner;

        let ctx = CliContext {
            config: None,
            json: false,
            stores: vec!["notes".to_string()],
            yes: false,
            daemon_url: None,
            config_env: None,
        };
        let err = reject_store_flag_inner(&ctx, JOB_LIST_REJECT_MESSAGE).unwrap_err();
        assert_eq!(
            err,
            Error::InvalidRequest {
                message: JOB_LIST_REJECT_MESSAGE.to_string(),
            }
        );
        assert_eq!(err.exit_code(), 2);
    }

    // -----------------------------------------------------------------
    // Fix F: `LOCALDB_DAEMON_URL` must be honored before any local config
    // load, for both daemon-only commands.
    // -----------------------------------------------------------------

    /// A `--config` pointing at unparseable YAML that `load_config_scaffolded`
    /// would `exit_err` on if it were ever loaded — the fixture proving Fix
    /// F actually skips the config load when `ctx.daemon_url` is set.
    fn invalid_config_ctx(daemon_url: String) -> (tempfile::TempDir, CliContext) {
        let dir = tempfile::tempdir().unwrap();
        let bad_config_path = dir.path().join("config.yaml");
        std::fs::write(&bad_config_path, "version: [1\nthis is not valid: yaml").unwrap();
        let ctx = CliContext {
            config: Some(bad_config_path),
            json: false,
            stores: vec![],
            yes: false,
            daemon_url: Some(daemon_url),
            config_env: None,
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn job_cancel_with_daemon_url_override_succeeds_even_with_an_invalid_local_config() {
        let (_daemon_dir, state, base_url) = spawn_real_daemon().await;

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (_never_tx, never_rx) = tokio::sync::oneshot::channel::<()>();
        let job = state
            .job_queue()
            .submit(
                "store-1",
                IndexJobScope::Store,
                move |_progress| async move {
                    let _ = started_tx.send(());
                    let _ = never_rx.await;
                    Ok(IndexJobStats::default())
                },
            )
            .await
            .unwrap();
        started_rx.await.unwrap();

        let (_config_dir, ctx) = invalid_config_ctx(base_url);

        // Must not exit_err/panic despite the broken --config: the
        // daemon_url override is honored before any config load is
        // attempted (Fix F). A successful run just prints and returns —
        // safe to call the real async entry point directly.
        run_job_cancel_async(&ctx, &job.id).await;

        let after = state.job_queue().get_job(&job.id).await.unwrap();
        assert_ne!(
            after.state,
            IndexJobState::Pending,
            "cancellation must have reached the job despite the broken local config"
        );
    }

    #[tokio::test]
    async fn job_list_with_daemon_url_override_succeeds_even_with_an_invalid_local_config() {
        let (_daemon_dir, state, base_url) = spawn_real_daemon().await;
        state
            .job_queue()
            .submit("store-1", IndexJobScope::Store, |_progress| async {
                Ok(IndexJobStats::default())
            })
            .await
            .unwrap();

        let (_config_dir, ctx) = invalid_config_ctx(base_url);

        // Same proof as the cancel test above, for `job list`.
        run_job_list_async(&ctx).await;
    }
}
