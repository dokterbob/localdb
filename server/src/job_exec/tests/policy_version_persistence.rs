//! `run_job`: a failure persisting the refreshed `policy_version` is
//! warn-and-continue, never fatal to the job.

use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::ingestion::DeletionPolicy;
use localdb_core::{
    DocumentInfo, Error, IndexJobScope, RetrievalStore, SourceRow, StoreBackend, StoreRow,
    TableSize,
};
use serde_json::json;

use super::common::{fake_yaml, test_state};
use crate::job_exec::{run_job, JobExecDeps};

/// A `StoreBackend` wrapper that runs every call against a real inner
/// backend except `upsert_store`, which always fails — the only way to
/// exercise `run_job`'s "persist the refreshed policy_version" failure
/// branch (job_exec.rs's `tracing::warn!` on `backend.upsert_store`
/// error) without a flaky, platform-dependent trick like corrupting the
/// SQLite file on disk.
struct FailingUpsertBackend {
    inner: Arc<dyn StoreBackend>,
}

#[async_trait]
impl StoreBackend for FailingUpsertBackend {
    async fn open(_config: localdb_core::StoreBackendConfig) -> Result<Self, Error> {
        unimplemented!("never constructed via the trait's own open()")
    }

    async fn upsert_store(&self, _store: &StoreRow) -> Result<(), Error> {
        Err(Error::Internal {
            message: "simulated upsert_store failure".to_string(),
            correlation_id: "test_failing_upsert_backend".to_string(),
        })
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
        self.inner.list_sources(store_id).await
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

#[tokio::test]
async fn run_job_continues_when_persisting_the_refreshed_policy_version_fails() {
    let (dir, state) = test_state().await;
    state.add_store("docs", "private").await.unwrap();
    // An existing, empty directory: a valid path source that indexes
    // zero documents without touching the network — this test's point
    // is the policy-version-persistence failure, not ingestion itself.
    let root = dir.path().join("empty-root");
    std::fs::create_dir(&root).unwrap();
    state
        .add_source(
            "docs",
            "path",
            json!({ "root": root.to_str().unwrap() }),
            "prose",
            None,
        )
        .await
        .unwrap();

    let mut store = state
        .backend()
        .get_store_by_name("docs")
        .await
        .unwrap()
        .unwrap();
    // Force a policy-version mismatch so `run_job` attempts the
    // refresh-and-persist path at all.
    store.policy_version = "stale-version".to_string();

    let yaml = fake_yaml();
    let wrapper: Arc<dyn StoreBackend> = Arc::new(FailingUpsertBackend {
        inner: state.backend_arc(),
    });

    let (stats, _embedder) = run_job(
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
    .unwrap();

    assert_eq!(
        stats.sources_count, 1,
        "the job must still process the store's source despite the policy_version \
         persistence failure — that failure is logged and swallowed, never fatal"
    );
}
