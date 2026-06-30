mod common;

use axum::http::{Method, StatusCode};
use serde_json::json;

use common::{create_store, json_body, make_app, request};

#[tokio::test]
async fn store_routes_page_and_update_runtime_stores() {
    // Given: a fresh daemon router with three runtime-owned stores.
    let (_dir, app) = make_app().await;
    for name in ["alpha", "beta", "gamma"] {
        create_store(app.clone(), name).await;
    }

    // When: the first stores page is requested and one store is patched.
    let first_page = request(app.clone(), Method::GET, "/v1/stores?limit=2", None).await;
    let patch = request(
        app.clone(),
        Method::PATCH,
        "/v1/stores/beta",
        Some(json!({ "visibility": "shared" })),
    )
    .await;

    // Then: pagination and the updated visibility are observable through HTTP.
    assert_eq!(first_page.status(), StatusCode::OK);
    let first_page_body = json_body(first_page.into_body()).await;
    assert_eq!(
        first_page_body["items"]
            .as_array()
            .expect("items array")
            .len(),
        2
    );
    assert_eq!(first_page_body["next_cursor"], "2");

    assert_eq!(patch.status(), StatusCode::OK);
    let patched = json_body(patch.into_body()).await;
    assert_eq!(patched["name"], "beta");
    assert_eq!(patched["visibility"], "shared");
}

#[tokio::test]
async fn store_routes_report_invalid_inputs_with_stable_errors() {
    // Given: a fresh daemon router.
    let (_dir, app) = make_app().await;

    // When: invalid store requests cross the HTTP boundary.
    let empty_name = request(
        app.clone(),
        Method::POST,
        "/v1/stores",
        Some(json!({ "name": "" })),
    )
    .await;
    let invalid_visibility = request(
        app.clone(),
        Method::POST,
        "/v1/stores",
        Some(json!({ "name": "notes", "visibility": "public" })),
    )
    .await;
    let invalid_cursor = request(app, Method::GET, "/v1/stores?cursor=not-a-number", None).await;

    // Then: each request returns the documented client-error status and code.
    assert_eq!(empty_name.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(empty_name.into_body()).await["code"],
        "invalid_request"
    );
    assert_eq!(invalid_visibility.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(invalid_visibility.into_body()).await["code"],
        "invalid_request"
    );
    assert_eq!(invalid_cursor.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(invalid_cursor.into_body()).await["code"],
        "invalid_request"
    );
}

#[tokio::test]
async fn source_delete_and_store_delete_return_no_content() {
    // Given: a store with one path source.
    let (_dir, app) = make_app().await;
    create_store(app.clone(), "docs").await;
    let created = request(
        app.clone(),
        Method::POST,
        "/v1/stores/docs/sources",
        Some(json!({ "kind": "path", "spec": {"root": "/tmp/docs"} })),
    )
    .await;
    let source_id = json_body(created.into_body()).await["id"]
        .as_str()
        .expect("created source id")
        .to_string();

    // When: the source and then the store are deleted.
    let source_deleted = request(
        app.clone(),
        Method::DELETE,
        &format!("/v1/sources/{source_id}"),
        None,
    )
    .await;
    let store_deleted = request(app, Method::DELETE, "/v1/stores/docs", None).await;

    // Then: both deletion endpoints use the no-content success contract.
    assert_eq!(source_deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(store_deleted.status(), StatusCode::NO_CONTENT);
}
