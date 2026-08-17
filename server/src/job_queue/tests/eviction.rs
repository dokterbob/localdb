//! Bounded terminal-job retention (issue #218-followups Fix 2, PR #229
//! review): the registry evicts the oldest `Done`/`Failed` jobs once their
//! count exceeds a cap, so a long-running daemon's job history doesn't grow
//! without bound.
//!
//! The pure eviction function (`evict_oldest_terminal_jobs_over_cap`) is
//! tested directly against hand-built registries below, with a small,
//! test-chosen `cap` — not the real `MAX_TERMINAL_JOBS` (200), since
//! `completed_at` is a fixed constant under `#[cfg(test)]`
//! (`localdb_core::ingestion::now_rfc3339`), so this crate's tests can't
//! rely on real wall-clock ordering to prove "oldest first" the way
//! production does; hand-built fixtures with distinct `completed_at`
//! strings sidestep that entirely. The one real end-to-end test below
//! (through `JobQueue::submit` at the real `MAX_TERMINAL_JOBS` cap) proves
//! the wiring and `get_job`'s post-eviction behavior with an exact evicted
//! *count* — see its own doc comment for why it still doesn't pin which
//! specific ids, even now that ties break deterministically by id (PR #229
//! round-3 review).
//!
//! `PROTECT_NONE` below: the pure tests that aren't about the
//! self-protection rule pass an id that matches nothing in their fixture,
//! so protection is inert and the test exercises only the ordering/cap
//! logic. The self-protection rule has its own dedicated test.

use std::collections::HashMap;

use localdb_core::{IndexJob, IndexJobScope, IndexJobState, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::{evict_oldest_terminal_jobs_over_cap, JobQueue, MAX_TERMINAL_JOBS};

/// Build a sample terminal or non-terminal `IndexJob` for the pure eviction
/// tests below — `completed_at` is the field eviction sorts by, always
/// explicit here rather than derived from `localdb_core::ingestion::now_rfc3339`
/// (stubbed to a fixed constant under `#[cfg(test)]`, so it can't express
/// real ordering).
fn sample_job(id: &str, state: IndexJobState, completed_at: Option<&str>) -> IndexJob {
    IndexJob {
        id: id.to_string(),
        store_id: "store-x".to_string(),
        scope: IndexJobScope::Store,
        state,
        stats: IndexJobStats::default(),
        error: None,
        error_code: None,
        created_at: "2020-01-01T00:00:00Z".to_string(),
        started_at: None,
        completed_at: completed_at.map(str::to_string),
    }
}

fn terminal_job(id: &str, completed_at: &str) -> IndexJob {
    sample_job(id, IndexJobState::Done, Some(completed_at))
}

/// A `protect_id` that matches no fixture job — for tests where the
/// self-protection rule is not the subject (see module doc).
const PROTECT_NONE: &str = "no-such-job";

// ---------------------------------------------------------------------------
// evict_oldest_terminal_jobs_over_cap — pure function, hand-built fixtures
// ---------------------------------------------------------------------------

#[test]
fn is_a_no_op_when_terminal_count_is_at_or_under_cap() {
    let mut registry: HashMap<String, IndexJob> = HashMap::new();
    for i in 0..3 {
        let id = format!("job-{i}");
        registry.insert(
            id.clone(),
            terminal_job(&id, &format!("2020-01-0{}T00:00:00Z", i + 1)),
        );
    }

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, PROTECT_NONE);
    assert_eq!(registry.len(), 3, "at the cap exactly: nothing evicted");

    evict_oldest_terminal_jobs_over_cap(&mut registry, 5, PROTECT_NONE);
    assert_eq!(registry.len(), 3, "under the cap: nothing evicted");
}

#[test]
fn respects_the_cap() {
    let mut registry: HashMap<String, IndexJob> = HashMap::new();
    for i in 0..10 {
        let id = format!("job-{i:02}");
        registry.insert(
            id.clone(),
            terminal_job(&id, &format!("2020-01-{:02}T00:00:00Z", i + 1)),
        );
    }
    assert_eq!(registry.len(), 10);

    evict_oldest_terminal_jobs_over_cap(&mut registry, 4, PROTECT_NONE);

    assert_eq!(
        registry.len(),
        4,
        "terminal count must be trimmed down to exactly the cap"
    );
}

#[test]
fn removes_oldest_first_by_completed_at() {
    let mut registry: HashMap<String, IndexJob> = HashMap::new();
    // Inserted out of chronological order on purpose — eviction must sort
    // by `completed_at`, not by insertion/iteration order.
    registry.insert(
        "newest".to_string(),
        terminal_job("newest", "2020-01-05T00:00:00Z"),
    );
    registry.insert(
        "oldest".to_string(),
        terminal_job("oldest", "2020-01-01T00:00:00Z"),
    );
    registry.insert(
        "middle-2".to_string(),
        terminal_job("middle-2", "2020-01-03T00:00:00Z"),
    );
    registry.insert(
        "middle-1".to_string(),
        terminal_job("middle-1", "2020-01-02T00:00:00Z"),
    );
    registry.insert(
        "second-newest".to_string(),
        terminal_job("second-newest", "2020-01-04T00:00:00Z"),
    );

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, PROTECT_NONE);

    assert_eq!(registry.len(), 3);
    assert!(
        !registry.contains_key("oldest"),
        "the single oldest entry must be evicted first"
    );
    assert!(
        !registry.contains_key("middle-1"),
        "the second-oldest entry must be evicted next"
    );
    assert!(registry.contains_key("middle-2"));
    assert!(registry.contains_key("second-newest"));
    assert!(registry.contains_key("newest"));
}

#[test]
fn never_evicts_pending_or_running_even_when_they_push_the_total_past_the_cap() {
    let mut registry: HashMap<String, IndexJob> = HashMap::new();
    for i in 0..5 {
        let id = format!("terminal-{i}");
        registry.insert(
            id.clone(),
            terminal_job(&id, &format!("2020-01-0{}T00:00:00Z", i + 1)),
        );
    }
    registry.insert(
        "pending-job".to_string(),
        sample_job("pending-job", IndexJobState::Pending, None),
    );
    registry.insert(
        "running-job".to_string(),
        sample_job("running-job", IndexJobState::Running, None),
    );
    assert_eq!(registry.len(), 7);

    // Cap of 2: only the terminal subset (5) is measured against it, so 3
    // of the 5 terminal jobs are evicted — the two non-terminal jobs are
    // never candidates at all, regardless of how far over the cap the
    // *terminal* count is.
    evict_oldest_terminal_jobs_over_cap(&mut registry, 2, PROTECT_NONE);

    assert!(
        registry.contains_key("pending-job"),
        "a Pending job must never be evicted"
    );
    assert!(
        registry.contains_key("running-job"),
        "a Running job must never be evicted"
    );
    let terminal_remaining = registry
        .values()
        .filter(|j| matches!(j.state, IndexJobState::Done))
        .count();
    assert_eq!(
        terminal_remaining, 2,
        "terminal jobs must still be trimmed down to the cap"
    );
    assert_eq!(
        registry.len(),
        4,
        "2 non-terminal + 2 terminal remaining after evicting 3 of the 5 terminal jobs"
    );
}

#[test]
fn ties_on_completed_at_break_deterministically_by_id() {
    // All five jobs completed within the same second — the exact burst
    // scenario from the PR #229 round-3 review: `completed_at` has
    // whole-second resolution, so the primary sort key ties across the
    // board and the id tie-break alone must decide, deterministically
    // (ULIDs sort lexicographically; these hand-picked ids stand in for
    // that ordering).
    let mut registry: HashMap<String, IndexJob> = HashMap::new();
    for id in ["tie-e", "tie-a", "tie-c", "tie-b", "tie-d"] {
        registry.insert(id.to_string(), terminal_job(id, "2020-01-01T00:00:00Z"));
    }

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, PROTECT_NONE);

    assert_eq!(registry.len(), 3);
    assert!(
        !registry.contains_key("tie-a") && !registry.contains_key("tie-b"),
        "with all completed_at equal, the two lexicographically-smallest ids must be evicted"
    );
    assert!(registry.contains_key("tie-c"));
    assert!(registry.contains_key("tie-d"));
    assert!(registry.contains_key("tie-e"));
}

#[test]
fn never_evicts_the_job_whose_transition_triggered_the_eviction() {
    // The protected job sorts as the single oldest candidate (it ties the
    // others on completed_at and has the smallest id) — without the
    // protection rule it would be evicted by its own terminal transition,
    // closing its progress channel while `get_job` on its id already 404s
    // (the attach-failure scenario from the PR #229 round-3 review).
    let mut registry: HashMap<String, IndexJob> = HashMap::new();
    for id in ["job-a", "job-b", "job-c", "job-d"] {
        registry.insert(id.to_string(), terminal_job(id, "2020-01-01T00:00:00Z"));
    }

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, "job-a");

    assert!(
        registry.contains_key("job-a"),
        "the job whose terminal write triggered eviction must survive it, \
         even when it sorts oldest"
    );
    assert!(
        !registry.contains_key("job-b"),
        "the next candidate in (completed_at, id) order is evicted instead"
    );
    assert_eq!(registry.len(), 3);
}

// ---------------------------------------------------------------------------
// Wiring: the real MAX_TERMINAL_JOBS cap, through JobQueue::submit
// ---------------------------------------------------------------------------

/// Proves eviction is actually wired into `process_job`'s terminal-write
/// path (not just correct in isolation), and that `get_job` on an evicted
/// id returns `None` — the direct, guaranteed consequence of eviction
/// removing the registry entry `get_job` reads from.
///
/// Can't assert *which* specific ids were evicted: `completed_at` is a
/// fixed constant under `#[cfg(test)]` (see this module's doc comment), so
/// every job submitted here ties on the primary sort key, and the id
/// tie-break (PR #229 round-3 review) — while deterministic given the ids —
/// doesn't map to submission order for jobs whose ULIDs share a millisecond
/// (the random component decides within one ms). What *is* deterministic
/// regardless: the terminal count never exceeds the cap, and exactly the
/// overflow amount of ids become unresolvable via `get_job` once every
/// submitted job has settled. The tie-break's id-ordering itself is pinned
/// by the pure-function tests above, where ids are hand-chosen.
#[tokio::test]
async fn eviction_caps_the_registry_and_get_job_returns_none_for_evicted_jobs() {
    let queue = JobQueue::new();
    let overflow = 5;
    let total = MAX_TERMINAL_JOBS + overflow;

    let mut ids = Vec::with_capacity(total);
    for i in 0..total {
        let job = queue
            .submit(&format!("store-{i}"), IndexJobScope::Store, ok_job)
            .await
            .unwrap();
        ids.push(job.id);
    }

    // Every submitted job must settle (Done, since `ok_job` never fails) —
    // bounded poll per id, mirroring `wait_for_done`'s own deadline
    // discipline, but tolerating that some ids may already be evicted
    // (and so unresolvable) by the time we get to checking them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let jobs = queue.list_jobs().await;
        let still_running = jobs
            .iter()
            .filter(|j| !matches!(j.state, IndexJobState::Done | IndexJobState::Failed))
            .count();
        if still_running == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("not every submitted job reached a terminal state in time");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_jobs = queue.list_jobs().await;
    assert_eq!(
        final_jobs.len(),
        MAX_TERMINAL_JOBS,
        "the registry must never hold more than MAX_TERMINAL_JOBS terminal jobs"
    );

    let mut missing = 0;
    for id in &ids {
        if queue.get_job(id).await.is_none() {
            missing += 1;
        }
    }
    assert_eq!(
        missing, overflow,
        "exactly the overflow amount of submitted jobs must now be unresolvable via get_job \
         (evicted), regardless of which specific ones the tie-break picked"
    );

    // Sanity: `wait_for_done` on a *surviving* id still resolves normally —
    // eviction didn't corrupt the registry for jobs it left alone. Any
    // entry still in `final_jobs` is, by definition, a survivor.
    let surviving_id = final_jobs
        .first()
        .map(|j| j.id.clone())
        .expect("at least one job must have survived eviction");
    let done = wait_for_done(&queue, &surviving_id).await;
    assert_eq!(done.state, IndexJobState::Done);
}
