//! `get_or_build_embedder` cache tests.

use std::sync::Arc;

use super::common::make_state;

// --- get_or_build_embedder (Codex review finding F2, issue #187) -------

/// Three sequential calls with the same policy must build the embedder
/// exactly once — the read-lock fast path must hit on calls 2 and 3, and
/// every call must return the same `Arc`.
#[tokio::test]
async fn get_or_build_embedder_builds_once_across_repeated_calls() {
    let (_dir, state) = make_state().await;
    let yaml = state.yaml_config().await;

    let a = state.get_or_build_embedder(&yaml).await.unwrap();
    let b = state.get_or_build_embedder(&yaml).await.unwrap();
    let c = state.get_or_build_embedder(&yaml).await.unwrap();

    assert_eq!(
        state.embedder_build_count(),
        1,
        "embedder should be built exactly once across 3 calls with an unchanged policy"
    );
    assert!(
        Arc::ptr_eq(&a, &b),
        "second call should return the cached Arc"
    );
    assert!(
        Arc::ptr_eq(&a, &c),
        "third call should return the cached Arc"
    );
}

/// A changed `EmbeddingPolicy` (different model) must miss the cache and
/// rebuild, returning a distinct `Arc`.
#[tokio::test]
async fn get_or_build_embedder_rebuilds_on_policy_change() {
    let (_dir, state) = make_state().await;
    let mut yaml = state.yaml_config().await;

    let first = state.get_or_build_embedder(&yaml).await.unwrap();

    yaml.defaults.indexing.embedding.model = "different-model".to_string();
    let second = state.get_or_build_embedder(&yaml).await.unwrap();

    assert_eq!(
        state.embedder_build_count(),
        2,
        "a changed embedding policy should trigger a rebuild"
    );
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a rebuilt embedder must not be the same Arc as the stale cached one"
    );
}

/// After `reload_yaml_config` swaps in a config with a different
/// embedding policy, the next `get_or_build_embedder` call (using the
/// freshly reloaded snapshot, as every real call site does via
/// `state.yaml_config()`) must rebuild exactly once — no explicit cache
/// flush needed, since the policy comparison itself misses.
#[tokio::test]
async fn get_or_build_embedder_rebuilds_once_after_config_reload() {
    let (_dir, state) = make_state().await;
    let old_yaml = state.yaml_config().await;
    let old = state.get_or_build_embedder(&old_yaml).await.unwrap();

    let mut new_yaml = old_yaml.clone();
    new_yaml.defaults.indexing.embedding.model = "reloaded-model".to_string();
    state.reload_yaml_config(new_yaml).await;

    let reloaded_yaml = state.yaml_config().await;
    let rebuilt = state.get_or_build_embedder(&reloaded_yaml).await.unwrap();
    let rebuilt_again = state.get_or_build_embedder(&reloaded_yaml).await.unwrap();

    assert_eq!(
        state.embedder_build_count(),
        2,
        "should build once for the original policy, once more for the reloaded policy"
    );
    assert!(
        !Arc::ptr_eq(&old, &rebuilt),
        "post-reload embedder must not be the stale pre-reload Arc"
    );
    assert!(
        Arc::ptr_eq(&rebuilt, &rebuilt_again),
        "a second call against the same reloaded policy should hit the cache"
    );
}

/// An unchanged `EmbeddingPolicy` but a changed `providers` entry (e.g.
/// editing a hosted provider's `base_url` under `providers:` in the
/// YAML) must still miss the cache and rebuild — the cache key is
/// policy *and* the providers snapshot, not policy alone (Codex review
/// finding H1, issue #212).
#[tokio::test]
async fn get_or_build_embedder_rebuilds_on_provider_settings_change() {
    let (_dir, state) = make_state().await;
    let mut old_yaml = state.yaml_config().await;
    old_yaml.providers = vec![localdb_core::config::schema::ProviderConfig {
        name: "hosted".to_string(),
        kind: "openai-compatible".to_string(),
        base_url: Some("https://old.example.com".to_string()),
        api_key_env: Some("OLD_API_KEY".to_string()),
    }];
    state.reload_yaml_config(old_yaml.clone()).await;
    let first = state.get_or_build_embedder(&old_yaml).await.unwrap();

    let mut new_yaml = old_yaml.clone();
    new_yaml.providers[0].base_url = Some("https://new.example.com".to_string());
    state.reload_yaml_config(new_yaml.clone()).await;
    let second = state.get_or_build_embedder(&new_yaml).await.unwrap();

    assert_eq!(
        state.embedder_build_count(),
        2,
        "a changed provider base_url under an unchanged policy should trigger a rebuild"
    );
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a rebuilt embedder must not be the same Arc as the stale cached one"
    );
}

/// An unchanged `EmbeddingPolicy` and `providers` but a changed `http:`
/// block (e.g. editing `max_retries` or `user_agent`) must still miss
/// the cache and rebuild — the cache key is policy, providers, *and*
/// `http`, not policy+providers alone (issue #207 adversarial review,
/// finding 1). Without this, an operator flipping `http.max_retries` via
/// a live config reload would keep getting an embedder built from the
/// *old* `http:` snapshot indefinitely, since the policy/providers
/// equality check alone would report a cache hit.
#[tokio::test]
async fn get_or_build_embedder_rebuilds_on_http_config_change() {
    let (_dir, state) = make_state().await;
    let old_yaml = state.yaml_config().await;
    let first = state.get_or_build_embedder(&old_yaml).await.unwrap();

    let mut new_yaml = old_yaml.clone();
    new_yaml.http.max_retries = old_yaml.http.max_retries + 1;
    state.reload_yaml_config(new_yaml.clone()).await;
    let second = state.get_or_build_embedder(&new_yaml).await.unwrap();

    assert_eq!(
        state.embedder_build_count(),
        2,
        "a changed http.max_retries under an unchanged policy/providers should rebuild"
    );
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a rebuilt embedder must not be the same Arc as the stale cached one"
    );

    // A third call with the same (already-changed) http config must hit
    // the cache again — this isn't a "rebuild on every call" regression.
    let third = state.get_or_build_embedder(&new_yaml).await.unwrap();
    assert_eq!(
        state.embedder_build_count(),
        2,
        "an unchanged http config on a subsequent call should hit the cache, not rebuild"
    );
    assert!(
        Arc::ptr_eq(&second, &third),
        "third call should return the cached Arc from the second build"
    );
}
