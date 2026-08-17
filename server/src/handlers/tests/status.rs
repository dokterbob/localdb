use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

use localdb_core::{
    DocumentInfo, Error, RetrievalStore, SourceRow, StoreBackend, StoreRow, TableSize,
};

use super::common::{build_router, json_body, make_app};
use crate::state::AppState;

#[tokio::test]
async fn get_status_returns_daemon_true() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["daemon"], true);
}

// ---------------------------------------------------------------------------
// A `StoreBackend` wrapper that lets a test fail `list_sources` for one
// specific store id and/or record every store id `list_sources`/
// `retrieval_store` is called with — everything else forwards to a real
// inner backend untouched. Mirrors `job_exec.rs`'s `FailingUpsertBackend`
// (issue #187 review, finding F7's fix).
// ---------------------------------------------------------------------------
struct TrackingBackend {
    inner: Arc<dyn StoreBackend>,
    /// If `Some(id)`, `list_sources(id)` returns an error instead of
    /// forwarding — simulates one corrupt/mid-migration store's source
    /// listing failing.
    fail_sources_for: Option<String>,
    /// Every store id passed to `list_sources` or `retrieval_store`, in call
    /// order — lets a test assert which stores the handler actually touched.
    touched: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl StoreBackend for TrackingBackend {
    async fn open(_config: localdb_core::StoreBackendConfig) -> Result<Self, Error> {
        unimplemented!("never constructed via the trait's own open()")
    }

    async fn upsert_store(&self, store: &StoreRow) -> Result<(), Error> {
        self.inner.upsert_store(store).await
    }
    async fn delete_store(&self, id: &str) -> Result<bool, Error> {
        self.inner.delete_store(id).await
    }
    async fn get_store(&self, id: &str) -> Result<Option<StoreRow>, Error> {
        self.inner.get_store(id).await
    }
    async fn get_store_by_name(&self, name: &str) -> Result<Option<StoreRow>, Error> {
        self.inner.get_store_by_name(name).await
    }
    async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
        self.inner.list_stores().await
    }
    async fn upsert_source(&self, source: &SourceRow) -> Result<(), Error> {
        self.inner.upsert_source(source).await
    }
    async fn delete_source(&self, id: &str) -> Result<bool, Error> {
        self.inner.delete_source(id).await
    }
    async fn get_source(&self, id: &str) -> Result<Option<SourceRow>, Error> {
        self.inner.get_source(id).await
    }
    async fn list_sources(&self, store_id: &str) -> Result<Vec<SourceRow>, Error> {
        self.touched.lock().unwrap().push(store_id.to_string());
        if self.fail_sources_for.as_deref() == Some(store_id) {
            return Err(Error::Internal {
                message: "simulated list_sources failure".to_string(),
                correlation_id: "test_tracking_backend".to_string(),
            });
        }
        self.inner.list_sources(store_id).await
    }
    async fn find_source_by_root_or_url(
        &self,
        value: &str,
        store_id: &str,
    ) -> Result<Option<SourceRow>, Error> {
        self.inner.find_source_by_root_or_url(value, store_id).await
    }
    async fn find_document(
        &self,
        doc_id: &str,
        store_id: Option<&str>,
    ) -> Result<Option<DocumentInfo>, Error> {
        self.inner.find_document(doc_id, store_id).await
    }
    async fn list_documents(
        &self,
        store_id: &str,
        source_id: Option<&str>,
    ) -> Result<Vec<DocumentInfo>, Error> {
        self.inner.list_documents(store_id, source_id).await
    }
    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        self.touched.lock().unwrap().push(store_id.to_string());
        self.inner.retrieval_store(store_id).await
    }
    async fn largest_tables(&self, limit: usize) -> Result<Vec<TableSize>, Error> {
        self.inner.largest_tables(limit).await
    }
}

async fn add_store_with_source(state: &AppState, name: &str) {
    state.add_store(name, "private").await.unwrap();
    state
        .add_source(name, "path", json!({"root": "/tmp"}), "prose", None)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// F7 (1): one broken store's `list_sources` failure must not fail the whole
// response — mirrors the adjacent stats call's existing best-effort handling.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn get_status_degrades_source_listing_best_effort_for_one_broken_store() {
    let (dir, real_state) = super::common::make_state_with_fake_config().await;
    add_store_with_source(&real_state, "healthy").await;
    add_store_with_source(&real_state, "broken").await;
    let broken_id = real_state
        .backend()
        .get_store_by_name("broken")
        .await
        .unwrap()
        .unwrap()
        .id;

    let touched = Arc::new(Mutex::new(Vec::new()));
    let wrapped: Arc<dyn StoreBackend> = Arc::new(TrackingBackend {
        inner: real_state.backend_arc(),
        fail_sources_for: Some(broken_id),
        touched,
    });
    let yaml = real_state.yaml_config().await;
    let queue = crate::job_queue::JobQueue::new();
    let wrapped_state = AppState::from_backend(
        yaml,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        wrapped,
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
    );
    let app = build_router(wrapped_state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "one store's list_sources failure must not fail the whole /v1/status response"
    );
    let body = json_body(resp.into_body()).await;

    let stores = body["stores"].as_array().unwrap();
    assert_eq!(
        stores.len(),
        2,
        "both stores must still appear, including the broken one: {body}"
    );
    assert!(
        stores.iter().any(|s| s["name"] == "healthy"),
        "healthy store missing: {body}"
    );
    assert!(
        stores.iter().any(|s| s["name"] == "broken"),
        "broken store missing: {body}"
    );

    // The broken store's source(s) must not be counted — only the healthy
    // store's single source contributes to the aggregate.
    assert_eq!(
        body["source_count"], 1,
        "broken store's source listing failure must not inflate/replace source_count: {body}"
    );
}

// ---------------------------------------------------------------------------
// F7 (2): `?store=` scoping — only touches/reports the requested subset.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn get_status_store_scoping_only_touches_and_reports_scoped_stores() {
    let (dir, real_state) = super::common::make_state_with_fake_config().await;
    add_store_with_source(&real_state, "a").await;
    add_store_with_source(&real_state, "b").await;
    add_store_with_source(&real_state, "c").await;
    let a_id = real_state
        .backend()
        .get_store_by_name("a")
        .await
        .unwrap()
        .unwrap()
        .id;
    let b_id = real_state
        .backend()
        .get_store_by_name("b")
        .await
        .unwrap()
        .unwrap()
        .id;
    let c_id = real_state
        .backend()
        .get_store_by_name("c")
        .await
        .unwrap()
        .unwrap()
        .id;

    let touched = Arc::new(Mutex::new(Vec::new()));
    let wrapped: Arc<dyn StoreBackend> = Arc::new(TrackingBackend {
        inner: real_state.backend_arc(),
        fail_sources_for: None,
        touched: touched.clone(),
    });
    let yaml = real_state.yaml_config().await;
    let queue = crate::job_queue::JobQueue::new();
    let wrapped_state = AppState::from_backend(
        yaml,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        wrapped,
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
    );
    let app = build_router(wrapped_state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/status?store=a&store=b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp.into_body()).await;

    let stores = body["stores"].as_array().unwrap();
    assert_eq!(
        stores.len(),
        2,
        "scoped response must contain exactly a and b: {body}"
    );
    let names: Vec<&str> = stores.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"a") && names.contains(&"b"));
    assert!(!names.contains(&"c"), "c is out of scope: {body}");

    assert_eq!(body["store_count"], 2);
    assert_eq!(
        body["source_count"], 2,
        "source_count must cover only the scoped stores' sources: {body}"
    );

    let touched = touched.lock().unwrap();
    assert!(touched.contains(&a_id), "a must have been queried");
    assert!(touched.contains(&b_id), "b must have been queried");
    assert!(
        !touched.contains(&c_id),
        "out-of-scope store c must never be queried (neither sources nor stats): {touched:?}"
    );
}

#[tokio::test]
async fn get_status_unknown_store_returns_404_store_not_found() {
    let (_dir, app) = make_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/status?store=does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = json_body(resp.into_body()).await;
    assert_eq!(body["code"], "store_not_found");
}

// ---------------------------------------------------------------------------
// G2 (issue #187 review round on PR #212): `get_status` must not require
// every store's `indexing_policy` to parse — it only needs raw
// name/id/visibility/backend off `StoreRow`. Seeds a store with a malformed
// `indexing_policy` directly through the real backend's `upsert_store` (no
// production test-only helper needed: `StoreRow.indexing_policy` is a plain
// unvalidated `String` column, so writing garbage into it and re-upserting
// is exactly what a corrupt/mid-migration row looks like on disk).
// ---------------------------------------------------------------------------
async fn corrupt_indexing_policy(state: &AppState, name: &str) {
    let mut row = state
        .backend()
        .get_store_by_name(name)
        .await
        .unwrap()
        .unwrap();
    row.indexing_policy = "not valid json".to_string();
    state.backend().upsert_store(&row).await.unwrap();
}

#[tokio::test]
async fn get_status_scoped_by_healthy_ignores_malformed_policy_on_other_store() {
    let (_dir, state) = super::common::make_state_with_fake_config().await;
    state.add_store("healthy", "private").await.unwrap();
    state.add_store("broken", "private").await.unwrap();
    corrupt_indexing_policy(&state, "broken").await;

    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/status?store=healthy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a malformed policy on an unrelated, out-of-scope store must not fail a scoped status request"
    );
    let body = json_body(resp.into_body()).await;
    let stores = body["stores"].as_array().unwrap();
    assert_eq!(
        stores.len(),
        1,
        "only the scoped store must be reported: {body}"
    );
    assert_eq!(stores[0]["name"], "healthy");
}

#[tokio::test]
async fn get_status_unscoped_degrades_best_effort_with_one_malformed_policy_store() {
    let (_dir, state) = super::common::make_state_with_fake_config().await;
    state.add_store("healthy", "private").await.unwrap();
    state.add_store("broken", "private").await.unwrap();
    corrupt_indexing_policy(&state, "broken").await;

    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an unscoped status request must degrade best-effort rather than 500 when one store's policy is malformed"
    );
    let body = json_body(resp.into_body()).await;
    let stores = body["stores"].as_array().unwrap();
    assert_eq!(stores.len(), 2, "both stores must be reported: {body}");
    let names: Vec<&str> = stores.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"healthy") && names.contains(&"broken"));
}
