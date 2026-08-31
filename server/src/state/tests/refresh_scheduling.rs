//! Source refresh-interval validation and scheduler registration tests.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::common::{make_attached_state, make_state};

// --- WS2: Validate refresh interval before persisting ---

#[tokio::test]
async fn add_source_invalid_refresh_is_rejected() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let result = state
        .add_source(
            "notes",
            "url",
            serde_json::json!({ "url": "https://example.com" }),
            "prose",
            Some("badvalue"),
        )
        .await;
    assert!(
        matches!(result, Err(localdb_core::Error::InvalidRequest { .. })),
        "expected InvalidRequest for invalid refresh, got: {:?}",
        result
    );
    // Nothing should have been persisted.
    let sources = state.list_sources("notes").await.unwrap();
    assert!(
        sources.is_empty(),
        "no source should be stored after invalid refresh"
    );
}

#[tokio::test]
async fn add_source_zero_refresh_is_rejected() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    for zero in &["0", "0s", "0m", "0h"] {
        let result = state
            .add_source(
                "notes",
                "url",
                serde_json::json!({ "url": "https://example.com" }),
                "prose",
                Some(zero),
            )
            .await;
        assert!(
            matches!(result, Err(localdb_core::Error::InvalidRequest { .. })),
            "expected InvalidRequest for zero refresh '{zero}', got: {:?}",
            result
        );
    }
    let sources = state.list_sources("notes").await.unwrap();
    assert!(
        sources.is_empty(),
        "no source should be stored after zero refresh"
    );
}

#[tokio::test]
async fn add_source_refresh_on_path_source_is_rejected() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let result = state
        .add_source(
            "notes",
            "path",
            serde_json::json!({"root": "/tmp/notes", "include": [], "exclude": []}),
            "prose",
            Some("1h"),
        )
        .await;
    assert!(
        matches!(result, Err(localdb_core::Error::InvalidRequest { .. })),
        "expected InvalidRequest for refresh on path source, got: {:?}",
        result
    );
    let sources = state.list_sources("notes").await.unwrap();
    assert!(
        sources.is_empty(),
        "no source should be stored when refresh on path source is rejected"
    );
}

#[tokio::test]
async fn add_source_valid_refresh_is_accepted() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    state
        .add_source(
            "notes",
            "url",
            serde_json::json!({ "url": "https://example.com" }),
            "prose",
            Some("1h"),
        )
        .await
        .unwrap();
    let sources = state.list_sources("notes").await.unwrap();
    assert_eq!(sources.len(), 1);
}

// --- WS3: Unregister scheduler records on delete ---

#[tokio::test]
async fn remove_source_unregisters_from_scheduler() {
    // Both url and feed sources register on add and unregister on remove —
    // the scheduler treats them identically.
    for kind in ["url", "feed"] {
        let (_dir, state) = make_state().await;
        state.add_store("notes", "private").await.unwrap();
        let src = state
            .add_source(
                "notes",
                kind,
                serde_json::json!({ "url": "https://example.com" }),
                "prose",
                Some("1h"),
            )
            .await
            .unwrap();
        assert_eq!(
            state.scheduler_source_count().await,
            1,
            "'{kind}' source should register with the scheduler on add"
        );
        state.remove_source(&src.id).await.unwrap();
        assert_eq!(
            state.scheduler_source_count().await,
            0,
            "url_scheduler should have 0 '{kind}' sources after remove_source"
        );
    }
}

// --- Feed sources actually get polled ---

/// A feed source with no `refresh` is tracked by the scheduler (so it shows
/// up in diagnostics and can still be refreshed manually) but never becomes
/// due on its own — `tick()` must never submit a job for it.
#[tokio::test]
async fn feed_source_without_refresh_is_tracked_but_never_due() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({"url": "https://example.com/feed.xml"}),
            "prose",
            None,
        )
        .await
        .unwrap();
    assert_eq!(state.scheduler_source_count().await, 1);

    state.tick_scheduler().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        state.job_queue().list_jobs().await.is_empty(),
        "a feed source with no refresh interval must never become due"
    );
}

/// A minimal RSS document with zero `<item>`s — with `fetch_full_content:
/// false` the feed document itself is the only document `FeedIngestor`
/// fetches, so this is the entire mock surface needed for a clean run.
fn minimal_rss() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Feed</title><link>https://feed.example.com/</link><description>d</description></channel></rss>"#.to_string()
}

/// `tick()` for a due feed source submits a job scoped to exactly that
/// source, and that job runs to completion through the same
/// `DeletionPolicy::Retain`-only path a url source's scheduled refresh
/// uses — automatic background polling never deletes, regardless of source
/// kind.
#[tokio::test]
async fn feed_source_tick_submits_source_scoped_job_that_completes() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(minimal_rss()))
        .mount(&server)
        .await;

    let (_dir, state) = make_attached_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({ "url": feed_url, "fetch_full_content": false }),
            "prose",
            Some("1h"),
        )
        .await
        .unwrap();
    assert_eq!(state.scheduler_source_count().await, 1);

    // Never refreshed yet, so the first tick is immediately due.
    state.tick_scheduler().await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let jobs = state.job_queue().list_jobs().await;
        if let Some(job) = jobs.first() {
            assert!(
                matches!(
                    &job.scope,
                    localdb_core::IndexJobScope::Source { source_id }
                        if source_id == &source.id
                ),
                "tick() should submit a job scoped to exactly the due feed source: {:?}",
                job.scope
            );
            match job.state {
                localdb_core::IndexJobState::Done => break,
                localdb_core::IndexJobState::Failed => {
                    panic!("feed refresh job failed: {:?}", job.error)
                }
                _ => {}
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "feed refresh job did not complete in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
