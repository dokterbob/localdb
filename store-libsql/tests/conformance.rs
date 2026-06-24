use store_libsql::LibsqlStore;
use localdb_core::VectorEncoding;
use localdb_core::store::conformance;

const DIM: usize = 2; // conformance tests use 2-dim embeddings

async fn fresh_store() -> (tempfile::TempDir, LibsqlStore) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    let store = LibsqlStore::open(&path, DIM, VectorEncoding::Float32)
        .await
        .unwrap();
    (tmp, store) // keep TempDir alive so the DB isn't deleted
}

#[tokio::test]
async fn conformance_upsert_and_stats() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_upsert_and_stats(&store).await;
}

#[tokio::test]
async fn conformance_upsert_replaces_existing() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_upsert_replaces_existing(&store).await;
}

#[tokio::test]
async fn conformance_delete_by_document() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_delete_by_document(&store).await;
}

#[tokio::test]
async fn conformance_delete_nonexistent_document() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_delete_nonexistent_document(&store).await;
}

#[tokio::test]
async fn conformance_dense_search_round_trip() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_dense_search_round_trip(&store).await;
}

#[tokio::test]
async fn conformance_bm25_search_round_trip() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_bm25_search_round_trip(&store).await;
}

#[tokio::test]
async fn conformance_metadata_filter_mime() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_metadata_filter_mime(&store).await;
}

#[tokio::test]
async fn conformance_metadata_filter_uri_prefix() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_metadata_filter_uri_prefix(&store).await;
}

#[tokio::test]
async fn conformance_get_chunk() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_get_chunk(&store).await;
}

#[tokio::test]
async fn conformance_get_chunks_for_document() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_get_chunks_for_document(&store).await;
}

#[tokio::test]
async fn conformance_delete_by_store() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_delete_by_store(&store).await;
}

#[tokio::test]
async fn conformance_dense_search_limit() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_dense_search_limit(&store).await;
}

#[tokio::test]
async fn conformance_bm25_search_limit() {
    let (_tmp, store) = fresh_store().await;
    conformance::test_bm25_search_limit(&store).await;
}
