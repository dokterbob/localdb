//! Gating benchmark for embedding throughput.
//!
//! This example measures end-to-end embedding throughput on the local ONNX provider.
//!
//! # Purpose
//!
//! Validates the gate from specs/04-search-pipeline.md §4:
//! > **Gate:** sustained ≥ 15 chunks/s end-to-end and first-index ≤ 30 min;
//! > if missed, the bge-small-class preset becomes the default and the 0.6b model the opt-in
//! > quality preset.
//!
//! # Usage
//!
//! ```
//! cargo run --example embedding_benchmark --features local-onnx
//! ```
//!
//! Run on the target hardware (Apple Silicon or Linux x86_64) and report results.
//! This is a human-run step, not CI.
//!
//! # Test corpus
//!
//! Generates synthetic chunks of ~400 tokens each (prose preset target size),
//! matching the expected index corpus size of ~2000 files / ~100 MB.

#[cfg(feature = "local-onnx")]
mod bench_impl {
    use embed::onnx::{ModelChoice, OnnxEmbedder};
    use localdb_core::{DocumentChunks, Embedder};
    use std::time::Instant;

    /// Generate a synthetic chunk of approximately `n_tokens` tokens.
    ///
    /// Each "token" is approximated as ~5 characters.
    pub fn synthetic_chunk(seed: u64, n_tokens: usize) -> String {
        let words = [
            "the",
            "quick",
            "brown",
            "fox",
            "jumps",
            "over",
            "lazy",
            "dog",
            "rust",
            "programming",
            "language",
            "memory",
            "safety",
            "performance",
            "embedding",
            "vector",
            "search",
            "semantic",
            "retrieval",
            "document",
            "context",
            "chunk",
            "model",
            "local",
            "index",
            "pipeline",
            "async",
        ];
        let mut text = String::with_capacity(n_tokens * 5);
        let mut rng = seed;
        for i in 0..n_tokens {
            if i > 0 {
                text.push(' ');
            }
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let word = words[(rng >> 33) as usize % words.len()];
            text.push_str(word);
        }
        text
    }

    /// Generate a synthetic document with `n_chunks` chunks of ~400 tokens each.
    pub fn synthetic_document(doc_idx: u64, n_chunks: usize) -> DocumentChunks {
        let context = synthetic_chunk(doc_idx * 1000, 400);
        let chunks: Vec<String> = (0..n_chunks)
            .map(|i| synthetic_chunk(doc_idx * 1000 + i as u64, 400))
            .collect();
        DocumentChunks {
            document_context: context,
            chunks,
        }
    }

    pub async fn run_benchmark(n_docs: usize, chunks_per_doc: usize) {
        println!("=== localdb Embedding Benchmark ===");
        println!("Provider: Local ONNX (BGE Small EN v1.5)");
        println!("Documents: {n_docs}");
        println!("Chunks per document: {chunks_per_doc}");
        println!("Total chunks: {}", n_docs * chunks_per_doc);
        println!("Chunk size: ~400 tokens");
        println!();

        // Load model
        print!("Loading model... ");
        let load_start = Instant::now();
        let embedder =
            OnnxEmbedder::new(ModelChoice::Default, None, true).expect("failed to load ONNX model");
        let load_time = load_start.elapsed();
        println!("done ({:.1}s)", load_time.as_secs_f64());
        println!(
            "Model: {} ({}d)",
            embedder.model_id(),
            embedder.embedding_dim()
        );
        println!();

        // Generate corpus
        print!("Generating synthetic corpus... ");
        let docs: Vec<DocumentChunks> = (0..n_docs as u64)
            .map(|i| synthetic_document(i, chunks_per_doc))
            .collect();
        println!("done");

        // Warm-up run (1 doc)
        let warmup = embedder
            .embed_documents(vec![docs[0].clone()])
            .await
            .expect("warm-up failed");
        println!("Warm-up complete, first vector dim: {}", warmup[0][0].len());
        println!();

        // Benchmark: process all documents in batches
        println!("Running benchmark...");
        let start = Instant::now();

        // Process in batches of 32 documents
        let batch_size = 32;
        let mut total_chunks = 0usize;
        let mut batches = 0usize;

        for batch in docs.chunks(batch_size) {
            let batch_start = Instant::now();
            let results = embedder
                .embed_documents(batch.to_vec())
                .await
                .expect("embedding failed");
            let batch_chunks: usize = results.iter().map(|r| r.len()).sum();
            total_chunks += batch_chunks;
            batches += 1;
            let batch_elapsed = batch_start.elapsed();
            println!(
                "  Batch {batches}: {batch_chunks} chunks in {:.2}s ({:.1} chunks/s)",
                batch_elapsed.as_secs_f64(),
                batch_chunks as f64 / batch_elapsed.as_secs_f64()
            );
        }

        let total_elapsed = start.elapsed();
        let throughput = total_chunks as f64 / total_elapsed.as_secs_f64();

        println!();
        println!("=== Results ===");
        println!("Total chunks embedded: {total_chunks}");
        println!("Total time: {:.1}s", total_elapsed.as_secs_f64());
        println!("Throughput: {throughput:.1} chunks/s");
        println!(
            "Estimated time for 2000 files × 10 chunks/file: {:.1} min",
            (20000.0 / throughput) / 60.0
        );
        println!();

        // Gate check
        let gate_passed = throughput >= 15.0;
        println!(
            "Gate (≥15 chunks/s): {}",
            if gate_passed {
                "PASSED ✓"
            } else {
                "FAILED ✗"
            }
        );
        if !gate_passed {
            println!(
                "  → bge-small-en-v1.5 remains the default; pplx-embed-context-v1-0.6b is opt-in"
            );
        } else {
            println!("  → pplx-embed-context-v1-0.6b can be confirmed as default (when available)");
        }
    }
}

#[cfg(not(feature = "local-onnx"))]
async fn run_no_feature() {
    eprintln!("Error: this benchmark requires the `local-onnx` feature.");
    eprintln!("Run with: cargo run --example embedding_benchmark --features local-onnx");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    // Default: 200 docs × 10 chunks = 2000 total chunks
    // Adjust as needed for the full 2000-file / 100 MB corpus test
    let n_docs: usize = std::env::var("BENCH_N_DOCS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let chunks_per_doc: usize = std::env::var("BENCH_CHUNKS_PER_DOC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    #[cfg(feature = "local-onnx")]
    bench_impl::run_benchmark(n_docs, chunks_per_doc).await;

    #[cfg(not(feature = "local-onnx"))]
    {
        let _ = (n_docs, chunks_per_doc);
        run_no_feature().await;
    }
}
