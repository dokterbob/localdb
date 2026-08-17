//! `get_or_build_fetchers` cache tests.

use std::sync::Arc;

use super::common::make_state;

// -----------------------------------------------------------------
// get_or_build_fetchers (issue #208 PR #227 review): mirrors the
// get_or_build_embedder tests immediately above — same cache shape,
// same invalidation rule, same reason to test it. Deterministic
// Arc-identity assertions only, per the review's own guidance: a
// timing-based pacing test would be unreliable (repo #181; see also
// `verifying-http-pacing-e2e` — wall-clock A/B races the embedding
// step and is unsuited to a unit test).
// -----------------------------------------------------------------

/// Three calls with an unchanged `http:` config — standing in for three
/// separate `run_scoped_job` invocations, i.e. three daemon jobs
/// (`server.job_workers` > 1 lets them run concurrently) — must all
/// return the exact same cached `Arc<(HttpUrlFetcher, HttpUrlFetcher)>`.
/// This is what actually closes the issue #227 review finding: since
/// the pair's own `HostLimiter` lives inside it (shared between the
/// pair's two fetchers via `HttpUrlFetcher::new_pair`), returning the
/// same `Arc` means every job that reused it paces through the exact
/// same limiter state — not a fresh one each time.
#[tokio::test]
async fn get_or_build_fetchers_builds_once_across_repeated_calls() {
    let (_dir, state) = make_state().await;
    let yaml = state.yaml_config().await;

    let a = state.get_or_build_fetchers(&yaml).await.unwrap();
    let b = state.get_or_build_fetchers(&yaml).await.unwrap();
    let c = state.get_or_build_fetchers(&yaml).await.unwrap();

    assert!(
        Arc::ptr_eq(&a, &b),
        "second call should return the cached Arc"
    );
    assert!(
        Arc::ptr_eq(&a, &c),
        "third call should return the cached Arc"
    );
}

/// A changed `http:` block (e.g. an operator editing
/// `http.rate_limit.requests_per_second` and the daemon's config-file
/// watcher reloading it) must miss the cache and rebuild a fresh pair —
/// otherwise every job would keep pacing through a `HostLimiter` built
/// from stale rate-limit settings indefinitely.
#[tokio::test]
async fn get_or_build_fetchers_rebuilds_on_http_config_change() {
    let (_dir, state) = make_state().await;
    let old_yaml = state.yaml_config().await;
    let first = state.get_or_build_fetchers(&old_yaml).await.unwrap();

    let mut new_yaml = old_yaml.clone();
    new_yaml.http.max_retries = old_yaml.http.max_retries + 1;
    state.reload_yaml_config(new_yaml.clone()).await;
    let second = state.get_or_build_fetchers(&new_yaml).await.unwrap();

    assert!(
        !Arc::ptr_eq(&first, &second),
        "a rebuilt fetcher pair must not be the same Arc as the stale cached one"
    );

    // A third call with the same (already-changed) http config must hit
    // the cache again — this isn't a "rebuild on every call" regression.
    let third = state.get_or_build_fetchers(&new_yaml).await.unwrap();
    assert!(
        Arc::ptr_eq(&second, &third),
        "third call should return the cached Arc from the second build"
    );
}
