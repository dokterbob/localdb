use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use super::common::{json_body, make_app};

#[tokio::test]
async fn list_stores_empty() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/stores")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn create_store_returns_201() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "my-notes"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["name"], "my-notes");
}

#[tokio::test]
async fn create_store_empty_name_returns_400() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": ""}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_store_invalid_visibility_returns_400() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"name": "my-notes", "visibility": "public"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_store_not_found_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/stores/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_store_not_found_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/v1/stores/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_store_updates_visibility() {
    let (_dir, app) = make_app().await;
    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "my-store"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri("/v1/stores/my-store")
                .header("content-type", "application/json")
                .body(Body::from(json!({"visibility": "shared"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["visibility"], "shared");
}

#[tokio::test]
async fn patch_store_not_found_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri("/v1/stores/no-such-store")
                .header("content-type", "application/json")
                .body(Body::from(json!({"visibility": "shared"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pagination_cursor_works() {
    let (_dir, app) = make_app().await;
    for name in &["alpha", "beta", "gamma"] {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": *name}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/stores?limit=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert!(body["next_cursor"].is_string());
    let cursor = body["next_cursor"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/stores?limit=2&cursor={}", cursor))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(body["next_cursor"].is_null());
}

#[tokio::test]
async fn list_stores_invalid_cursor_returns_400() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/stores?cursor=abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
