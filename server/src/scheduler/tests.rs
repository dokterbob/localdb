use super::*;
use localdb_core::IndexJobState;
use std::time::Duration;

fn make_scheduler() -> UrlRefreshScheduler {
    let queue = JobQueue::new();
    UrlRefreshScheduler::new(queue)
}

/// Poll `scheduler.records` until `source_id`'s `last_refreshed` is
/// set, up to 5s. The stamp lands on
/// a separate spawned task (`wait_for_job_terminal` + the stamp itself)
/// woken by the job's own terminal transition, independently scheduled
/// from whatever poll a test itself uses to observe that same terminal
/// state — so a test that needs to see the stamp must poll for it in
/// its own right, not assume it's already visible the instant the
/// job's state is.
async fn wait_for_last_refreshed_stamp(scheduler: &UrlRefreshScheduler, source_id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        {
            let records = scheduler.records.read().await;
            if records
                .get(source_id)
                .is_some_and(|r| r.last_refreshed.is_some())
            {
                return;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("last_refreshed for '{source_id}' was never stamped in time");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn register_and_count() {
    let scheduler = make_scheduler();
    assert_eq!(scheduler.source_count().await, 0);

    scheduler
        .register(
            "src-1".to_string(),
            "store-A".to_string(),
            "https://example.com/feed".to_string(),
            Some(3600),
        )
        .await;

    assert_eq!(scheduler.source_count().await, 1);
}

#[tokio::test]
async fn unregister_removes_source() {
    let scheduler = make_scheduler();
    scheduler
        .register(
            "src-1".to_string(),
            "store-A".to_string(),
            "https://example.com/feed".to_string(),
            Some(3600),
        )
        .await;

    scheduler.unregister("src-1").await;
    assert_eq!(scheduler.source_count().await, 0);
}

#[tokio::test]
async fn tick_submits_job_for_due_sources() {
    // A source with interval=0 is always due.
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    scheduler
        .register(
            "src-refresh".to_string(),
            "my-store".to_string(),
            "https://example.com/docs".to_string(),
            Some(0), // 0-second interval → always due
        )
        .await;

    scheduler.tick().await;

    // Give the job queue worker time to pick up the job.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let jobs = queue.list_jobs().await;
    assert_eq!(
        jobs.len(),
        1,
        "tick() should have submitted one job for the due source"
    );
    let job = &jobs[0];
    assert_eq!(job.store_id, "my-store");
    assert!(
        matches!(
            &job.scope,
            localdb_core::IndexJobScope::Source { source_id }
                if source_id == "src-refresh"
        ),
        "job scope should reference the source: {:?}",
        job.scope
    );
}

#[tokio::test]
async fn tick_does_not_submit_job_for_sources_without_interval() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    // No interval → never auto-refreshed.
    scheduler
        .register(
            "src-manual".to_string(),
            "my-store".to_string(),
            "https://example.com/page".to_string(),
            None,
        )
        .await;

    scheduler.tick().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let jobs = queue.list_jobs().await;
    assert!(
        jobs.is_empty(),
        "tick() should not submit jobs for sources with no interval"
    );
}

#[tokio::test]
async fn tick_twice_only_submits_once_when_not_due_yet() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    // Interval = 1 hour → only due on the first tick (never refreshed).
    scheduler
        .register(
            "src-hourly".to_string(),
            "my-store".to_string(),
            "https://example.com/data".to_string(),
            Some(3600),
        )
        .await;

    // First tick: source was never refreshed → is due → submits job.
    scheduler.tick().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let after_first_tick = queue.list_jobs().await.len();
    assert_eq!(after_first_tick, 1, "first tick should submit one job");

    // Second tick immediately after: `last_refreshed` is ~now, interval not reached.
    scheduler.tick().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let after_second_tick = queue.list_jobs().await.len();
    assert_eq!(
        after_second_tick, 1,
        "second tick should not re-submit (interval not elapsed)"
    );
}

/// Without `attach_state`, `tick()` still submits and the job still
/// reaches a terminal state — but honestly: `Failed`, with a clear
/// error, never a fabricated `Done` with zero stats (issue #187 §1).
#[tokio::test]
async fn submitted_job_without_attached_state_fails_honestly() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    scheduler
        .register(
            "src-complete".to_string(),
            "store-Z".to_string(),
            "https://example.com/".to_string(),
            Some(0),
        )
        .await;

    scheduler.tick().await;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("refresh job did not reach a terminal state in time");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let jobs = queue.list_jobs().await;
        if let Some(job) = jobs.first() {
            if job.state == IndexJobState::Failed {
                assert!(
                    job.error
                        .as_deref()
                        .is_some_and(|e| e.contains("no state attached")),
                    "expected a 'no state attached' error, got: {:?}",
                    job.error
                );
                break;
            }
            assert_ne!(
                job.state,
                IndexJobState::Done,
                "a job with no attached state must never report Done"
            );
        }
    }
}

/// #187 review F1: `tick()` submits the job, but must not stamp
/// `last_refreshed` until the job actually reaches a terminal state.
/// Uses the same deterministic fast-failure path as
/// `submitted_job_without_attached_state_fails_honestly` above — no
/// state attached, so the job fails immediately without needing a real
/// store/embedder — rather than a real, slow ingestion, so the
/// "submitted but not yet complete" window is reliably observable on a
/// single-threaded test runtime (the worker task doesn't get to run
/// until this test task itself yields, e.g. via `sleep`).
///
/// The stamp itself happens on a
/// separate spawned task that wakes up once the job's progress channel
/// closes (see `wait_for_job_terminal`) — deliberately *not* inline
/// with the job's own future — so "the job is Failed" and "the stamp
/// has landed" are two independently-scheduled events; the final
/// assertion below polls for the stamp with its own bounded deadline
/// rather than asserting it's already visible the instant this test's
/// own poll first observes `Failed`.
#[tokio::test]
async fn last_refreshed_is_recorded_on_completion_not_on_submission() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    scheduler
        .register(
            "src-timing".to_string(),
            "store-T".to_string(),
            "https://example.com/".to_string(),
            Some(0),
        )
        .await;

    scheduler.tick().await;

    // Immediately after `tick()` returns: the job has been submitted
    // (it exists in the queue) but the worker — a separate task on this
    // current-thread runtime — has not yet had a chance to run it.
    assert_eq!(
        queue.list_jobs().await.len(),
        1,
        "tick() should have submitted the job"
    );
    {
        let records = scheduler.records.read().await;
        assert_eq!(
            records.get("src-timing").unwrap().last_refreshed,
            None,
            "last_refreshed must not be set merely because the job was submitted"
        );
    }

    // Drive the job to its terminal (failed, no state attached) state.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("refresh job did not reach a terminal state in time");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let jobs = queue.list_jobs().await;
        if let Some(job) = jobs.first() {
            if job.state == IndexJobState::Failed {
                break;
            }
        }
    }

    // The stamp lands asynchronously (a separate task, woken by the
    // same terminal transition) — poll for it with its own deadline.
    wait_for_last_refreshed_stamp(&scheduler, "src-timing").await;
}

/// A source whose refresh job has been
/// submitted but whose completion watcher has not yet stamped
/// `last_refreshed` is never due — even when its timestamp is stale
/// and the job queue would accept a submission. The queue's per-store
/// in-flight guard is released by `process_job` *before* the watcher
/// task gets scheduled, so the guard alone cannot suppress a resubmit
/// in that window; `refresh_inflight` must. Stages the window directly
/// (flag set, queue empty, timestamp stale) — the real interleaving
/// depends on task scheduling order and can't be forced
/// deterministically, but the flag's set/clear wiring is covered by
/// the stamp-lifecycle tests around this one.
#[tokio::test]
async fn tick_skips_sources_with_an_unstamped_inflight_refresh() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());
    scheduler
        .register(
            "src-window".to_string(),
            "store-W".to_string(),
            "https://example.com/".to_string(),
            Some(0),
        )
        .await;

    // The exact race window: a previous refresh was submitted
    // (`refresh_inflight` set by `tick`), its job has fully left the
    // queue (guard released — the queue is empty), and the watcher has
    // not yet stamped (`last_refreshed` still `None`, maximally stale).
    {
        let mut records = scheduler.records.write().await;
        records.get_mut("src-window").unwrap().refresh_inflight = true;
    }

    scheduler.tick().await;

    assert_eq!(
        queue.list_jobs().await.len(),
        0,
        "a tick in the unstamped-inflight window must not submit a refresh"
    );

    // Watcher completion: stamp + clear in one write, exactly as the
    // real watcher does. With interval 0 the source is immediately due
    // again by timestamp — the next tick may submit normally.
    {
        let mut records = scheduler.records.write().await;
        let r = records.get_mut("src-window").unwrap();
        r.last_refreshed = Some(Instant::now());
        r.refresh_inflight = false;
    }
    scheduler.tick().await;
    assert_eq!(
        queue.list_jobs().await.len(),
        1,
        "once stamped and cleared, a due source submits normally again"
    );
}

/// #187 review F1: a failed job must still be stamped with
/// `last_refreshed`, so it waits out a full interval before being
/// retried rather than tight-looping.
#[tokio::test]
async fn failed_job_is_not_resubmitted_before_a_full_interval_elapses() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    scheduler
        .register(
            "src-retry".to_string(),
            "store-R".to_string(),
            "https://example.com/".to_string(),
            Some(3600), // 1 hour — long enough to never elapse in-test.
        )
        .await;

    scheduler.tick().await;

    // Drive the job to Failed (no state attached).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("refresh job did not fail in time");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let jobs = queue.list_jobs().await;
        if let Some(job) = jobs.first() {
            if job.state == IndexJobState::Failed {
                break;
            }
        }
    }

    // The stamp itself lands on a separate task (Fix C) — wait for it
    // before ticking again, or the second tick could race a
    // not-yet-stamped record and wrongly consider it still due.
    wait_for_last_refreshed_stamp(&scheduler, "src-retry").await;

    // A second tick immediately after the failure must not resubmit:
    // the interval (1 hour) has not elapsed since `last_refreshed` was
    // stamped at completion.
    scheduler.tick().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let jobs = queue.list_jobs().await;
    assert_eq!(
        jobs.len(),
        1,
        "a failed job must not be resubmitted before a full interval has elapsed"
    );
}

/// A cancelled refresh job must not be
/// resubmitted on the tick immediately following its cancellation —
/// were `last_refreshed` stamped as the tail expression of the job's
/// own submitted closure, `job_queue::process_job` would drop that
/// stamp (along with everything else still pending inside the future)
/// when it `handle.abort()`s a cancelled task, and the never-stamped
/// refresh would be resubmitted on the very next tick — undoing the
/// backoff the cancellation is supposed to buy; hence the detached
/// watcher task (see `tick`). Constructed deterministically: a
/// blocker job on a *different* store occupies the queue's sole worker
/// (mirrors `job_queue::tests::cancellation`'s pending-cancel test)
/// so the scheduler's own tick-submitted refresh job is guaranteed to
/// still be `Pending` when this test cancels it.
#[tokio::test]
async fn cancelled_refresh_job_is_not_resubmitted_on_the_immediately_following_tick() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    scheduler
        .register(
            "src-cancel".to_string(),
            "store-C".to_string(),
            "https://example.com/".to_string(),
            Some(3600), // 1 hour — long enough that only the cancel-stamp, not a real elapsed interval, could suppress resubmission.
        )
        .await;

    // Occupy the queue's sole worker with an unrelated job parked on a
    // gate this test controls, so the scheduler's own submission
    // (below) is guaranteed to still be Pending when this test cancels
    // it.
    let (blocker_release_tx, blocker_release_rx) = tokio::sync::oneshot::channel::<()>();
    queue
        .submit(
            "blocker-store",
            IndexJobScope::Store,
            move |_progress| async move {
                let _ = blocker_release_rx.await;
                Ok(localdb_core::IndexJobStats::default())
            },
        )
        .await
        .unwrap();

    scheduler.tick().await;

    let jobs = queue.list_jobs().await;
    let refresh_job = jobs
        .iter()
        .find(|j| j.store_id == "store-C")
        .expect("tick() should have submitted the refresh job")
        .clone();
    assert_eq!(
        refresh_job.state,
        IndexJobState::Pending,
        "the refresh job must still be queued behind the blocker job"
    );

    queue.cancel(&refresh_job.id).await.unwrap();

    // Release the blocker so the worker can move on to (not-)running
    // the refresh job.
    let _ = blocker_release_tx.send(());

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("cancelled refresh job did not reach a terminal state in time");
        }
        let job = queue.get_job(&refresh_job.id).await.unwrap();
        if job.state == IndexJobState::Failed {
            assert_eq!(job.error_code.as_deref(), Some("job_cancelled"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The stamp lands on a separate task (Fix C) — wait for it before
    // ticking again, or the next tick could race a not-yet-stamped
    // record and wrongly consider it still due.
    wait_for_last_refreshed_stamp(&scheduler, "src-cancel").await;

    // A tick immediately after the cancellation must not resubmit: the
    // interval (1 hour) has not elapsed since `last_refreshed` was
    // stamped at cancellation — exactly the same backoff an ordinary
    // failure gets
    // (`failed_job_is_not_resubmitted_before_a_full_interval_elapses`
    // above), now also honored for cancellation.
    scheduler.tick().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let jobs = queue.list_jobs().await;
    let refresh_jobs_for_source = jobs.iter().filter(|j| j.store_id == "store-C").count();
    assert_eq!(
        refresh_jobs_for_source, 1,
        "a cancelled refresh job must not be resubmitted before a full interval has elapsed"
    );
}

/// The real regression test for #187 §1 on the scheduler path: with a
/// real `AppState` attached (real store, real path source with content,
/// fake embedder), `tick()` on a due source must produce genuine,
/// nonzero stats — not the old stub's `IndexJobStats::default()`.
#[tokio::test]
async fn tick_with_attached_state_runs_real_ingestion_and_produces_nonzero_stats() {
    let content_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        content_dir.path().join("doc.md"),
        "rust programming language performance tips",
    )
    .unwrap();

    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    let mut yaml_config = localdb_core::config::schema::RawConfig::default();
    yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    let state_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        yaml_config,
        state_dir.path().to_path_buf(),
        state_dir.path().join("models"),
        queue.clone(),
        scheduler.clone(),
        crate::auth::AuthMode::Open,
    )
    .await
    .unwrap();

    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "path",
            serde_json::json!({"root": content_dir.path().to_string_lossy()}),
            "prose",
            None,
        )
        .await
        .unwrap();

    scheduler.attach_state(state.clone()).await;
    // The scheduler's own bookkeeping doesn't care about source *kind* —
    // real ingestion re-reads the persisted `SourceRow` (a `path`
    // source here) via `job_exec::run_job`, not this record's `url`.
    scheduler
        .register(
            source.id.clone(),
            "notes".to_string(),
            "https://example.com".to_string(),
            Some(0),
        )
        .await;

    scheduler.tick().await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("refresh job did not complete within timeout");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let jobs = queue.list_jobs().await;
        if let Some(job) = jobs.first() {
            if job.state == IndexJobState::Failed {
                panic!("refresh job failed: {:?}", job.error);
            }
            if job.state == IndexJobState::Done {
                assert!(
                    job.stats.docs_indexed > 0,
                    "expected nonzero docs_indexed, got {:?}",
                    job.stats
                );
                assert!(
                    job.stats.chunks_written > 0,
                    "expected nonzero chunks_written, got {:?}",
                    job.stats
                );
                // #187 review F1: `last_refreshed` must be recorded
                // once the job actually completes, not back when it was
                // merely submitted. The stamp itself lands on a
                // separate task, so poll
                // for it rather than asserting it's already visible the
                // instant this loop observes `Done`.
                wait_for_last_refreshed_stamp(&scheduler, &source.id).await;
                break;
            }
        }
    }
}

/// Codex review finding G1 (issue #187): the scheduler's refresh closure
/// used to call `state.get_or_build_embedder` unconditionally before
/// `run_job` ran. For `IndexJobScope::Source`, an unresolvable source
/// (e.g. deleted since it was registered for refresh) makes
/// `resolve_job_sources` return `Err(SourceNotFound)` rather than an
/// empty list — but under the old ordering that error surfaced only
/// *after* an embedder had already been built and thrown away. Registers
/// a refresh record for a source id that was never actually added to the
/// store, ticks, and asserts the job fails with that source unresolved
/// while the daemon's embedder cache is never built.
#[tokio::test]
async fn tick_with_unresolvable_source_never_builds_embedder() {
    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue.clone());

    let mut yaml_config = localdb_core::config::schema::RawConfig::default();
    yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
        provider: "fake".to_string(),
        model: "default".to_string(),
    };
    let state_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        yaml_config,
        state_dir.path().to_path_buf(),
        state_dir.path().join("models"),
        queue.clone(),
        scheduler.clone(),
        crate::auth::AuthMode::Open,
    )
    .await
    .unwrap();

    state.add_store("notes", "private").await.unwrap();
    scheduler.attach_state(state.clone()).await;
    // No source was ever added to "notes" — this id resolves to nothing.
    scheduler
        .register(
            "missing-source".to_string(),
            "notes".to_string(),
            "https://example.com".to_string(),
            Some(0),
        )
        .await;

    scheduler.tick().await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("refresh job did not reach a terminal state in time");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        let jobs = queue.list_jobs().await;
        if let Some(job) = jobs.first() {
            if job.state == IndexJobState::Done {
                panic!("job with an unresolvable source must not report Done: {job:?}");
            }
            if job.state == IndexJobState::Failed {
                assert!(
                    job.error
                        .as_deref()
                        .is_some_and(|e| e.contains("missing-source")),
                    "expected a source-not-found error naming the missing source, got: {:?}",
                    job.error
                );
                break;
            }
        }
    }

    assert_eq!(
        state.embedder_build_count(),
        0,
        "an unresolvable source scope must never trigger an embedder build"
    );
}
