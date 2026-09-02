//! `spawn_url_scheduler`'s startup registration loop: which pre-existing
//! sources it registers with the `UrlRefreshScheduler` when the daemon
//! starts (as opposed to `AppState::add_source`'s live-registration path,
//! covered in `state::tests::refresh_scheduling`).

use localdb_core::ingestion::now_rfc3339;
use localdb_core::types::SourceKind;
use localdb_core::SourceRow;

use crate::daemon::spawn_url_scheduler;
use crate::job_queue::JobQueue;
use crate::scheduler::UrlRefreshScheduler;

use super::common::make_state;

fn source_row(store_id: &str, kind: SourceKind, url: &str) -> SourceRow {
    SourceRow {
        id: localdb_core::new_ulid(),
        store_id: store_id.to_string(),
        kind,
        root: None,
        url: Some(url.to_string()),
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: Some("1h".to_string()),
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    }
}

/// Poll `scheduler.source_count()` until it reaches `expected`, up to 5s —
/// registration happens on a spawned background task, not synchronously.
async fn wait_for_source_count(scheduler: &UrlRefreshScheduler, expected: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if scheduler.source_count().await == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scheduler never reached {expected} registered source(s), got {}",
            scheduler.source_count().await
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Rows persisted directly against the backend (bypassing
/// `AppState::add_source`) simulate sources that already existed before this
/// daemon start — exactly the scenario `spawn_url_scheduler` exists for: an
/// operator's `refresh` interval, set months ago, must be picked back up on
/// restart without requiring the source to be re-added.
#[tokio::test]
async fn spawn_url_scheduler_registers_both_url_and_feed_sources() {
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let store = state
        .backend()
        .get_store_by_name("notes")
        .await
        .unwrap()
        .unwrap();

    state
        .backend()
        .upsert_source(&source_row(
            &store.id,
            SourceKind::Url,
            "https://example.com/page",
        ))
        .await
        .unwrap();
    state
        .backend()
        .upsert_source(&source_row(
            &store.id,
            SourceKind::Feed,
            "https://example.com/feed.xml",
        ))
        .await
        .unwrap();

    let queue = JobQueue::new();
    let scheduler = UrlRefreshScheduler::new(queue);
    spawn_url_scheduler(&state, scheduler.clone());

    wait_for_source_count(&scheduler, 2).await;
}
