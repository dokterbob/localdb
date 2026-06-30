mod common;

use axum::http::{Method, StatusCode};
use serde_json::json;

use common::{create_store, json_body, make_app, request};

#[tokio::test]
async fn source_routes_roundtrip_path_and_url_specs() {
    // Given: a runtime store that will own both source kinds.
    let (_dir, app) = make_app().await;
    create_store(app.clone(), "docs").await;

    // When: path and URL sources are created, then listed.
    let path_source = request(
        app.clone(),
        Method::POST,
        "/v1/stores/docs/sources",
        Some(json!({
            "kind": "path",
            "spec": {"root": "/tmp/docs", "include": ["**/*.md"], "exclude": ["target/**"]},
            "preset": "prose"
        })),
    )
    .await;
    let url_source = request(
        app.clone(),
        Method::POST,
        "/v1/stores/docs/sources",
        Some(json!({
            "kind": "url",
            "spec": {"url": "https://example.test/guide"},
            "preset": "prose",
            "refresh": "15m"
        })),
    )
    .await;
    let listed = request(app, Method::GET, "/v1/stores/docs/sources", None).await;

    // Then: both source shapes survive persistence and rendering.
    assert_eq!(path_source.status(), StatusCode::CREATED);
    assert_eq!(url_source.status(), StatusCode::CREATED);
    assert_eq!(listed.status(), StatusCode::OK);
    let body = json_body(listed.into_body()).await;
    let items = body["items"].as_array().expect("source items array");
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|item| item["spec"]["root"] == "/tmp/docs"));
    assert!(items
        .iter()
        .any(|item| item["spec"]["url"] == "https://example.test/guide"));
}

#[tokio::test]
async fn source_routes_reject_invalid_kind_and_refresh_on_path() {
    // Given: a runtime store exists.
    let (_dir, app) = make_app().await;
    create_store(app.clone(), "docs").await;

    // When: callers submit an unknown source kind and an invalid path refresh.
    let invalid_kind = request(
        app.clone(),
        Method::POST,
        "/v1/stores/docs/sources",
        Some(json!({ "kind": "rss", "spec": {"url": "https://example.test/feed"} })),
    )
    .await;
    let path_refresh = request(
        app,
        Method::POST,
        "/v1/stores/docs/sources",
        Some(json!({
            "kind": "path",
            "spec": {"root": "/tmp/docs"},
            "refresh": "10m"
        })),
    )
    .await;

    // Then: the boundary keeps both invalid requests out of state.
    assert_eq!(invalid_kind.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(invalid_kind.into_body()).await["code"],
        "invalid_request"
    );
    assert_eq!(path_refresh.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(path_refresh.into_body()).await["code"],
        "invalid_request"
    );
}
