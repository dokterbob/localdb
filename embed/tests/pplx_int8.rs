//! Slow integration test: pplx-embed-v1 int8 embeddings via direct ORT inference.
//!
//! This test validates that the `pplx-embed-v1-0.6B` model (ONNX Community export)
//! produces int8 embeddings that correctly separate semantically distinct documents.
//! It uses ORT and the `tokenizers` crate directly rather than fastembed, because the
//! model uses split external data files (`model.onnx` + `model.onnx_data`) which
//! fastembed's `UserDefinedEmbeddingModel` does not support in v5.1.x.
//!
//! # Model
//!
//! `perplexity-ai/pplx-embed-v1-0.6b` — the official Perplexity ONNX export.
//! Access requires a HuggingFace account with model access; set `HF_TOKEN` to your
//! token before running.  The model uses split external data files (`model.onnx` +
//! `model.onnx_data` + `model.onnx_data_1`) which fastembed's
//! `UserDefinedEmbeddingModel` does not support in v5.1.x.
//!
//! # ONNX output layout (index → semantics)
//!
//! | Index | dtype   | shape        | meaning                        |
//! |-------|---------|--------------|--------------------------------|
//! | 0     | float32 | [B, 1024]    | float pooled embeddings        |
//! | 1     | float32 | [B, S, H]    | last hidden states (token lvl) |
//! | 2     | int8    | [B, 1024]    | int8 quantised embeddings ← we use this |
//! | 3     | int8    | [B, 1024]    | binary embeddings (±1 as int8) |
//!
//! # Running
//!
//! ```sh
//! HF_TOKEN=<your-token> cargo test -p embed --features local-onnx -- --ignored pplx_embed_v1_int8
//! ```
//!
//! The first run downloads ~1 GB of model files to
//! `<platform-cache>/localdb/test-models/pplx-embed-v1-0.6b/`.  Subsequent runs
//! use the local cache and complete in seconds.

// Only compile this file when the local-onnx feature is active, which ensures
// fastembed (and therefore ORT) is compiled and the ORT binary is present.
#![cfg(feature = "local-onnx")]

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use futures_util::StreamExt;
use ndarray::{Array2, ArrayViewD, Axis};
use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};

/// HuggingFace model repo — official Perplexity ONNX export.
const REPO_ID: &str = "perplexity-ai/pplx-embed-v1-0.6b";

/// Expected embedding dimension for pplx-embed-v1-0.6B.
const EMBED_DIM: usize = 1024;

/// ONNX output index for the int8 pooled embeddings.
const INT8_OUTPUT_IDX: usize = 2;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn model_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("localdb")
        .join("test-models")
        .join("pplx-embed-v1-0.6b")
}

/// Download `remote_path` from `REPO_ID` to `dest`, skipping if already present.
///
/// 404 responses are silently ignored — some files (e.g. `model.onnx_data`) may not
/// exist for all-inline models.
async fn download_if_missing(client: &reqwest::Client, remote_path: &str, dest: &Path) {
    if dest.exists() {
        return;
    }
    std::fs::create_dir_all(dest.parent().unwrap())
        .unwrap_or_else(|e| panic!("create dir for {}: {e}", dest.display()));

    let url = format!("https://huggingface.co/{REPO_ID}/resolve/main/{remote_path}");
    eprintln!("  downloading {url}");

    let mut req = client.get(&url).header("user-agent", "localdb-test/0.1");
    if let Ok(token) = std::env::var("HF_TOKEN") {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url}: {e}"));

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        eprintln!("  skip {remote_path}: 404 (all-inline model — no external data file)");
        return;
    }
    assert!(
        resp.status().is_success(),
        "unexpected HTTP {} for {url} (if 401, set HF_TOKEN=<your-token> and accept the model \
         license at https://huggingface.co/{REPO_ID})",
        resp.status()
    );

    let total_mb = resp.content_length().map(|n| n / 1_048_576);
    let tmp = dest.with_extension("part");
    let mut file =
        std::fs::File::create(&tmp).unwrap_or_else(|e| panic!("create {}: {e}", tmp.display()));
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_reported: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap_or_else(|e| panic!("stream error for {url}: {e}"));
        file.write_all(&chunk)
            .unwrap_or_else(|e| panic!("write {}: {e}", tmp.display()));
        downloaded += chunk.len() as u64;
        let mb = downloaded / 1_048_576;
        if mb >= last_reported + 50 {
            match total_mb {
                Some(t) => eprintln!("  {remote_path}: {mb}/{t} MB"),
                None => eprintln!("  {remote_path}: {mb} MB"),
            }
            last_reported = mb;
        }
    }
    drop(file);
    std::fs::rename(&tmp, dest)
        .unwrap_or_else(|e| panic!("rename {} → {}: {e}", tmp.display(), dest.display()));
    eprintln!("  saved {} ({} MB)", dest.display(), downloaded / 1_048_576);
}

/// Download all required model files, returning the local model directory.
async fn ensure_model_files(client: &reqwest::Client) -> PathBuf {
    let dir = model_cache_dir();

    // Required files
    let required: &[&str] = &[
        "onnx/model.onnx",
        "tokenizer.json",
        "tokenizer_config.json",
        "special_tokens_map.json",
        "config.json",
    ];
    for &f in required {
        download_if_missing(client, f, &dir.join(f)).await;
    }

    // External weight shards (pplx-embed-v1-0.6b splits into two shards).
    // 404 is silently skipped for models that inline weights or have fewer shards.
    for shard in &["onnx/model.onnx_data", "onnx/model.onnx_data_1"] {
        download_if_missing(client, shard, &dir.join(shard)).await;
    }

    dir
}

/// Embed a single text with the given ORT session, returning int8 values as f32.
///
/// `session` must be `&mut` because `Session::run` takes `&mut self` (ort 2.x).
/// `tokenizer` is `&` because `Tokenizer::encode` only needs `&self`.
fn embed_single(
    session: &mut ort::session::Session,
    tokenizer: &Tokenizer,
    text: &str,
) -> Vec<f32> {
    let encoding = tokenizer.encode(text, false).expect("tokenize");

    let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
    let mask: Vec<i64> = encoding
        .get_attention_mask()
        .iter()
        .map(|&x| x as i64)
        .collect();
    let seq_len = ids.len();

    let ids_arr = Array2::from_shape_vec((1usize, seq_len), ids).expect("shape ids");
    let mask_arr = Array2::from_shape_vec((1usize, seq_len), mask).expect("shape mask");

    // Build ORT tensors from owned ndarray arrays.
    //
    // We use positional (unnamed) inputs here.  The pplx ONNX model has exactly
    // two inputs in order: [input_ids, attention_mask], which maps to positional
    // index 0 and 1.  This avoids the Result wrapping that the named-input form
    // `inputs!["name" => ndarray_value]` introduces when the value is not yet a
    // `Tensor<T>`.  We pre-create Tensor<i64> values so the positional `inputs!`
    // macro can accept them without returning a Result.
    let ids_tensor = ort::value::Tensor::from_array(ids_arr).expect("ids tensor");
    let mask_tensor = ort::value::Tensor::from_array(mask_arr).expect("mask tensor");

    // `ort::inputs![t1, t2]` with pre-created Tensor<T> values returns
    // `SessionInputs` directly (not a Result).  `session.run(...)` returns
    // `Result<SessionOutputs<'_>>`.
    let outputs = session
        .run(ort::inputs![ids_tensor, mask_tensor])
        .expect("ort session run");

    // Extract int8 pooled embeddings at output index INT8_OUTPUT_IDX.
    // Shape: [1, EMBED_DIM]. Type: int8.
    let int8_view: ArrayViewD<i8> = outputs[INT8_OUTPUT_IDX]
        .try_extract_array()
        .expect("extract int8 tensor from ONNX output[2]");

    // Take row 0 (only batch element) and cast i8 → f32 for cosine similarity.
    // Values are in [-128, 127]; cosine similarity is scale-invariant so this
    // produces the same ranking as comparing the native int8 values.
    int8_view
        .index_axis(Axis(0), 0)
        .iter()
        .map(|&x| x as f32)
        .collect()
}

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

// ---------------------------------------------------------------------------
// Integration test
// ---------------------------------------------------------------------------

/// Validate that pplx-embed-v1-0.6B int8 embeddings correctly rank two
/// documents from distinct semantic domains.
///
/// **Slow — downloads ~1 GB on first run.**  Skip in CI (the default).
/// Run locally with:
///
/// ```sh
/// cargo test -p embed --features local-onnx -- --ignored pplx_embed_v1_int8
/// ```
///
/// Cached under `<platform-cache>/localdb/test-models/pplx-embed-v1-0.6b/`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: downloads ~1 GB of ONNX model files on first run; run with --ignored"]
async fn pplx_embed_v1_int8_two_documents_retrieval() {
    // -----------------------------------------------------------------------
    // 1. Ensure model files are present (download if needed).
    // -----------------------------------------------------------------------
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600)) // large files
        .build()
        .unwrap();

    eprintln!("\n[pplx_int8] ensuring model files for {REPO_ID}");
    let model_dir = ensure_model_files(&client).await;

    let model_onnx = model_dir.join("onnx").join("model.onnx");
    assert!(
        model_onnx.exists(),
        "model.onnx not found at {} — download may have failed",
        model_onnx.display()
    );

    // -----------------------------------------------------------------------
    // 2. Load tokenizer.
    // -----------------------------------------------------------------------
    let tokenizer_path = model_dir.join("tokenizer.json");
    let mut tokenizer = Tokenizer::from_file(&tokenizer_path).expect("load tokenizer.json");

    // Truncate to 512 tokens to keep inference fast for short test texts.
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: 512,
            strategy: TruncationStrategy::LongestFirst,
            stride: 0,
            direction: TruncationDirection::Right,
        }))
        .expect("configure truncation");

    // -----------------------------------------------------------------------
    // 3. Load ORT session.
    //    ORT automatically resolves `model.onnx_data` from the same directory.
    // -----------------------------------------------------------------------
    eprintln!(
        "[pplx_int8] loading ORT session from {}",
        model_onnx.display()
    );
    let mut session = ort::session::Session::builder()
        .expect("ort SessionBuilder")
        .commit_from_file(&model_onnx)
        .expect(
            "load pplx ONNX model (if this fails with a 'custom op' error, try \
                  perplexity-ai/pplx-embed-v1-0.6b with a contrib-ops-enabled ORT build)",
        );

    // -----------------------------------------------------------------------
    // 4. Define two semantically distinct (document, query) pairs.
    // -----------------------------------------------------------------------
    let doc_systems =
        "Rust is a systems programming language that guarantees memory safety without a \
         garbage collector by using an ownership model enforced at compile time.";
    let doc_culinary =
        "Sauté diced onions in unsalted butter over medium heat until translucent, then \
         deglaze the pan with dry white wine to create a rich sauce base.";

    let query_systems = "memory safe programming without garbage collection";
    let query_culinary = "how to caramelise onions when cooking";

    // -----------------------------------------------------------------------
    // 5. Embed all four texts.
    // -----------------------------------------------------------------------
    eprintln!("[pplx_int8] embedding four texts");
    let emb_q_sys = embed_single(&mut session, &tokenizer, query_systems);
    let emb_d_sys = embed_single(&mut session, &tokenizer, doc_systems);
    let emb_q_cul = embed_single(&mut session, &tokenizer, query_culinary);
    let emb_d_cul = embed_single(&mut session, &tokenizer, doc_culinary);

    // -----------------------------------------------------------------------
    // 6. Sanity-check embedding dimensions.
    // -----------------------------------------------------------------------
    assert_eq!(
        emb_q_sys.len(),
        EMBED_DIM,
        "int8 embedding dim should be {EMBED_DIM}"
    );
    assert_eq!(emb_d_sys.len(), EMBED_DIM);
    assert_eq!(emb_q_cul.len(), EMBED_DIM);
    assert_eq!(emb_d_cul.len(), EMBED_DIM);

    // -----------------------------------------------------------------------
    // 7. Verify semantic ranking via cosine similarity.
    //    Each query should score higher against its own document than against
    //    the other domain's document.
    // -----------------------------------------------------------------------
    let sim_sys_sys = cosine_sim(&emb_q_sys, &emb_d_sys);
    let sim_sys_cul = cosine_sim(&emb_q_sys, &emb_d_cul);
    let sim_cul_cul = cosine_sim(&emb_q_cul, &emb_d_cul);
    let sim_cul_sys = cosine_sim(&emb_q_cul, &emb_d_sys);

    eprintln!(
        "[pplx_int8] cosine sims:\n  \
         systems query × systems doc = {sim_sys_sys:.4}\n  \
         systems query × culinary doc = {sim_sys_cul:.4}\n  \
         culinary query × culinary doc = {sim_cul_cul:.4}\n  \
         culinary query × systems doc = {sim_cul_sys:.4}"
    );

    assert!(
        sim_sys_sys > sim_sys_cul,
        "systems query should rank systems doc higher than culinary doc: \
         {sim_sys_sys:.4} vs {sim_sys_cul:.4}"
    );
    assert!(
        sim_cul_cul > sim_cul_sys,
        "culinary query should rank culinary doc higher than systems doc: \
         {sim_cul_cul:.4} vs {sim_cul_sys:.4}"
    );

    eprintln!(
        "[pplx_int8] PASS — pplx-embed-v1 int8 embeddings separate the two domains correctly"
    );
}
