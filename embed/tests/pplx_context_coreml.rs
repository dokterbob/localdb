//! Device + parity tests: pplx-embed-context-v1-0.6b via `PplxContextCoreMLEmbedder`.
//!
//! These tests exercise the real CoreML backend on Apple Silicon. They require a
//! one-time download of the CoreML bundle (`dokterbob/pplx-embed-coreml`, several
//! hundred MB) and a working Neural Engine / GPU, so they are `#[ignore]`d and
//! never run in CI (which stays offline). Run them manually with:
//!
//! ```sh
//! cargo test -p embed --features "local-onnx local-coreml" -- --ignored --test-threads=1
//! ```
//!
//! Use `--test-threads=1` on the first (cold-cache) run: both tests fetch the
//! same HF bundle and would otherwise contend on the HF cache lock and time out.
//! Once the bundle is cached they are safe to run in parallel.
//!
//! The parity test additionally compares the CoreML output against the ONNX
//! reference embedder and is gated behind `local-onnx`.

#![cfg(all(target_os = "macos", feature = "local-coreml"))]

use embed::PplxContextCoreMLEmbedder;
use localdb_core::{DocumentChunks, Embedder};

/// Four short, single-topic chunks reused across both tests.
fn sample_chunks() -> Vec<String> {
    vec![
        "Rust guarantees memory safety without a garbage collector using ownership.".to_string(),
        "The borrow checker statically prevents data races and use-after-free bugs.".to_string(),
        "Sauté diced onions in butter over medium heat until translucent.".to_string(),
        "Fold beaten egg whites into the chocolate batter to preserve the air.".to_string(),
    ]
}

/// Device smoke test: embed one document of four short chunks and validate the
/// shape and int8-cast-to-f32 value range of every vector.
///
/// **Slow — downloads the CoreML bundle on first run; requires Apple Silicon.**
#[tokio::test(flavor = "multi_thread")]
#[ignore = "device: downloads the CoreML bundle and needs Apple Silicon; run with --ignored"]
async fn coreml_context_device_smoke() {
    let embedder =
        PplxContextCoreMLEmbedder::new(None, false).expect("create PplxContextCoreMLEmbedder");

    assert_eq!(embedder.embedding_dim(), 1024);
    assert_eq!(embedder.model_id(), "pplx-embed-context-v1-0.6b");

    let chunks = sample_chunks();
    let n_chunks = chunks.len();
    let result = embedder
        .embed_documents(vec![DocumentChunks {
            document_context: String::new(),
            chunks,
        }])
        .await
        .expect("embed four chunks");

    assert_eq!(result.len(), 1, "one document in, one document out");
    assert_eq!(
        result[0].len(),
        n_chunks,
        "expected one embedding per chunk (got {})",
        result[0].len()
    );

    for (i, emb) in result[0].iter().enumerate() {
        assert_eq!(emb.len(), 1024, "chunk {i}: embedding dim should be 1024");
        for (d, &v) in emb.iter().enumerate() {
            assert!(
                (-128.0..=127.0).contains(&v),
                "chunk {i} dim {d}: value {v} out of int8 range [-128, 127]"
            );
            assert_eq!(
                v,
                v.trunc(),
                "chunk {i} dim {d}: value {v} is not integral (expected int8-cast-to-f32)"
            );
        }
    }
}

/// Multi-doc async pipeline: embed two documents simultaneously and verify that
/// each document's vectors are placed in the correct slots (tests scatter logic).
///
/// **Slow — downloads the CoreML bundle on first run; requires Apple Silicon.**
#[tokio::test(flavor = "multi_thread")]
#[ignore = "device: downloads the CoreML bundle and needs Apple Silicon; run with --ignored"]
async fn coreml_context_multi_doc_async_scatter() {
    let embedder =
        PplxContextCoreMLEmbedder::new(None, false).expect("create PplxContextCoreMLEmbedder");

    let doc0_chunks = vec![
        "Rust ownership prevents data races at compile time.".to_string(),
        "The borrow checker is Rust's secret weapon for safety.".to_string(),
    ];
    let doc1_chunks = vec![
        "Caramelise onions slowly over low heat for 40 minutes.".to_string(),
        "Deglaze the pan with white wine and reduce by half.".to_string(),
        "Season generously and finish with fresh thyme.".to_string(),
    ];

    let result = embedder
        .embed_documents(vec![
            DocumentChunks {
                document_context: String::new(),
                chunks: doc0_chunks.clone(),
            },
            DocumentChunks {
                document_context: String::new(),
                chunks: doc1_chunks.clone(),
            },
        ])
        .await
        .expect("embed two documents");

    assert_eq!(result.len(), 2, "two documents in, two documents out");
    assert_eq!(
        result[0].len(),
        doc0_chunks.len(),
        "doc 0: wrong chunk count"
    );
    assert_eq!(
        result[1].len(),
        doc1_chunks.len(),
        "doc 1: wrong chunk count"
    );

    for (doc_i, doc_embs) in result.iter().enumerate() {
        for (chunk_i, emb) in doc_embs.iter().enumerate() {
            assert_eq!(
                emb.len(),
                1024,
                "doc {doc_i} chunk {chunk_i}: expected dim 1024"
            );
            for &v in emb {
                assert!(
                    (-128.0..=127.0).contains(&v),
                    "doc {doc_i} chunk {chunk_i}: value {v} out of int8 range"
                );
            }
        }
    }

    // Vectors for different documents must not be identical (scatter correctness
    // check: wrong slot assignment would produce identical vectors).
    let doc0_first = &result[0][0];
    let doc1_first = &result[1][0];
    assert_ne!(
        doc0_first, doc1_first,
        "doc 0 chunk 0 and doc 1 chunk 0 must not be identical \
         (likely a scatter bug if they are)"
    );
}

/// Fraction of dimensions where `sign(a_i) == sign(b_i)`, treating `0` as
/// positive to match the binarization tie-rule (`x >= 0 -> +1`).
#[cfg(feature = "local-onnx")]
fn sign_agreement(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    let agree = a
        .iter()
        .zip(b.iter())
        .filter(|(&x, &y)| (x >= 0.0) == (y >= 0.0))
        .count();
    agree as f32 / a.len() as f32
}

#[cfg(feature = "local-onnx")]
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

/// Parity test: embed the same chunks through both the CoreML and the ONNX
/// reference embedders and require near-identical per-chunk vectors.
///
/// For each chunk pair we require cosine similarity ≥ 0.99 (the magnitude signal,
/// the strong correctness check) AND per-dimension sign agreement ≥ 0.98 (the
/// binarized retrieval signal). The sign threshold is looser than cosine on
/// purpose: CoreML runs fp16 on the ANE while ONNX runs fp32, so a handful of
/// dimensions whose pre-tanh value sits within fp16-rounding distance of zero
/// flip sign at the int8 quantization boundary — exactly the measure-zero tie
/// case the Swift reference doc-comment describes. Empirically the two backends
/// agree on ~0.989–0.994 of dimensions with cosine ~0.999; 0.98 catches a real
/// regression (a broken backend scores far lower) while tolerating tie flips.
///
/// **Slow — downloads both the CoreML bundle and the ONNX model on first run.**
#[cfg(feature = "local-onnx")]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "device + download: needs Apple Silicon and ~1 GB of model files; run with --ignored"]
async fn coreml_onnx_parity() {
    use embed::PplxContextOnnxEmbedder;

    let coreml =
        PplxContextCoreMLEmbedder::new(None, false).expect("create PplxContextCoreMLEmbedder");
    let onnx = PplxContextOnnxEmbedder::new(None, false).expect("create PplxContextOnnxEmbedder");

    let chunks = sample_chunks();
    let doc = DocumentChunks {
        document_context: String::new(),
        chunks: chunks.clone(),
    };

    let coreml_out = coreml
        .embed_documents(vec![doc.clone()])
        .await
        .expect("CoreML embed");
    let onnx_out = onnx.embed_documents(vec![doc]).await.expect("ONNX embed");

    assert_eq!(coreml_out[0].len(), chunks.len());
    assert_eq!(onnx_out[0].len(), chunks.len());

    for (i, (c, o)) in coreml_out[0].iter().zip(onnx_out[0].iter()).enumerate() {
        assert_eq!(c.len(), o.len(), "chunk {i}: dimension mismatch");
        let agree = sign_agreement(c, o);
        let cos = cosine_sim(c, o);
        eprintln!("chunk {i}: sign_agreement={agree:.4} cosine={cos:.4}");
        assert!(
            agree >= 0.98,
            "chunk {i}: per-dimension sign agreement {agree:.4} < 0.98 \
             (cosine = {cos:.4})"
        );
        assert!(
            cos >= 0.99,
            "chunk {i}: cosine similarity {cos:.4} < 0.99 \
             (sign agreement = {agree:.4})"
        );
    }
}
