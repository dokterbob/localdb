use super::*;
use crate::ingestion::DocumentRecord;
use crate::store::{ResourceRecord, StoreStats};
use std::sync::atomic::{AtomicUsize, Ordering};

struct BoundedStore {
    fake: FakeStore,
    dense_calls: AtomicUsize,
    bm25_calls: AtomicUsize,
    expected_k: usize,
}

#[async_trait::async_trait]
impl RetrievalStore for BoundedStore {
    async fn upsert_chunks(&self, _: Vec<ChunkRecord>) -> Result<usize, Error> {
        panic!("unexpected write")
    }
    async fn delete_by_resource(&self, _: &str) -> Result<usize, Error> {
        panic!("unexpected write")
    }
    async fn delete_by_store(&self, _: &str) -> Result<usize, Error> {
        panic!("unexpected write")
    }
    async fn dense_search(
        &self,
        vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        assert_eq!(limit, self.expected_k);
        self.dense_calls.fetch_add(1, Ordering::SeqCst);
        self.fake.dense_search(vector, limit, filters).await
    }
    async fn bm25_search(
        &self,
        query: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        assert_eq!(limit, self.expected_k);
        self.bm25_calls.fetch_add(1, Ordering::SeqCst);
        self.fake.bm25_search(query, limit, filters).await
    }
    async fn stats(&self) -> Result<StoreStats, Error> {
        panic!("unexpected stats read")
    }
    async fn get_chunk(&self, _: &str) -> Result<Option<ChunkRecord>, Error> {
        panic!("unexpected chunk read")
    }
    async fn get_chunks_for_resource(&self, _: &str) -> Result<Vec<ChunkRecord>, Error> {
        panic!("unexpected full document read")
    }
    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        panic!("unexpected collection read")
    }
    async fn update_resource_metadata(
        &self,
        _: &str,
        _: &str,
        _: &ResourceRecord,
    ) -> Result<(), Error> {
        panic!("unexpected write")
    }
    async fn get_resource_record(&self, _: &str, _: &str) -> Result<Option<ResourceRecord>, Error> {
        panic!("unexpected metadata read")
    }
    async fn get_blocks_for_resource(&self, _: &str) -> Result<Vec<crate::block::Block>, Error> {
        panic!("unexpected block read")
    }
}

#[tokio::test]
async fn collapsed_results_do_not_refill_or_read_documents_and_filter_every_member() {
    for leg_k in [None, Some(2)] {
        let store = Arc::new(BoundedStore {
            fake: FakeStore::new(),
            dense_calls: AtomicUsize::new(0),
            bm25_calls: AtomicUsize::new(0),
            expected_k: leg_k.unwrap_or(DEFAULT_LEG_K),
        });
        let records = (0..160)
            .map(|i| {
                let mut c = make_chunk(
                    &format!("{i:03}"),
                    "doc",
                    "store",
                    "same query passage",
                    vec![],
                    "uri",
                    vec![1.0, 0.0],
                );
                c.source_id = if i % 2 == 0 { "selected" } else { "excluded" }.into();
                c
            })
            .collect();
        store.fake.upsert_chunks(records).await.unwrap();
        let handles = [StoreHandle {
            id: "store".into(),
            name: "store".into(),
            store: store.clone(),
        }];
        let response = SearchOrchestrator::query(
            &handles,
            &FakeEmbedder::new(2),
            &QueryRequest {
                query: "query".into(),
                dedup: SearchDedup::default(),
                leg_k,
                top_n: Some(10),
                filters: vec![MetadataFilter::SourceId("selected".into())],
            },
        )
        .await
        .unwrap();
        assert_eq!(response.total_results, 1);
        assert_eq!(response.total_candidates, store.expected_k.min(80));
        assert_eq!(response.citations.len(), 1);
        assert_eq!(
            response.citations[0].duplicates.len(),
            response.total_candidates - 1
        );
        assert_eq!(
            response.citations[0].chunk_id.parse::<usize>().unwrap() % 2,
            0
        );
        assert!(response.citations[0].duplicates.iter().all(|d| d
            .citation
            .chunk_id
            .parse::<usize>()
            .unwrap()
            % 2
            == 0));
        assert_eq!(store.dense_calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.bm25_calls.load(Ordering::SeqCst), 1);
    }
}
