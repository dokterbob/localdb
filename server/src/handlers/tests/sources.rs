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

// --- #116: feed sources ---

async fn create_store_named(app: &axum::Router, name: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": name}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_feed_source_returns_201_with_clean_spec() {
    let (_dir, app) = make_app().await;
    create_store_named(&app, "docs").await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/docs/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "feed",
                        "spec": {
                            "url": "https://example.com/feed.xml",
                            "max_entries": 25,
                            "fetch_full_content": false,
                        },
                        "preset": "prose",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["kind"], "feed");
    // The spec must be a clean reconstruction — never the raw config_json blob.
    assert_eq!(body["spec"]["url"], "https://example.com/feed.xml");
    assert_eq!(body["spec"]["max_entries"], 25);
    assert_eq!(body["spec"]["fetch_full_content"], false);
    assert!(body["spec"].get("config_json").is_none());
}

#[tokio::test]
async fn create_feed_source_bad_url_returns_400() {
    let (_dir, app) = make_app().await;
    create_store_named(&app, "docs").await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/docs/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "feed",
                        "spec": {"url": "ftp://example.com/feed.xml"},
                        "preset": "prose",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn create_feed_source_max_entries_zero_returns_400() {
    let (_dir, app) = make_app().await;
    create_store_named(&app, "docs").await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/docs/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "feed",
                        "spec": {
                            "url": "https://example.com/feed.xml",
                            "max_entries": 0,
                        },
                        "preset": "prose",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn create_source_unknown_kind_still_rejected() {
    let (_dir, app) = make_app().await;
    create_store_named(&app, "docs").await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/docs/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "rss",
                        "spec": {"url": "https://example.com/feed.xml"},
                        "preset": "prose",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_feed_source_with_refresh_is_persisted_and_returned() {
    let (_dir, app) = make_app().await;
    create_store_named(&app, "docs").await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/docs/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "feed",
                        "spec": {"url": "https://example.com/feed.xml"},
                        "preset": "prose",
                        "refresh": "1h",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["refresh"], "1h");
}

#[tokio::test]
async fn list_sources_includes_feed_arm_record_shape() {
    let (_dir, app) = make_app().await;
    create_store_named(&app, "docs").await;

    app.clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/stores/docs/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "kind": "feed",
                        "spec": {
                            "url": "https://example.com/feed.xml",
                            "max_entries": 5,
                        },
                        "preset": "prose",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
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
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "feed");
    assert_eq!(items[0]["spec"]["max_entries"], 5);
    assert_eq!(items[0]["spec"]["fetch_full_content"], true);
}
