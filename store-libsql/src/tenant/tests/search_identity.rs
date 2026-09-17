//! Search identities preserve stored precision across both retrieval legs.

use localdb_core::{SearchResult, StoreBackend, StoreBackendConfig, VectorEncoding};
use tempfile::tempdir;

use super::common::{add_store_and_source, chunk_record};
use crate::SqliteBackend;

async fn search_vectors(
    encoding: VectorEncoding,
    vectors: &[Vec<f32>],
) -> (Vec<SearchResult>, Vec<SearchResult>) {
    let dir = tempdir().unwrap();
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        dir.path().join("identity.db"),
        4,
        encoding,
    ))
    .await
    .unwrap();
    add_store_and_source(&backend, "store-1", "src-1", "/docs").await;
    let handle = backend.retrieval_store("store-1").await.unwrap();
    let records = vectors
        .iter()
        .enumerate()
        .rev()
        .map(|(i, vector)| {
            let mut record = chunk_record("2026-07-01T00:00:00Z", None);
            record.id = format!("chunk-{i}");
            record.seq_in_block = i as u32;
            record.embedding = vector.clone();
            record
        })
        .collect();
    handle.upsert_chunks(records).await.unwrap();
    let dense = handle
        .dense_search(&vectors[0], vectors.len(), &[])
        .await
        .unwrap();
    let bm25 = handle
        .bm25_search("chunk", vectors.len(), &[])
        .await
        .unwrap();
    assert_eq!(dense.len(), vectors.len());
    assert_eq!(bm25.len(), vectors.len());
    let limited = handle.bm25_search("chunk", 2, &[]).await.unwrap();
    assert_eq!(
        limited
            .iter()
            .map(|hit| hit.chunk.id.as_str())
            .collect::<Vec<_>>(),
        ["chunk-0", "chunk-1"],
        "ties must be ordered before applying the SQL limit"
    );
    if encoding == VectorEncoding::Binary {
        let limited = handle.dense_search(&vectors[0], 2, &[]).await.unwrap();
        assert_eq!(
            limited
                .iter()
                .map(|hit| hit.chunk.id.as_str())
                .collect::<Vec<_>>(),
            ["chunk-0", "chunk-1"]
        );
    }
    for hit in &dense {
        let identity = hit.embedding_identity.as_ref().expect("dense identity");
        assert_eq!(identity.format, "libsql-blob-v1");
        assert_eq!(identity.encoding, encoding);
        assert_eq!(identity.dimensions, 4);
        assert!(!identity.bytes.is_empty());
        let bm25_hit = bm25.iter().find(|b| b.chunk.id == hit.chunk.id).unwrap();
        assert_eq!(bm25_hit.embedding_identity.as_ref(), Some(identity));
        let mut rows = backend
            .conn
            .reader()
            .query(
                "SELECT embedding FROM chunks WHERE id = ?",
                [hit.chunk.id.clone()],
            )
            .await
            .unwrap();
        let stored: Vec<u8> = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(identity.bytes.as_ref(), stored.as_slice());
    }
    (dense, bm25)
}

fn identity(hits: &[SearchResult], id: &str) -> localdb_core::StoredEmbeddingIdentity {
    hits.iter()
        .find(|hit| hit.chunk.id == id)
        .unwrap()
        .embedding_identity
        .clone()
        .unwrap()
}

#[tokio::test]
async fn binary_identity_compares_stored_signs_not_original_magnitudes() {
    let (dense, bm25) = search_vectors(
        VectorEncoding::Binary,
        &[
            vec![1.0, -1.0, 1.0, -1.0],
            vec![9.0, -0.5, 2.0, -7.0],
            vec![1.0, 1.0, 1.0, -1.0],
        ],
    )
    .await;
    for hits in [&dense, &bm25] {
        assert_eq!(identity(hits, "chunk-0"), identity(hits, "chunk-1"));
        assert_ne!(identity(hits, "chunk-0"), identity(hits, "chunk-2"));
    }
    assert_eq!(dense[0].chunk.id, "chunk-0");
    assert_eq!(dense[1].chunk.id, "chunk-1");
    assert_eq!(
        bm25.iter()
            .map(|hit| hit.chunk.id.as_str())
            .collect::<Vec<_>>(),
        ["chunk-0", "chunk-1", "chunk-2"]
    );
}

#[tokio::test]
async fn float32_identity_preserves_export_rounding_and_signed_zero_distinctions() {
    let (dense, bm25) = search_vectors(
        VectorEncoding::Float32,
        &[
            vec![f32::from_bits(1.0_f32.to_bits() + 1), 1.0, 0.0, 0.0],
            vec![f32::from_bits(1.0_f32.to_bits() + 2), 1.0, 0.0, 0.0],
            vec![1.0, 1.0, 0.0, 0.0],
            vec![1.0, 1.0, -0.0, 0.0],
            vec![1.0, 1.0, 0.0, 0.0],
        ],
    )
    .await;
    for hits in [&dense, &bm25] {
        let first = hits.iter().find(|hit| hit.chunk.id == "chunk-0").unwrap();
        let second = hits.iter().find(|hit| hit.chunk.id == "chunk-1").unwrap();
        assert_eq!(
            first.chunk.embedding, second.chunk.embedding,
            "the export-rounding collision must actually occur in this regression fixture"
        );
        assert_ne!(identity(hits, "chunk-0"), identity(hits, "chunk-1"));
        assert_ne!(identity(hits, "chunk-2"), identity(hits, "chunk-3"));
        assert_eq!(identity(hits, "chunk-2"), identity(hits, "chunk-4"));
    }
}
