use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use std::collections::HashSet;
use std::sync::Arc;

use super::common::{
    json_body, make_app, make_state_with_fake_config, seed_many_chunks, seed_store_a_chunk,
    SeedChunkInput,
};

fn citation_ids(body: &serde_json::Value) -> Vec<String> {
    body["citations"]
        .as_array()
        .expect("citations should be an array")
        .iter()
        .map(|c| c["chunk_id"].as_str().unwrap().to_string())
        .collect()
}

async fn search_page(
    app: &axum::Router,
    query: &str,
    limit: usize,
    cursor: Option<&str>,
) -> serde_json::Value {
    let mut payload = json!({"query": query, "limit": limit});
    if let Some(cursor) = cursor {
        payload["cursor"] = json!(cursor);
    }
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    json_body(resp.into_body()).await
}

#[tokio::test]
async fn search_empty_query_returns_400() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": ""}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_with_nonexistent_store_filter_returns_404() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"query": "hello", "store_filter": ["no-such-store"]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_returns_citations_shape() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": "hello world"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert!(body["citations"].is_array());
    assert!(body["total_candidates"].is_number());
}

#[tokio::test]
async fn search_returns_citations_after_indexing() {
    let (_dir, state) = make_state_with_fake_config().await;
    seed_store_a_chunk(
        &state,
        SeedChunkInput {
            chunk_id: "chunk-1",
            doc_id: "doc-1",
            text: "hello world rust programming",
            uri: "file:///hello.md",
            metadata: localdb_core::metadata::Metadata::default(),
        },
    )
    .await;

    let app = crate::daemon::build_router(
        state,
        vec![],
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(json!({"query": "hello world"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let citations = body["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "got: {:?}", body);
    assert_eq!(citations[0]["uri"], "file:///hello.md");
}

#[tokio::test]
async fn search_with_nonexistent_store_filter_returns_empty() {
    let (_dir, state) = make_state_with_fake_config().await;
    state.add_store("my-store", "private").await.unwrap();
    seed_store_a_chunk(
        &state,
        SeedChunkInput {
            chunk_id: "chunk-ff",
            doc_id: "doc-ff",
            text: "hello world",
            uri: "file:///foreign.md",
            metadata: localdb_core::metadata::Metadata::default(),
        },
    )
    .await;

    let app = crate::daemon::build_router(
        state,
        vec![],
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"query": "hello world", "store_filter": ["my-store"]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    let citations = body["citations"].as_array().unwrap();
    assert!(citations.is_empty(), "got: {:?}", body);
}

#[tokio::test]
async fn search_pagination_page_two_is_disjoint_from_page_one() {
    let (_dir, state) = make_state_with_fake_config().await;
    seed_many_chunks(&state, 30).await;

    let app = crate::daemon::build_router(
        state,
        vec![],
        Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    let page1 = search_page(&app, "pagination test rust programming", 5, None).await;
    let page1_ids = citation_ids(&page1);
    assert_eq!(page1_ids.len(), 5, "page 1 got: {:?}", page1);

    let cursor = page1["next_cursor"]
        .as_str()
        .expect("expected a next_cursor after page 1 of 30 results");

    let page2 = search_page(&app, "pagination test rust programming", 5, Some(cursor)).await;
    let page2_ids = citation_ids(&page2);
    assert_eq!(page2_ids.len(), 5, "page 2 got: {:?}", page2);

    let page1_set: HashSet<_> = page1_ids.iter().collect();
    let page2_set: HashSet<_> = page2_ids.iter().collect();
    assert!(
        page1_set.is_disjoint(&page2_set),
        "page 1 and page 2 should be disjoint sets of chunk ids; \
         page1={page1_ids:?} page2={page2_ids:?}"
    );
}

#[tokio::test]
async fn search_limit_is_silently_clamped_to_the_max_instead_of_erroring() {
    // Given: a corpus of candidates for the query to match.
    let (_dir, state) = make_state_with_fake_config().await;
    seed_many_chunks(&state, 210).await;

    let app = crate::daemon::build_router(
        state,
        vec![],
        Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    // When: the request asks for a limit far above the clamp (issue #187
    // review, finding G3 — mirrors the MCP `search` tool's silent-clamp
    // idiom, `mcp/src/tools.rs::resolve_search_limit`: too-large is capped,
    // not rejected). Previously this fed straight into an unchecked
    // `offset + limit` used as `top_n`.
    let page = search_page(&app, "pagination test rust programming", 100_000, None).await;

    // Then: the request succeeds (no `invalid_request`, no panic) and the
    // page never exceeds the clamp. This corpus's candidate pool is in
    // practice bounded well below `SEARCH_MAX_LIMIT` by
    // `core::search::DEFAULT_LEG_K` (each leg — dense, BM25 — returns at
    // most `leg_k` = 50 candidates, and `/v1/search` does not expose
    // `leg_k` to the caller), so this asserts the clamp never lets the
    // response exceed the max rather than asserting it is hit exactly —
    // the exact-max case is covered at the unit level by
    // `localdb_core::search::tests::clamp_search_limit_*`.
    let ids = citation_ids(&page);
    assert!(
        ids.len() <= localdb_core::SEARCH_MAX_LIMIT,
        "citations must never exceed the {}-item clamp, got {}: {:?}",
        localdb_core::SEARCH_MAX_LIMIT,
        ids.len(),
        page
    );
    assert!(!ids.is_empty(), "expected at least one match: {:?}", page);
}

#[tokio::test]
async fn search_pagination_walk_to_exhaustion_covers_all_results_without_duplicates() {
    let (_dir, state) = make_state_with_fake_config().await;
    let seeded_ids = seed_many_chunks(&state, 33).await;
    let seeded_set: HashSet<_> = seeded_ids.into_iter().collect();

    let app = crate::daemon::build_router(
        state,
        vec![],
        Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    let mut all_ids: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = search_page(
            &app,
            "pagination test rust programming",
            7,
            cursor.as_deref(),
        )
        .await;
        let ids = citation_ids(&page);
        assert!(
            !ids.is_empty() || pages == 0,
            "page {pages} unexpectedly empty: {:?}",
            page
        );
        all_ids.extend(ids);
        pages += 1;
        assert!(pages <= 10, "pagination did not terminate: {:?}", page);

        cursor = page["next_cursor"].as_str().map(|s| s.to_string());
        if cursor.is_none() {
            break;
        }
    }

    let all_set: HashSet<_> = all_ids.iter().cloned().collect();
    assert_eq!(
        all_ids.len(),
        all_set.len(),
        "walking the cursor to exhaustion produced duplicate chunk ids: {:?}",
        all_ids
    );
    assert_eq!(
        all_set, seeded_set,
        "walking the cursor to exhaustion should yield exactly the seeded chunk ids"
    );
}
