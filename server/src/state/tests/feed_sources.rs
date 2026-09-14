//! Feed-source (`kind = "feed"`) tests.

use localdb_core::Error;

use super::common::make_state;

// --- #116: feed sources ---

#[tokio::test]
async fn add_feed_source_persists_clean_spec_and_config_json() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({
                "url": "https://example.com/feed.xml",
                "max_entries": 25,
                "fetch_full_content": false,
            }),
            "prose",
            None,
        )
        .await
        .unwrap();

    let fetched = state.get_source(&source.id).await.unwrap();
    assert_eq!(fetched.kind, "feed");
    assert_eq!(fetched.spec["url"], "https://example.com/feed.xml");
    assert_eq!(fetched.spec["max_entries"], 25);
    assert_eq!(fetched.spec["fetch_full_content"], false);
    // Never leak the raw config_json blob through the reconstructed spec.
    assert!(fetched.spec.get("config_json").is_none());
}

#[tokio::test]
async fn add_feed_source_bad_url_is_rejected() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let result = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({"url": "ftp://example.com/feed.xml"}),
            "prose",
            None,
        )
        .await;
    assert!(matches!(result, Err(Error::InvalidRequest { .. })));
    assert!(state.list_sources("notes").await.unwrap().is_empty());
}

#[tokio::test]
async fn add_feed_source_max_entries_zero_is_rejected() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let result = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({
                "url": "https://example.com/feed.xml",
                "max_entries": 0,
            }),
            "prose",
            None,
        )
        .await;
    assert!(matches!(result, Err(Error::InvalidRequest { .. })));
    assert!(state.list_sources("notes").await.unwrap().is_empty());
}

#[tokio::test]
async fn add_feed_source_refresh_is_accepted_and_surfaced() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({"url": "https://example.com/feed.xml"}),
            "prose",
            Some("1h"),
        )
        .await
        .unwrap();
    assert_eq!(source.refresh.as_deref(), Some("1h"));

    let fetched = state.get_source(&source.id).await.unwrap();
    assert_eq!(fetched.refresh.as_deref(), Some("1h"));
}

#[tokio::test]
async fn add_feed_source_registers_with_url_scheduler() {
    // A feed source with a refresh interval registers with the scheduler on
    // the same terms as a url source, so it actually gets polled.
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({"url": "https://example.com/feed.xml"}),
            "prose",
            Some("1h"),
        )
        .await
        .unwrap();
    assert_eq!(state.scheduler_source_count().await, 1);
}

#[tokio::test]
async fn add_source_same_url_across_kinds_is_rejected_known_limitation() {
    // Known limitation (#116): `idx_sources_store_url` is UNIQUE on
    // (store_id, url) regardless of kind, so a url source and a feed
    // source can never coexist on the same URL within a store even
    // though they index semantically different content (raw page vs.
    // feed entries). This pins the current cross-kind ownership
    // behavior; making the constraint kind-aware is a follow-up, not
    // part of #116.
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    state
        .add_source(
            "notes",
            "url",
            serde_json::json!({"url": "https://example.com/same"}),
            "prose",
            None,
        )
        .await
        .unwrap();

    let result = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({"url": "https://example.com/same"}),
            "prose",
            None,
        )
        .await;
    assert!(
        matches!(result, Err(Error::InvalidRequest { .. })),
        "expected InvalidRequest (duplicate URL across kinds), got: {:?}",
        result
    );
    assert_eq!(state.list_sources("notes").await.unwrap().len(), 1);
}

#[tokio::test]
async fn remove_store_unregisters_all_sources() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    state
        .add_source(
            "notes",
            "url",
            serde_json::json!({ "url": "https://example.com/a" }),
            "prose",
            Some("1h"),
        )
        .await
        .unwrap();
    state
        .add_source(
            "notes",
            "url",
            serde_json::json!({ "url": "https://example.com/b" }),
            "prose",
            Some("2h"),
        )
        .await
        .unwrap();
    state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({ "url": "https://example.com/feed.xml" }),
            "prose",
            Some("3h"),
        )
        .await
        .unwrap();
    assert_eq!(state.scheduler_source_count().await, 3);
    state.remove_store("notes").await.unwrap();
    assert_eq!(
        state.scheduler_source_count().await,
        0,
        "url_scheduler should have 0 sources (url and feed alike) after remove_store"
    );
}
