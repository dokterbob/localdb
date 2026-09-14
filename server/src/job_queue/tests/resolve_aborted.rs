//! `resolve_aborted` (issue #218 review, fix 1): the pure decision function
//! `run_worker`'s cancellation branch uses to interpret a `JoinHandle`
//! re-awaited after `handle.abort()`.
//!
//! The bug this guards against: `abort()` is a no-op on a task that had
//! *already* finished before the cancellation branch of the `select!` won
//! the race — the re-awaited handle then resolves to the task's real
//! `Ok(Ok(stats))`/`Ok(Err(e))`, not a cancellation error. Unconditionally
//! reporting `Cancelled` in that case silently discarded a
//! genuinely-completed (and, per Wave 1, durably committed) run. These
//! tests exercise `resolve_aborted` directly with real `Result<..,
//! JoinError>` values — two constructed inline (the "real result wins"
//! cases, which need no async machinery at all) and two obtained from a
//! real `tokio::spawn` + `abort()`/panic (since `JoinError` has no public
//! constructor) — rather than trying to force the actual racy `select!`
//! branch from outside, which issue #181's determinism rule rules out.

use localdb_core::{Error, IndexJobStats};

use crate::job_queue::{resolve_aborted, JobOutcome};

#[test]
fn resolve_aborted_prefers_a_real_ok_result_over_the_cancellation_flag() {
    let stats = IndexJobStats {
        docs_indexed: 5,
        ..Default::default()
    };
    let joined = Ok(Ok(stats.clone()));

    match resolve_aborted(joined) {
        JobOutcome::Finished(Ok(Ok(s))) => assert_eq!(s, stats),
        other => panic!("expected Finished(Ok(Ok(stats))), got a Cancelled/panic/error outcome instead: {other:?}"),
    }
}

#[test]
fn resolve_aborted_prefers_a_real_err_result_over_the_cancellation_flag() {
    let err = Error::InvalidConfig {
        message: "unconfigured embedder provider".to_string(),
    };
    let joined = Ok(Err(err.clone()));

    match resolve_aborted(joined) {
        JobOutcome::Finished(Ok(Err(e))) => assert_eq!(e, err),
        other => panic!("expected Finished(Ok(Err(err))), got: {other:?}"),
    }
}

/// `JoinError` has no public constructor, so this obtains a *real* one the
/// only way possible: spawn a task that never yields control back on its
/// own (`std::future::pending`, matching `cancellation.rs`'s
/// `cancellation_preempts_a_never_resolving_await_point`), abort it, and
/// await the handle again — exactly the sequence `run_worker`'s
/// cancellation branch performs.
#[tokio::test]
async fn resolve_aborted_reports_cancelled_for_a_genuinely_aborted_task() {
    let handle = tokio::spawn(async {
        std::future::pending::<()>().await;
        #[allow(unreachable_code)]
        Ok(IndexJobStats::default())
    });
    // Yield once so the task is actually polled (and parked on `pending()`)
    // before aborting it — otherwise `abort()` could race a task that was
    // merely scheduled, not yet running.
    tokio::task::yield_now().await;
    handle.abort();
    let joined = handle.await;

    assert!(
        joined.as_ref().is_err_and(|e| e.is_cancelled()),
        "expected a real cancellation JoinError, got: {joined:?}"
    );
    assert!(
        matches!(resolve_aborted(joined), JobOutcome::Cancelled),
        "a genuinely aborted task must resolve to Cancelled"
    );
}

/// The mirror case: a task that panics (not aborted) must never be
/// misreported as `Cancelled` — `JoinError::is_cancelled()` is `false` for
/// a panic, and `resolve_aborted` must preserve it as `Finished(Err(_))` so
/// `run_worker`'s existing panic-handling arm still runs for it.
#[tokio::test]
async fn resolve_aborted_preserves_a_genuine_panic_not_as_cancelled() {
    let handle = tokio::spawn(async {
        panic!("simulated task panic");
        #[allow(unreachable_code)]
        Ok(IndexJobStats::default())
    });
    let joined = handle.await;

    assert!(
        joined
            .as_ref()
            .is_err_and(|e| e.is_panic() && !e.is_cancelled()),
        "expected a real panic JoinError, got: {joined:?}"
    );
    assert!(
        matches!(resolve_aborted(joined), JobOutcome::Finished(Err(_))),
        "a genuine panic must never be reported as Cancelled"
    );
}
