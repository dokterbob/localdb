mod common;

use axum::http::{Method, StatusCode};
use serde_json::json;

use common::{create_store, json_body, make_app, request};

#[tokio::test]
async fn config_status_and_not_found_routes_expose_stable_shapes() {
    // Given: a fresh daemon router with one store.
    let (_dir, app) = make_app().await;
    create_store(app.clone(), "docs").await;

    // When: read-only operational endpoints and missing resources are requested.
    let config = request(app.clone(), Method::GET, "/v1/config", None).await;
    let status = request(app.clone(), Method::GET, "/v1/status", None).await;
    let missing_store = request(app.clone(), Method::GET, "/v1/stores/missing", None).await;
    let missing_source = request(app, Method::DELETE, "/v1/sources/missing", None).await;

    // Then: successful responses carry daemon/config shape and misses remain typed 404s.
    assert_eq!(config.status(), StatusCode::OK);
    let config_body = json_body(config.into_body()).await;
    assert_eq!(config_body["yaml_config"]["version"], 1);
    assert_eq!(
        config_body["effective_stores"]
            .as_array()
            .expect("effective stores array")
            .len(),
        1
    );

    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(json_body(status.into_body()).await["daemon"], true);
    assert_eq!(missing_store.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(missing_store.into_body()).await["code"],
        "store_not_found"
    );
    assert_eq!(missing_source.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(missing_source.into_body()).await["code"],
        "source_not_found"
    );
}

#[tokio::test]
async fn search_routes_validate_query_and_store_filter_before_returning_empty_results() {
    // Given: a fake-embedder daemon router with no indexed stores.
    let (_dir, app) = make_app().await;

    // When: search receives an empty query, an unknown store filter, and a valid no-store query.
    let empty_query = request(
        app.clone(),
        Method::POST,
        "/v1/search",
        Some(json!({ "query": "" })),
    )
    .await;
    let unknown_store = request(
        app.clone(),
        Method::POST,
        "/v1/search",
        Some(json!({ "query": "rust", "store_filter": ["missing"] })),
    )
    .await;
    let no_stores = request(
        app,
        Method::POST,
        "/v1/search",
        Some(json!({ "query": "rust", "limit": 3 })),
    )
    .await;

    // Then: invalid inputs are typed errors and an empty corpus is a successful empty result.
    assert_eq!(empty_query.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(empty_query.into_body()).await["code"],
        "invalid_request"
    );
    assert_eq!(unknown_store.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(unknown_store.into_body()).await["code"],
        "store_not_found"
    );
    assert_eq!(no_stores.status(), StatusCode::OK);
    let body = json_body(no_stores.into_body()).await;
    assert_eq!(
        body["citations"].as_array().expect("citations array").len(),
        0
    );
    assert_eq!(body["total_candidates"], 0);
    assert!(body["next_cursor"].is_null());
}

#[tokio::test]
async fn job_routes_create_source_scoped_jobs_and_report_missing_jobs() {
    // Given: a store with a source that can scope an indexing job.
    let (_dir, app) = make_app().await;
    create_store(app.clone(), "docs").await;
    let source = request(
        app.clone(),
        Method::POST,
        "/v1/stores/docs/sources",
        Some(json!({ "kind": "path", "spec": {"root": "/tmp/docs"} })),
    )
    .await;
    let source_id = json_body(source.into_body()).await["id"]
        .as_str()
        .expect("source id")
        .to_string();

    // When: a source-scoped job is created and an absent job is requested.
    let created_job = request(
        app.clone(),
        Method::POST,
        "/v1/jobs",
        Some(json!({ "store_name": "docs", "source_id": source_id })),
    )
    .await;
    let missing_job = request(app, Method::GET, "/v1/jobs/missing-job", None).await;

    // Then: job creation is accepted and missing jobs remain typed 404s.
    assert_eq!(created_job.status(), StatusCode::ACCEPTED);
    let job = json_body(created_job.into_body()).await;
    assert_eq!(job["store_id"], "docs");
    assert_eq!(job["scope"]["type"], "source");
    assert_eq!(job["scope"]["source_id"], source_id);
    assert_eq!(missing_job.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(missing_job.into_body()).await["code"],
        "job_not_found"
    );
}
