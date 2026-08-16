//! `localdb job cancel` — request cancellation of a queued or running job on
//! a daemon's job queue (issue #218).
//!
//! Daemon-only, unlike every other dual-transport command in this crate:
//! there is no meaningful embedded equivalent. The CLI's embedded indexing
//! path (`cli::job_attach::run_embedded_store_job`) spins up a throwaway
//! `JobQueue` that lives and dies inside a single `localdb index`
//! invocation — there is no separate, longer-lived process a `job cancel`
//! command could ever reach to interrupt it. Cancellation only makes sense
//! against the daemon's persistent queue, so this requires a running daemon
//! and exits 5 (`daemon_unreachable`) without one, the same outcome every
//! other daemon-only path in this crate gives.

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

    let config_loader = load_config_scaffolded(ctx).await;
    let daemon_state = probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref());
    let base_url = match daemon_state {
        DaemonState::Running { base_url } => base_url,
        DaemonState::NotRunning => exit_err(&Error::DaemonUnreachable, ctx.json),
    };

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
}
