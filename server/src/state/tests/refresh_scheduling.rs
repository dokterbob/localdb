//! Source refresh-interval validation and scheduler registration tests.

use super::common::make_state;

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
    let (_dir, state) = make_state().await;
    state.add_store("notes", "private").await.unwrap();
    let src = state
        .add_source(
            "notes",
            "url",
            serde_json::json!({ "url": "https://example.com" }),
            "prose",
            Some("1h"),
        )
        .await
        .unwrap();
    assert_eq!(state.scheduler_source_count().await, 1);
    state.remove_source(&src.id).await.unwrap();
    assert_eq!(
        state.scheduler_source_count().await,
        0,
        "url_scheduler should have 0 sources after remove_source"
    );
}
