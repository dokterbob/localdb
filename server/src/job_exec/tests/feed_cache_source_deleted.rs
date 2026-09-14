//! `run_job`'s feed-cache persistence hop must never resurrect a source that
//! was deleted while the job was running.
//!
//! `run_job` snapshots the store's `SourceRow`s once, at the top, and
//! persists cache state derived from that snapshot at the end. A
//! `source delete` landing in between leaves the job holding a row for a
//! source that no longer exists — and an `INSERT ... ON CONFLICT DO UPDATE`
//! would put it straight back, silently undoing a deletion the scheduler has
//! already acted on. `update_source_feed_cache` is update-only for exactly
//! this reason.
//!
//! The delete is triggered from inside `list_sources` rather than raced
//! against a delayed HTTP response: the window this exercises is "after the
//! snapshot, before the persistence hop", and firing it off the snapshot
//! call itself puts it there deterministically, with no sleeps to tune.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::ingestion::DeletionPolicy;
use localdb_core::{
    DocumentInfo, Error, IndexJobScope, RetrievalStore, SourceRow, StoreBackend, StoreRow,
    TableSize,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::common::{fake_yaml, test_state};
use crate::job_exec::{run_job, JobExecDeps};

/// Delegates everything to a real backend, but deletes `victim_id` from
/// under the caller the first time `list_sources` is called — after
/// returning the row, so the job proceeds on a snapshot of a source that no
/// longer exists.
struct DeletingBackend {
    inner: Arc<dyn StoreBackend>,
    victim_id: String,
    fired: AtomicBool,
}

#[async_trait]
impl StoreBackend for DeletingBackend {
    async fn open(_config: localdb_core::StoreBackendConfig) -> Result<Self, Error> {
        unimplemented!("never constructed via the trait's own open()")
    }

    async fn list_sources(&self, store_id: &str) -> Result<Vec<SourceRow>, Error> {
        let rows = self.inner.list_sources(store_id).await?;
        if !self.fired.swap(true, Ordering::SeqCst) {
            self.inner.delete_source(&self.victim_id).await?;
        }
        Ok(rows)
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
    async fn find_source_by_root_or_url(
        &self,
        value: &str,
        store_id: &str,
    ) -> Result<Option<SourceRow>, Error> {
        self.inner.find_source_by_root_or_url(value, store_id).await
    }
    async fn update_source_feed_cache(
        &self,
        id: &str,
        feed_etag: Option<&str>,
        feed_last_modified: Option<&str>,
        feed_inputs_digest: Option<&str>,
    ) -> Result<bool, Error> {
        self.inner
            .update_source_feed_cache(id, feed_etag, feed_last_modified, feed_inputs_digest)
            .await
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
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Vec<DocumentInfo>, Error> {
        self.inner
            .list_documents(store_id, source_id, limit, offset)
            .await
    }
    async fn count_documents(&self, store_id: &str, source_id: Option<&str>) -> Result<u64, Error> {
        self.inner.count_documents(store_id, source_id).await
    }
    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        self.inner.retrieval_store(store_id).await
    }
    async fn largest_tables(&self, limit: usize) -> Result<Vec<TableSize>, Error> {
        self.inner.largest_tables(limit).await
    }
}

/// An empty channel: the feed document is fetched and answers 200 — enough
/// to produce validators worth persisting — while enumerating no entries, so
/// the run writes no resources against the source row being deleted out from
/// under it. That keeps this test about the persistence hop alone.
fn empty_rss() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Feed</title><link>https://feed.example.com/</link><description>d</description></channel></rss>"#.to_string()
}

#[tokio::test]
async fn a_source_deleted_during_the_run_is_not_resurrected_by_the_cache_write() {
    let server = MockServer::start().await;
    let feed_url = format!("{}/feed.xml", server.uri());
    Mock::given(method("GET"))
        .and(path("/feed.xml"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_string(empty_rss()),
        )
        .mount(&server)
        .await;

    let (dir, state) = test_state().await;
    state.add_store("notes", "private").await.unwrap();
    let source = state
        .add_source(
            "notes",
            "feed",
            serde_json::json!({ "url": feed_url, "fetch_full_content": false }),
            "prose",
            None,
        )
        .await
        .unwrap();

    let store = state
        .backend()
        .get_store_by_name("notes")
        .await
        .unwrap()
        .unwrap();
    let yaml = fake_yaml();
    let wrapper: Arc<dyn StoreBackend> = Arc::new(DeletingBackend {
        inner: state.backend_arc(),
        victim_id: source.id.clone(),
        fired: AtomicBool::new(false),
    });

    let (_stats, _embedder) = run_job(
        &store,
        IndexJobScope::Store,
        DeletionPolicy::Retain,
        false,
        JobExecDeps {
            backend: wrapper.as_ref(),
            yaml: &yaml,
            models_dir: state.models_dir(),
            embedder: None,
            fetchers: None,
            progress: None,
            on_source_error: None,
        },
    )
    .await
    .expect("a source vanishing mid-run is a race to absorb, not a job failure");

    assert!(
        state
            .backend()
            .get_source(&source.id)
            .await
            .unwrap()
            .is_none(),
        "the deleted source must stay deleted: an upsert here would re-insert \
         a row the scheduler has already unregistered"
    );

    drop(dir);
}
