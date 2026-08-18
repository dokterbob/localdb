//! HTTP integration via `build_router`.

use std::sync::Arc;

use localdb_core::config::schema::RawConfig;

use crate::daemon::build_router;
use crate::job_queue::JobQueue;
use crate::scheduler::UrlRefreshScheduler;
use crate::state::AppState;

// --- HTTP integration via build_router ---

#[tokio::test]
async fn router_serves_status_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let yaml_config = RawConfig::default();
    let queue = JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue.clone(),
        UrlRefreshScheduler::new(queue),
    )
    .await
    .unwrap();
    // `vec![]` disables the Host check entirely (see `mcp_allowed_hosts`);
    // this test only drives `/v1/status` via `oneshot`, never `/mcp`.
    let app = build_router(
        state,
        vec![],
        Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
}
