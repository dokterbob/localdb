//! Bounded terminal-job retention (issue #218-followups Fix 2, PR #229
//! review): the registry evicts the oldest `Done`/`Failed` jobs once their
//! count exceeds a cap, so a long-running daemon's job history doesn't grow
//! without bound.
//!
//! The pure eviction function (`evict_oldest_terminal_jobs_over_cap`) is
//! tested directly against hand-built registries below, with small,
//! test-chosen `cap`/`cutoff` values — not the real `MAX_TERMINAL_JOBS`
//! (200) or a real minute-long grace wait; hand-built fixtures with
//! explicit `completed_at` strings make ordering, grace, and protection
//! each provable in isolation. The one real end-to-end test below (through
//! `JobQueue::submit` at the real constants) pins the production-wired
//! consequence of the retention grace (PR #229 round-5 review): a burst of
//! fresh completions past the cap evicts *nothing*, so a submitter's first
//! post-submit attach/poll can never 404.
//!
//! `PROTECT_NONE` below: the pure tests that aren't about the
//! self-protection rule pass an id that matches nothing in their fixture,
//! so protection is inert and the test exercises only the ordering/cap
//! logic. The self-protection rule has its own dedicated test; same idea
//! for `CUTOFF_EVICT_ALL` and the grace rule.

use std::collections::HashMap;

use localdb_core::{IndexJob, IndexJobScope, IndexJobState, IndexJobStats};

use super::common::{ok_job, wait_for_done};
use crate::job_queue::{evict_oldest_terminal_jobs_over_cap, JobQueue, MAX_TERMINAL_JOBS};

/// Build a sample terminal or non-terminal `IndexJob` for the pure eviction
/// tests below — `completed_at` is the field eviction sorts and
/// grace-filters by, always explicit here so ordering and grace are fully
/// under the test's control (real jobs get wall-clock timestamps from
/// `localdb_core::ingestion::now_rfc3339`).
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

/// A `cutoff` far past every fixture's `completed_at` — every terminal
/// fixture is older than it, so the retention grace (PR #229 round-5
/// review) is inert and the test exercises only the ordering/cap logic.
/// The grace rule has its own dedicated test.
const CUTOFF_EVICT_ALL: &str = "9999-01-01T00:00:00Z";

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

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, PROTECT_NONE, CUTOFF_EVICT_ALL);
    assert_eq!(registry.len(), 3, "at the cap exactly: nothing evicted");

    evict_oldest_terminal_jobs_over_cap(&mut registry, 5, PROTECT_NONE, CUTOFF_EVICT_ALL);
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

    evict_oldest_terminal_jobs_over_cap(&mut registry, 4, PROTECT_NONE, CUTOFF_EVICT_ALL);

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

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, PROTECT_NONE, CUTOFF_EVICT_ALL);

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
    evict_oldest_terminal_jobs_over_cap(&mut registry, 2, PROTECT_NONE, CUTOFF_EVICT_ALL);

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

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, PROTECT_NONE, CUTOFF_EVICT_ALL);

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

    evict_oldest_terminal_jobs_over_cap(&mut registry, 3, "job-a", CUTOFF_EVICT_ALL);

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

/// The retention grace (PR #229 round-5 review): terminal jobs younger
/// than the cutoff are never eviction candidates, even when the terminal
/// count is over cap — the registry deliberately stays over cap until they
/// age out. Also pins the mixed case: aged entries are still trimmed while
/// fresh ones survive.
#[test]
fn never_evicts_terminal_jobs_within_the_retention_grace() {
    let mut registry: HashMap<String, IndexJob> = HashMap::new();
    // Three aged jobs (before the cutoff) and three fresh ones (at/after
    // the cutoff — a burst that just completed).
    for (id, at) in [
        ("aged-a", "2020-01-01T00:00:00Z"),
        ("aged-b", "2020-01-02T00:00:00Z"),
        ("aged-c", "2020-01-03T00:00:00Z"),
        ("fresh-a", "2020-06-01T00:00:00Z"),
        ("fresh-b", "2020-06-01T00:00:01Z"),
        ("fresh-c", "2020-06-01T00:00:02Z"),
    ] {
        registry.insert(id.to_string(), terminal_job(id, at));
    }
    let cutoff = "2020-06-01T00:00:00Z"; // fresh-* are >= cutoff → protected

    // Cap 1 with 6 terminal entries: overflow is 5, but only the three
    // aged entries are candidates — the registry stays over cap rather
    // than touching anything within the grace.
    evict_oldest_terminal_jobs_over_cap(&mut registry, 1, PROTECT_NONE, cutoff);

    assert!(
        !registry.contains_key("aged-a")
            && !registry.contains_key("aged-b")
            && !registry.contains_key("aged-c"),
        "aged entries must still be trimmed"
    );
    for id in ["fresh-a", "fresh-b", "fresh-c"] {
        assert!(
            registry.contains_key(id),
            "a terminal job within the retention grace must never be evicted \
             ({id} was) — over-cap is deliberate until entries age out"
        );
    }
    assert_eq!(registry.len(), 3, "over cap by the fresh burst, by design");
}

// ---------------------------------------------------------------------------
// Wiring: the real constants, through JobQueue::submit
// ---------------------------------------------------------------------------

/// Aged-out overflow is trimmed by `list_jobs` itself (PR #229 round-6
/// review): eviction otherwise only runs on terminal writes, so a burst
/// past the cap with no *subsequent* completions would keep its overflow
/// entries forever. Stages aged terminal entries directly in the registry
/// (`test_insert_job` — real jobs get wall-clock `completed_at`, which
/// can't age past the grace inside a deterministic test) and asserts one
/// `list_jobs` call, with no terminal write anywhere in between, trims
/// them to the cap.
#[tokio::test]
async fn list_jobs_trims_aged_overflow_without_a_terminal_write() {
    let queue = JobQueue::new();
    let overflow = 5;
    let total = MAX_TERMINAL_JOBS + overflow;
    for i in 0..total {
        let id = format!("aged-{i:03}");
        queue
            .test_insert_job(terminal_job(&id, "2020-01-01T00:00:00Z"))
            .await;
    }

    let listed = queue.list_jobs().await;

    assert_eq!(
        listed.len(),
        MAX_TERMINAL_JOBS,
        "list_jobs must sweep aged-out terminal overflow down to the cap"
    );
    for i in 0..overflow {
        let id = format!("aged-{i:03}");
        assert!(
            queue.get_job(&id).await.is_none(),
            "the {overflow} oldest-by-(completed_at, id) entries must be the evicted ones \
             ({id} should be gone)"
        );
    }
}

/// Proves the production wiring of the retention grace (PR #229 round-5
/// review): a burst of completions past `MAX_TERMINAL_JOBS` evicts
/// *nothing*, because every job just completed and is inside
/// `TERMINAL_RETENTION_GRACE_SECS`. This is the guarantee that closes the
/// submit→first-attach gap — a daemon client that just got its job id from
/// `POST /v1/jobs` can always still resolve it via `GET /v1/jobs/{id}`
/// (or subscribe) on its first request, no matter how many other jobs
/// completed in between; an in-flight `run_worker` teardown can therefore
/// never 404 a freshly-submitted job's poll fallback. Aged-entry trimming
/// at the cap is pinned by the pure-function tests above (a real test of
/// it here would need a minute-long wait — non-deterministic timing, per
/// #181's deterministic-tests rule).
#[tokio::test]
async fn fresh_terminal_burst_past_the_cap_is_fully_retained_and_resolvable() {
    let queue = JobQueue::new();
    let total = MAX_TERMINAL_JOBS + 5;

    let mut ids = Vec::with_capacity(total);
    for i in 0..total {
        let job = queue
            .submit(&format!("store-{i}"), IndexJobScope::Store, ok_job)
            .await
            .unwrap();
        ids.push(job.id);
    }

    // Every submitted job must settle (Done, since `ok_job` never fails).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let jobs = queue.list_jobs().await;
        let all_settled = jobs.len() == total
            && jobs
                .iter()
                .all(|j| matches!(j.state, IndexJobState::Done | IndexJobState::Failed));
        if all_settled {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("not every submitted job reached a terminal state in time");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Nothing evicted: all `total` jobs completed within the grace window
    // (this test runs in seconds, the grace is 60s), so the registry is
    // deliberately over cap and every single id — including the ones whose
    // completion pushed the count past MAX_TERMINAL_JOBS — still resolves.
    for id in &ids {
        assert!(
            queue.get_job(id).await.is_some(),
            "job {id} completed within the retention grace and must still \
             be resolvable via get_job"
        );
    }

    // Sanity: a resolved entry is a normal, fully-formed terminal job.
    let done = wait_for_done(&queue, &ids[0]).await;
    assert_eq!(done.state, IndexJobState::Done);
}
