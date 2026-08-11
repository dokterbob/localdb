use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use super::common::{json_body, make_app};

/// POST `body` to `uri` and return the parsed response — success-status
/// checks belong to the individual tests below, so this stays a thin
/// wrapper reusable for both 2xx setup calls and error-path assertions.
async fn post_json(app: &axum::Router, uri: &str, body: serde_json::Value) -> serde_json::Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "unexpected status for POST {uri}: {}",
        resp.status()
    );
    json_body(resp.into_body()).await
}

/// Poll `GET /v1/jobs/{id}` until the job reaches `done` or `failed`, up to
/// 10s. Returns the final job body. Panics on timeout — a job that never
/// terminates is itself a bug worth failing loudly on.
async fn poll_job_to_terminal(app: &axum::Router, job_id: &str) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("job {job_id} did not reach a terminal state in time");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/jobs/{}", job_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        if body["state"] == "done" || body["state"] == "failed" {
            return body;
        }
    }
}

#[tokio::test]
async fn post_job_returns_202() {
    let (_dir, app) = make_app().await;
    post_json(&app, "/v1/stores", json!({"name": "test"})).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/jobs")
                .header("content-type", "application/json")
                .body(Body::from(json!({"store_name": "test"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = json_body(resp.into_body()).await;
    assert!(body["id"].as_str().is_some());
}

#[tokio::test]
async fn get_job_not_found_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/jobs/nonexistent-job-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// THE regression test for issue #187 §1: `POST /v1/jobs` used to mark every
/// job `Completed` with `IndexJobStats::default()` — zero stats — without
/// ever running ingestion. A store with no sources legitimately produces
/// zero stats (`post_job_returns_202` above covers that), so this test seeds
/// a real `path` source pointing at a real file and asserts *nonzero* stats,
/// then confirms the content is actually searchable — the thing the old
/// stub could never produce no matter how it was polled.
#[tokio::test]
async fn post_and_poll_job_runs_real_ingestion_and_content_is_searchable() {
    let (_dir, app) = make_app().await;

    let content_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        content_dir.path().join("doc.md"),
        "the quokka is a small wallaby found on Rottnest Island",
    )
    .unwrap();

    post_json(&app, "/v1/stores", json!({"name": "test"})).await;
    post_json(
        &app,
        "/v1/stores/test/sources",
        json!({
            "kind": "path",
            "spec": {"root": content_dir.path().to_string_lossy()},
        }),
    )
    .await;

    let job = post_json(&app, "/v1/jobs", json!({"store_name": "test"})).await;
    let job_id = job["id"].as_str().unwrap().to_string();

    let final_job = poll_job_to_terminal(&app, &job_id).await;
    assert_eq!(
        final_job["state"], "done",
        "job should complete successfully: {:?}",
        final_job
    );
    assert!(
        final_job["stats"]["docs_indexed"].as_u64().unwrap() > 0,
        "expected nonzero docs_indexed, got: {:?}",
        final_job["stats"]
    );
    assert!(
        final_job["stats"]["chunks_written"].as_u64().unwrap() > 0,
        "expected nonzero chunks_written, got: {:?}",
        final_job["stats"]
    );

    let search_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": "quokka"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search_resp.status(), StatusCode::OK);
    let search_body = json_body(search_resp.into_body()).await;
    let citations = search_body["citations"].as_array().unwrap();
    assert!(
        !citations.is_empty(),
        "the just-indexed content should be searchable: {:?}",
        search_body
    );
    assert!(
        citations
            .iter()
            .any(|c| c["uri"].as_str().is_some_and(|u| u.contains("doc.md"))),
        "search results should reference the indexed file: {:?}",
        citations
    );
}

#[tokio::test]
async fn create_job_nonexistent_store_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/jobs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"store_name": "no-such-store"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "store_not_found");
}

// --- deletion_policy (#187 deliverable D6) -----------------------------

#[tokio::test]
async fn create_job_rejects_invalid_deletion_policy() {
    let (_dir, app) = make_app().await;
    post_json(&app, "/v1/stores", json!({"name": "test"})).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/jobs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"store_name": "test", "deletion_policy": "obliterate"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "invalid_request");
}

/// Shared setup for the retain/delete pair below: a store with a `path`
/// source over a directory containing two files, both indexed by an initial
/// job, then one file removed from disk before the second job runs.
async fn setup_two_docs_then_remove_one(app: &axum::Router) -> (tempfile::TempDir, String) {
    let content_dir = tempfile::tempdir().unwrap();
    std::fs::write(content_dir.path().join("keep.md"), "alpha bravo charlie").unwrap();
    std::fs::write(content_dir.path().join("gone.md"), "delta echo foxtrot").unwrap();

    post_json(app, "/v1/stores", json!({"name": "test"})).await;
    post_json(
        app,
        "/v1/stores/test/sources",
        json!({
            "kind": "path",
            "spec": {"root": content_dir.path().to_string_lossy()},
        }),
    )
    .await;

    let job = post_json(app, "/v1/jobs", json!({"store_name": "test"})).await;
    let job_id = job["id"].as_str().unwrap().to_string();
    let final_job = poll_job_to_terminal(app, &job_id).await;
    assert_eq!(final_job["state"], "done");
    assert_eq!(final_job["stats"]["docs_indexed"], 2);

    std::fs::remove_file(content_dir.path().join("gone.md")).unwrap();

    (content_dir, "test".to_string())
}

/// Whether any citation from a `query` search references a uri containing
/// `needle`. Checks citation identity (uri substring), not mere non-empty
/// results — RRF fusion can return a store's only remaining document as a
/// low-relevance dense-channel hit for an unrelated query, so "citations
/// non-empty" is not a reliable "this specific document is present" signal
/// once a store holds just one document.
async fn search_returns_uri_containing(app: &axum::Router, query: &str, needle: &str) -> bool {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": query}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    body["citations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["uri"].as_str().is_some_and(|u| u.contains(needle)))
}

#[tokio::test]
async fn deletion_policy_delete_removes_gone_document_from_search() {
    let (_dir, app) = make_app().await;
    let (_content_dir, store_name) = setup_two_docs_then_remove_one(&app).await;

    let job = post_json(
        &app,
        "/v1/jobs",
        json!({"store_name": store_name, "deletion_policy": "delete"}),
    )
    .await;
    let job_id = job["id"].as_str().unwrap().to_string();
    let final_job = poll_job_to_terminal(&app, &job_id).await;
    assert_eq!(final_job["state"], "done", "job: {:?}", final_job);
    assert_eq!(
        final_job["stats"]["docs_deleted"], 1,
        "expected the removed file to be pruned: {:?}",
        final_job["stats"]
    );

    assert!(
        search_returns_uri_containing(&app, "alpha bravo", "keep.md").await,
        "the kept document should still be searchable"
    );
    assert!(
        !search_returns_uri_containing(&app, "delta echo", "gone.md").await,
        "the deleted document must no longer be searchable"
    );
}

#[tokio::test]
async fn deletion_policy_default_retain_keeps_gone_document_in_search() {
    let (_dir, app) = make_app().await;
    let (_content_dir, store_name) = setup_two_docs_then_remove_one(&app).await;

    // No `deletion_policy` field at all — must default to retain.
    let job = post_json(&app, "/v1/jobs", json!({"store_name": store_name})).await;
    let job_id = job["id"].as_str().unwrap().to_string();
    let final_job = poll_job_to_terminal(&app, &job_id).await;
    assert_eq!(final_job["state"], "done", "job: {:?}", final_job);
    assert_eq!(
        final_job["stats"]["docs_deleted"], 0,
        "a retaining run must never delete: {:?}",
        final_job["stats"]
    );

    assert!(
        search_returns_uri_containing(&app, "alpha bravo", "keep.md").await,
        "the kept document should still be searchable"
    );
    assert!(
        search_returns_uri_containing(&app, "delta echo", "gone.md").await,
        "a retaining run must keep the now-gone document searchable"
    );
}

// --- GET /v1/jobs/{id}/events (SSE live progress, issue #83) ----------

/// One parsed SSE frame: its `event:` name and the raw text of its
/// `data:` field (still JSON-encoded — callers parse it themselves).
struct SseFrame {
    event: String,
    data: String,
}

/// Parse a raw `text/event-stream` body into its frames.
///
/// Frames are separated by a blank line; within a frame, a `event: <name>`
/// line sets the name and a `data: <value>` line sets the data. Good enough
/// for the single-field-per-line frames this handler emits — not a
/// general-purpose SSE parser.
fn parse_sse_frames(body: &str) -> Vec<SseFrame> {
    body.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut event = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    event = rest.to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_string();
                }
            }
            SseFrame { event, data }
        })
        .collect()
}

/// GET `/v1/jobs/{id}/events` and return its parsed SSE frames. Bounded by a
/// timeout — a stream that never terminates is itself a bug worth failing
/// loudly on, per this file's existing `poll_job_to_terminal` convention.
async fn get_job_events(app: &axum::Router, job_id: &str) -> Vec<SseFrame> {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/jobs/{}/events", job_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(resp.into_body(), usize::MAX),
    )
    .await
    .expect("SSE stream did not terminate in time")
    .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    parse_sse_frames(&body)
}

/// THE regression test for issue #83: subscribing to a running job's
/// `/events` must observe at least one `progress` frame from real ingestion
/// (not a stub), followed by exactly one terminal `job` frame reporting
/// `done` with nonzero `chunks_written` — mirroring
/// `post_and_poll_job_runs_real_ingestion_and_content_is_searchable`'s setup
/// but asserting on the live stream instead of polling `GET /jobs/{id}`.
#[tokio::test]
async fn sse_events_stream_progress_then_terminal_job_event() {
    let (_dir, app) = make_app().await;

    let content_dir = tempfile::tempdir().unwrap();
    std::fs::write(content_dir.path().join("alpha.md"), "alpha bravo charlie").unwrap();
    std::fs::write(content_dir.path().join("delta.md"), "delta echo foxtrot").unwrap();

    post_json(&app, "/v1/stores", json!({"name": "test"})).await;
    post_json(
        &app,
        "/v1/stores/test/sources",
        json!({
            "kind": "path",
            "spec": {"root": content_dir.path().to_string_lossy()},
        }),
    )
    .await;

    let job = post_json(&app, "/v1/jobs", json!({"store_name": "test"})).await;
    let job_id = job["id"].as_str().unwrap().to_string();

    // Subscribe immediately — the job may still be Pending/Running at this
    // point, which is exactly the "running job" path this test exercises.
    let frames = get_job_events(&app, &job_id).await;

    assert!(
        !frames.is_empty(),
        "expected at least the terminal job frame"
    );

    let (job_frames, progress_frames): (Vec<_>, Vec<_>) =
        frames.iter().partition(|f| f.event == "job");
    assert_eq!(
        job_frames.len(),
        1,
        "expected exactly one terminal job frame, got frames: {:?}",
        frames.iter().map(|f| &f.event).collect::<Vec<_>>()
    );

    assert!(
        !progress_frames.is_empty(),
        "expected at least one progress frame from real ingestion"
    );
    // At least one recognizable ingestion-level event — source-started or a
    // per-document event — proves this is real pipeline progress, not an
    // empty placeholder stream.
    assert!(
        progress_frames.iter().any(|f| {
            let v: serde_json::Value = serde_json::from_str(&f.data).unwrap();
            matches!(
                v["type"].as_str(),
                Some("source_started") | Some("document_started") | Some("document_finished")
            )
        }),
        "expected a source/document-level progress event, got: {:?}",
        progress_frames.iter().map(|f| &f.data).collect::<Vec<_>>()
    );

    let final_job: serde_json::Value = serde_json::from_str(&job_frames[0].data).unwrap();
    assert_eq!(final_job["state"], "done", "job: {:?}", final_job);
    assert!(
        final_job["stats"]["chunks_written"].as_u64().unwrap() > 0,
        "expected nonzero chunks_written, got: {:?}",
        final_job["stats"]
    );
}

/// Late subscribe: a job that has already finished must yield exactly the
/// terminal `job` frame, immediately, with no `progress` frames — the
/// "already terminal at subscribe time" path.
#[tokio::test]
async fn sse_events_late_subscribe_yields_only_terminal_job_event() {
    let (_dir, app) = make_app().await;

    let content_dir = tempfile::tempdir().unwrap();
    std::fs::write(content_dir.path().join("doc.md"), "quokka wallaby island").unwrap();

    post_json(&app, "/v1/stores", json!({"name": "test"})).await;
    post_json(
        &app,
        "/v1/stores/test/sources",
        json!({
            "kind": "path",
            "spec": {"root": content_dir.path().to_string_lossy()},
        }),
    )
    .await;

    let job = post_json(&app, "/v1/jobs", json!({"store_name": "test"})).await;
    let job_id = job["id"].as_str().unwrap().to_string();

    // Run the job to completion first via the existing polling helper.
    let final_job = poll_job_to_terminal(&app, &job_id).await;
    assert_eq!(final_job["state"], "done", "job: {:?}", final_job);

    let frames = get_job_events(&app, &job_id).await;

    assert_eq!(
        frames.len(),
        1,
        "expected exactly one frame (the terminal job event), got: {:?}",
        frames.iter().map(|f| &f.event).collect::<Vec<_>>()
    );
    assert_eq!(frames[0].event, "job");
    let body: serde_json::Value = serde_json::from_str(&frames[0].data).unwrap();
    assert_eq!(body["state"], "done");
    assert_eq!(body["id"], job_id);
}

#[tokio::test]
async fn sse_events_unknown_job_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/jobs/nonexistent-job-id/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "job_not_found");
}

#[tokio::test]
async fn deletion_policy_explicit_retain_behaves_like_default() {
    let (_dir, app) = make_app().await;
    let (_content_dir, store_name) = setup_two_docs_then_remove_one(&app).await;

    let job = post_json(
        &app,
        "/v1/jobs",
        json!({"store_name": store_name, "deletion_policy": "retain"}),
    )
    .await;
    let job_id = job["id"].as_str().unwrap().to_string();
    let final_job = poll_job_to_terminal(&app, &job_id).await;
    assert_eq!(final_job["state"], "done", "job: {:?}", final_job);
    assert_eq!(final_job["stats"]["docs_deleted"], 0);
}
