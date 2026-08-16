//! `resolve_post_trigger_outcome` (issue #218 review, fix 3 then fix 5): the
//! pure decision function `JobQueue::cancel` uses to interpret the job's
//! registry state re-read after triggering its cancellation token.
//!
//! The bug this guards against: the token trigger and this re-read are two
//! separate lock acquisitions, so on a multi-thread runtime `run_worker` can
//! race this very call and finish recording `Failed`/`job_cancelled` before
//! the re-read ever runs. Treating *every* terminal state found there as a
//! conflict (as the naive fix-3 pass did) hands the very caller whose
//! cancellation just worked a confusing `409`. These tests pin the mapping
//! directly and deterministically — no timing involved, since the function
//! takes an already-constructed `IndexJob` snapshot rather than racing
//! anything itself.

use localdb_core::{IndexJob, IndexJobScope, IndexJobState, IndexJobStats};

use crate::job_queue::resolve_post_trigger_outcome;

fn job(state: IndexJobState, error_code: Option<&str>) -> IndexJob {
    IndexJob {
        id: "job-1".to_string(),
        store_id: "store-1".to_string(),
        scope: IndexJobScope::Store,
        state,
        stats: IndexJobStats::default(),
        error: None,
        error_code: error_code.map(str::to_string),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        started_at: None,
        completed_at: None,
    }
}

/// The case this fix exists for: the job is `Failed` with
/// `error_code: "job_cancelled"` — the outcome the caller asked for was
/// actually achieved, whether by this call or a racing one, so it must be
/// `Ok`, not `Err(JobAlreadyTerminal)`.
#[test]
fn treats_a_job_cancelled_terminal_state_as_success() {
    let j = job(IndexJobState::Failed, Some("job_cancelled"));
    let result = resolve_post_trigger_outcome(&j).unwrap();
    assert_eq!(result.state, IndexJobState::Failed);
    assert_eq!(result.error_code.as_deref(), Some("job_cancelled"));
}

/// A job that reached `Done` first (unrelated to this cancel) is a genuine
/// conflict — the cancellation did not, and could not, cause this outcome.
#[test]
fn rejects_done_as_a_conflict() {
    let j = job(IndexJobState::Done, None);
    let err = resolve_post_trigger_outcome(&j).unwrap_err();
    assert!(matches!(err, localdb_core::Error::JobAlreadyTerminal));
}

/// A job that failed for a *different* reason (a real typed error) must
/// not be swallowed as if it were this cancellation's own doing.
#[test]
fn rejects_failed_with_a_different_error_code_as_a_conflict() {
    let j = job(IndexJobState::Failed, Some("invalid_config"));
    let err = resolve_post_trigger_outcome(&j).unwrap_err();
    assert!(matches!(err, localdb_core::Error::JobAlreadyTerminal));
}

/// A synthetic queue-level failure (queue full/closed, task panic) carries
/// no `error_code` at all (see `IndexJob::error_code`'s doc comment) — this
/// must not be mistaken for `job_cancelled` and must still be a conflict.
#[test]
fn rejects_failed_with_no_error_code_as_a_conflict() {
    let j = job(IndexJobState::Failed, None);
    let err = resolve_post_trigger_outcome(&j).unwrap_err();
    assert!(matches!(err, localdb_core::Error::JobAlreadyTerminal));
}

/// The ordinary case: a job still `Pending` or `Running` after the trigger
/// is the expected "cancellation requested" outcome.
#[test]
fn allows_pending_and_running_as_the_ordinary_requested_outcome() {
    for state in [IndexJobState::Pending, IndexJobState::Running] {
        let j = job(state.clone(), None);
        let result = resolve_post_trigger_outcome(&j).unwrap();
        assert_eq!(result.state, state);
    }
}
