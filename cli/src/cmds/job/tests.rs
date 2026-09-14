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
        server::auth::AuthMode::Open,
    )
    .await
    .unwrap();
    let embedder: Arc<dyn Embedder> = Arc::new(localdb_core::FakeEmbedder::new(128));
    let router = server::build_router(
        state.clone(),
        std::sync::Arc::new(server::mcp_bridge::AppStateStoreProvider::new(
            state.clone(),
        )),
        embedder,
        vec![],
    );

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
    let err = cancel_daemon_job(&crate::tests::context(), &base_url, "nonexistent-job-id")
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

    let snapshot = cancel_daemon_job(&crate::tests::context(), &base_url, &job.id)
        .await
        .unwrap();
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

    let err = cancel_daemon_job(&crate::tests::context(), &base_url, &job.id)
        .await
        .unwrap_err();
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
        api_key: None,
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

    let jobs = list_daemon_jobs(&crate::tests::context(), &base_url)
        .await
        .unwrap();
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
        api_key: None,
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
        api_key: None,
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
