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

use crate::tests::context as test_ctx;

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
        api_key: None,
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
        &mut embedder,
        RunEmbeddedStoreJobArgs {
            scope: unknown_scope.clone(),
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::StrictExit,
            progress_label: None,
        },
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
        &mut embedder,
        RunEmbeddedStoreJobArgs {
            scope: unknown_scope,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::WarnAndContinue,
            progress_label: None,
        },
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
            feed_inputs_digest: None,
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
        &mut embedder,
        RunEmbeddedStoreJobArgs {
            scope: IndexJobScope::Store,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::WarnAndContinue,
            progress_label: None,
        },
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
async fn run_daemon_store_job_reports_submission_failure_strict_and_warn() {
    let (_dir, _state, base_url) = spawn_real_daemon().await;
    let ctx = test_ctx();

    let err = run_daemon_store_job(
        &ctx,
        &base_url,
        "nonexistent-store",
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::StrictExit,
            progress_label: None,
        },
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
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::WarnAndContinue,
            progress_label: None,
        },
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

    let final_job = attach_daemon_job(&test_ctx(), &base_url, &job.id, false, None)
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
    let attach_task = tokio::spawn(async move {
        try_attach_via_sse(&test_ctx(), &base_url, &job_id, Some(&sink)).await
    });

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
    status: VecDeque<(u16, String)>,
    /// Call counters, kept outside the outer `tokio::sync::Mutex` (via
    /// their own `Arc`) so a test can clone them before moving
    /// `MockResponses` into `spawn_mock_daemon` and still read them
    /// afterward — proving a route was (or was never) hit, not just
    /// inferring it from a response shape.
    jobs_post_calls: Arc<StdMutex<u32>>,
    status_calls: Arc<StdMutex<u32>>,
}

type SharedMockResponses = Arc<tokio::sync::Mutex<MockResponses>>;

async fn mock_jobs_post(
    State(state): State<SharedMockResponses>,
    _body: axum::body::Bytes,
) -> (StatusCode, String) {
    let mut guard = state.lock().await;
    *guard.jobs_post_calls.lock().unwrap() += 1;
    let (code, body) = guard
        .jobs_post
        .pop_front()
        .unwrap_or((500, "{}".to_string()));
    (StatusCode::from_u16(code).unwrap(), body)
}

async fn mock_status(State(state): State<SharedMockResponses>) -> (StatusCode, String) {
    let mut guard = state.lock().await;
    *guard.status_calls.lock().unwrap() += 1;
    let (code, body) = guard
        .status
        .pop_front()
        .unwrap_or((200, serde_json::json!({ "features": [] }).to_string()));
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
        .route("/v1/status", get(mock_status))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

/// Mirrors `cli::cmds::search`'s `require_daemon_search_filter_support`
/// precedent: a daemon whose `/v1/status` reports no `refetch` feature
/// (an empty `features` array — the shape an older daemon predating this
/// field would never even send, but also the shape a daemon that has the
/// field yet lacks the capability would send) must fail the whole call
/// with `DaemonCapabilityUnavailable` (exit 5) and never reach
/// `POST /v1/jobs` at all — submitting anyway would silently run an
/// ordinary gated job while reporting success, exactly the failure this
/// check exists to prevent.
#[tokio::test]
async fn run_daemon_store_job_refetch_without_the_feature_exits_5_and_never_posts_a_job() {
    let ctx = test_ctx();
    let jobs_post_calls = Arc::new(StdMutex::new(0));
    let mut responses = MockResponses {
        jobs_post_calls: jobs_post_calls.clone(),
        ..Default::default()
    };
    responses
        .status
        .push_back((200, serde_json::json!({ "features": [] }).to_string()));
    let base_url = spawn_mock_daemon(responses).await;

    let err = run_daemon_store_job(
        &ctx,
        &base_url,
        "store-x",
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: true, // --refetch
            mode: IndexErrorMode::StrictExit,
            progress_label: None,
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, Error::DaemonCapabilityUnavailable { .. }),
        "expected DaemonCapabilityUnavailable, got: {err:?}"
    );
    assert_eq!(err.exit_code(), 5);
    assert_eq!(
        *jobs_post_calls.lock().unwrap(),
        0,
        "must never POST /v1/jobs once the capability check has failed"
    );
}

/// The converse of the test above: an ordinary run (no `--refetch`) must
/// never pay for the capability probe at all — mirroring
/// `require_daemon_search_filter_support`'s own "only when a filter is
/// actually set" gating for search. Proven by asserting `/v1/status` was
/// never hit while driving a real submit-attach-complete round trip
/// through the mock daemon.
#[tokio::test]
async fn run_daemon_store_job_without_refetch_never_probes_the_capability_endpoint() {
    let ctx = test_ctx();
    let status_calls = Arc::new(StdMutex::new(0));
    let done_job = sample_job("job-9", IndexJobState::Done);
    let mut responses = MockResponses {
        status_calls: status_calls.clone(),
        ..Default::default()
    };
    responses
        .jobs_post
        .push_back((202, serde_json::json!({ "id": "job-9" }).to_string()));
    responses.events.push_back((
        200,
        format!(
            "event: job\ndata: {}\n\n",
            serde_json::to_string(&done_job).unwrap()
        ),
    ));
    let base_url = spawn_mock_daemon(responses).await;

    let (_summary, job_id) = run_daemon_store_job(
        &ctx,
        &base_url,
        "store-x",
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: false, // no --refetch
            mode: IndexErrorMode::StrictExit,
            progress_label: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(job_id.as_deref(), Some("job-9"));
    assert_eq!(
        *status_calls.lock().unwrap(),
        0,
        "must never probe /v1/status when --refetch is not set"
    );
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
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::StrictExit,
            progress_label: None,
        },
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
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::WarnAndContinue,
            progress_label: None,
        },
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

    let job = attach_daemon_job(&test_ctx(), &base_url, "job-1", false, None)
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

    let job = attach_daemon_job(&test_ctx(), &base_url, "job-7", false, None)
        .await
        .unwrap();
    assert_eq!(job.state, IndexJobState::Done);
}

#[tokio::test]
async fn attach_daemon_job_falls_back_to_polling_when_the_sse_stream_ends_without_a_terminal_frame()
{
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

    let job = attach_daemon_job(&test_ctx(), &base_url, "job-2", false, None)
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

    let err = attach_daemon_job(&test_ctx(), &base_url, "job-3", false, None)
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
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::StrictExit,
            progress_label: None,
        },
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
        RunDaemonStoreJobArgs {
            source_id: None,
            deletion: DeletionPolicy::Retain,
            refetch: false,
            mode: IndexErrorMode::WarnAndContinue,
            progress_label: None,
        },
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

    let job = match try_attach_via_sse(&test_ctx(), &base_url, "job-6", Some(&sink)).await {
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

mod auth;
