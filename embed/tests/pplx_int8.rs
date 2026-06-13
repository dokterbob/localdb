//! Smoke test: pplx-embed-v1-0.6b via `PplxOnnxEmbedder`.
//!
//! Verifies end-to-end that the production embedder produces 1024-dim int8
//! embeddings that correctly separate two semantically distinct document–query
//! pairs.  Downloads ~2.4 GB of model files to the platform model cache on
//! first run; subsequent runs use the cached files and finish in seconds.
//!
//! # Running
//!
//! ```sh
//! HF_TOKEN=<your-token> cargo test -p embed --features local-onnx -- --ignored pplx_embed_v1_int8
//! ```
//!
//! `HF_TOKEN` is required on first run to download the gated model from
//! `perplexity-ai/pplx-embed-v1-0.6b`.  The token is not needed once the
//! files are cached under `<platform-cache>/localdb/models/pplx-embed-v1-0.6b/`.

#![cfg(feature = "local-onnx")]

use embed::PplxOnnxEmbedder;
use localdb_core::{DocumentChunks, Embedder};

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na * nb < 1e-9 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Validate pplx-embed-v1-0.6b int8 embeddings via the production `PplxOnnxEmbedder`.
///
/// **Slow — downloads ~2.4 GB on first run.**  Skip in CI (the default).
/// Run locally with:
///
/// ```sh
/// HF_TOKEN=<token> cargo test -p embed --features local-onnx -- --ignored pplx_embed_v1_int8
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: downloads ~2.4 GB of ONNX model files on first run; run with --ignored"]
async fn pplx_embed_v1_int8_two_documents_retrieval() {
    // Use the test-models cache dir so CI-adjacent runs share the same download.
    // Production runs use None (→ ~/Library/Caches/localdb/models/).
    let test_cache = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("localdb")
        .join("test-models");
    let embedder = PplxOnnxEmbedder::new(Some(test_cache), true)
        .expect("create PplxOnnxEmbedder (set HF_TOKEN if 401)");

    assert_eq!(embedder.embedding_dim(), 1024);
    assert_eq!(embedder.model_id(), "pplx-embed-v1-0.6b");

    let doc_systems =
        "Rust is a systems programming language that guarantees memory safety without a \
         garbage collector by using an ownership model enforced at compile time.";
    let doc_culinary =
        "Sauté diced onions in unsalted butter over medium heat until translucent, then \
         deglaze the pan with dry white wine to create a rich sauce base.";
    let query_systems = "memory safe programming without garbage collection";
    let query_culinary = "how to caramelise onions when cooking";

    let result = embedder
        .embed_documents(vec![DocumentChunks {
            document_context: String::new(),
            chunks: vec![
                query_systems.to_string(),
                doc_systems.to_string(),
                query_culinary.to_string(),
                doc_culinary.to_string(),
            ],
        }])
        .await
        .expect("embed four texts");

    let embeddings = &result[0];
    assert_eq!(embeddings.len(), 4);
    for emb in embeddings {
        assert_eq!(emb.len(), 1024, "int8 embedding dim should be 1024");
    }

    let sim_sys_sys = cosine_sim(&embeddings[0], &embeddings[1]);
    let sim_sys_cul = cosine_sim(&embeddings[0], &embeddings[3]);
    let sim_cul_cul = cosine_sim(&embeddings[2], &embeddings[3]);
    let sim_cul_sys = cosine_sim(&embeddings[2], &embeddings[1]);

    eprintln!(
        "cosine sims:\n  \
         systems query × systems doc  = {sim_sys_sys:.4}\n  \
         systems query × culinary doc = {sim_sys_cul:.4}\n  \
         culinary query × culinary doc = {sim_cul_cul:.4}\n  \
         culinary query × systems doc  = {sim_cul_sys:.4}"
    );

    assert!(
        sim_sys_sys > sim_sys_cul,
        "systems query should rank systems doc higher: {sim_sys_sys:.4} vs {sim_sys_cul:.4}"
    );
    assert!(
        sim_cul_cul > sim_cul_sys,
        "culinary query should rank culinary doc higher: {sim_cul_cul:.4} vs {sim_cul_sys:.4}"
    );
}
