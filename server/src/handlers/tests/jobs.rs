use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use super::common::{json_body, make_app};

#[tokio::test]
async fn post_job_returns_202() {
    let (_dir, app) = make_app().await;
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "test"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

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

#[tokio::test]
async fn post_and_poll_job() {
    let (_dir, app) = make_app().await;
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "test"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .clone()
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
    let body = json_body(resp.into_body()).await;
    let job_id = body["id"].as_str().unwrap().to_string();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("job did not complete in time");
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
        if body["state"] == "done" {
            break;
        }
    }
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
