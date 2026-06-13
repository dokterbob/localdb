//! Smoke test: pplx-embed-context-v1-0.6b via `PplxContextOnnxEmbedder`.
//!
//! Verifies end-to-end that the late-chunking embedder produces 1024-dim vectors,
//! one per chunk (guarding the SEP-split contract), and that cosine similarity
//! ranks same-topic chunks above cross-topic chunks.
//!
//! Downloads ~706 MB (quantized) from the public MIT repo on first run; no token
//! required.  Subsequent runs use the cached files.
//!
//! # Running
//!
//! ```sh
//! cargo test -p embed --features local-onnx -- --ignored pplx_embed_context
//! ```

#![cfg(feature = "local-onnx")]

use embed::PplxContextOnnxEmbedder;
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

/// Validate chunk-group windowing: a document whose chunks sum to > MAX_SEQ_LEN (8192 tokens)
/// must still return exactly one embedding per chunk without error.
///
/// **Slow — requires the locally-cached model.**
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: downloads ~706 MB of quantized ONNX model files on first run; run with --ignored"]
async fn pplx_embed_context_oversized_document_windowed() {
    let embedder =
        PplxContextOnnxEmbedder::new(None, false).expect("create PplxContextOnnxEmbedder");

    // Each repetition of this sentence is ~10 tokens; 500 reps ≈ 5 000 tokens per chunk.
    // Two such chunks sum to ~10 001 tokens + 1 SEP > 8 192 → windowing splits them.
    let long_chunk = "The quick brown fox jumps over the lazy dog. ".repeat(500);
    let n_chunks = 3usize;
    let chunks: Vec<String> = (0..n_chunks).map(|_| long_chunk.clone()).collect();

    let result = embedder
        .embed_documents(vec![DocumentChunks {
            document_context: String::new(),
            chunks,
        }])
        .await
        .expect("oversized document must not error with windowing enabled");

    assert_eq!(
        result[0].len(),
        n_chunks,
        "windowing must produce exactly one embedding per chunk"
    );
    for emb in &result[0] {
        assert_eq!(emb.len(), 1024, "embedding dim should be 1024");
    }
}

/// Validate pplx-embed-context-v1-0.6b late-chunking via `PplxContextOnnxEmbedder`.
///
/// **Slow — downloads ~706 MB on first run.**  Skip in CI (the default).
/// Run locally with:
///
/// ```sh
/// cargo test -p embed --features local-onnx -- --ignored pplx_embed_context
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: downloads ~706 MB of quantized ONNX model files on first run; run with --ignored"]
async fn pplx_embed_context_late_chunking_four_chunks() {
    let embedder =
        PplxContextOnnxEmbedder::new(None, true).expect("create PplxContextOnnxEmbedder");

    assert_eq!(embedder.embedding_dim(), 1024);
    assert_eq!(embedder.model_id(), "pplx-embed-context-v1-0.6b");

    let chunk_sys1 =
        "Rust is a systems programming language that guarantees memory safety without \
         a garbage collector by using an ownership model enforced at compile time.";
    let chunk_sys2 =
        "The borrow checker in Rust statically prevents data races, dangling pointers, \
         and use-after-free bugs by tracking ownership and lifetimes at compile time.";
    let chunk_cul1 =
        "Sauté diced onions in unsalted butter over medium heat until translucent, then \
         deglaze the pan with dry white wine to create a rich sauce base.";
    let chunk_cul2 =
        "Fold beaten egg whites into the chocolate batter gently to preserve the air, \
         then bake at 170 °C until a skewer inserted in the centre comes out clean.";

    let result = embedder
        .embed_documents(vec![DocumentChunks {
            document_context: String::new(),
            chunks: vec![
                chunk_sys1.to_string(),
                chunk_sys2.to_string(),
                chunk_cul1.to_string(),
                chunk_cul2.to_string(),
            ],
        }])
        .await
        .expect("embed four chunks");

    // One vector per chunk — guards the SEP-split contract.
    assert_eq!(result[0].len(), 4, "expected one embedding per chunk");
    for emb in &result[0] {
        assert_eq!(emb.len(), 1024, "embedding dim should be 1024");
    }

    let embeddings = &result[0];
    let sim_sys12 = cosine_sim(&embeddings[0], &embeddings[1]);
    let sim_sys1_cul1 = cosine_sim(&embeddings[0], &embeddings[2]);
    let sim_cul12 = cosine_sim(&embeddings[2], &embeddings[3]);
    let sim_cul1_sys1 = cosine_sim(&embeddings[2], &embeddings[0]);

    eprintln!(
        "cosine sims:\n  \
         sys1 × sys2   = {sim_sys12:.4}\n  \
         sys1 × cul1   = {sim_sys1_cul1:.4}\n  \
         cul1 × cul2   = {sim_cul12:.4}\n  \
         cul1 × sys1   = {sim_cul1_sys1:.4}"
    );

    assert!(
        sim_sys12 > sim_sys1_cul1,
        "systems chunks should be closer to each other: {sim_sys12:.4} vs {sim_sys1_cul1:.4}"
    );
    assert!(
        sim_cul12 > sim_cul1_sys1,
        "culinary chunks should be closer to each other: {sim_cul12:.4} vs {sim_cul1_sys1:.4}"
    );
}
