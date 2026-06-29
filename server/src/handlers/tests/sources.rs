use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use super::common::{json_body, make_app};

#[tokio::test]
async fn source_crud_roundtrip() {
    let (_dir, app) = make_app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "docs"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/docs/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "path",
                        "spec": {"root": "/tmp/docs", "include": [], "exclude": []},
                        "preset": "prose"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert!(body["id"].as_str().is_some());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/stores/docs/sources")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn delete_source_removes_it() {
    let (_dir, app) = make_app().await;
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "mystore"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/mystore/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "path",
                        "spec": {"root": "/tmp/mystore", "include": [], "exclude": []},
                        "preset": "prose"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    let source_id = body["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/v1/sources/{}", source_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/stores/mystore/sources")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn delete_nonexistent_source_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/v1/sources/nonexistent-src-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "source_not_found");
}
