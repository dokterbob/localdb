//! Local pplx-embed-v1-0.6b embedder via direct ORT inference.
//!
//! Uses `ort` + `tokenizers` directly rather than fastembed, because the model
//! ships split external data files (`model.onnx` + `model.onnx_data` +
//! `model.onnx_data_1`) which fastembed's `UserDefinedEmbeddingModel` does not
//! support in v5.1.x.
//!
//! # Model
//!
//! `perplexity-ai/pplx-embed-v1-0.6b` — the official Perplexity ONNX export.
//! Access requires a HuggingFace account with model access (gated repo).
//! Set `HF_TOKEN` to a valid token before the first run; subsequent runs use
//! the local cache and do not require the token.
//!
//! # ONNX output layout
//!
//! | Index | dtype   | shape     | meaning                        |
//! |-------|---------|-----------|--------------------------------|
//! | 0     | float32 | [B, 1024] | float pooled embeddings        |
//! | 1     | float32 | [B, S, H] | last hidden states             |
//! | 2     | int8    | [B, 1024] | int8 quantised embeddings ← used here |
//! | 3     | int8    | [B, 1024] | binary embeddings              |
//!
//! Int8 values are cast to f32 before storage; cosine similarity is
//! scale-invariant so ranking is identical to native int8 comparison.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use ndarray::{Array2, ArrayViewD, Axis};
use tokenizers::{
    PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationDirection,
    TruncationParams, TruncationStrategy,
};
use tracing::info;

use localdb_core::{DocumentChunks, EmbeddedDocument, Embedder, Error as CoreError, TokenCounter};

use crate::error::EmbedError;

const HF_REPO: &str = "perplexity-ai/pplx-embed-v1-0.6b";
const EMBED_DIM: usize = 1024;
const INT8_OUTPUT_IDX: usize = 2;
const MAX_SEQ_LEN: usize = 512;
const HF_REVISION: &str = "2c4d510dd4a732063c31a0f70193e35067b51fd8";

// Required files to download (path inside the HF repo → relative dest under model_dir).
const REQUIRED_FILES: &[&str] = &[
    "onnx/model.onnx",
    "tokenizer.json",
    "tokenizer_config.json",
    "special_tokens_map.json",
    "config.json",
];

// Optional shards: present for this model, silently skipped on 404.
const OPTIONAL_FILES: &[&str] = &["onnx/model.onnx_data", "onnx/model.onnx_data_1"];

// ---------------------------------------------------------------------------
// Download helpers
// ---------------------------------------------------------------------------

async fn download_hf_file(
    client: &reqwest::Client,
    remote_path: &str,
    dest: &Path,
    optional: bool,
    show_progress: bool,
) -> Result<(), EmbedError> {
    if dest.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(EmbedError::Io)?;

    let url = format!("https://huggingface.co/{HF_REPO}/resolve/{HF_REVISION}/{remote_path}");
    info!("downloading {url}");

    let mut req = client.get(&url).header("user-agent", "localdb/0.1");
    if let Ok(token) = std::env::var("HF_TOKEN") {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().await.map_err(EmbedError::Http)?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND && optional {
        info!("skip {remote_path}: 404 (optional file absent)");
        return Ok(());
    }
    if !resp.status().is_success() {
        return Err(EmbedError::ProviderError {
            provider: "huggingface".into(),
            message: format!(
                "HTTP {} downloading {remote_path} \
                 (if 401: set HF_TOKEN env var and accept the model license at \
                 https://huggingface.co/{HF_REPO})",
                resp.status()
            ),
        });
    }

    let total_mb = resp.content_length().map(|n| n / 1_048_576);
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp).map_err(EmbedError::Io)?;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_reported: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(EmbedError::Http)?;
        file.write_all(&chunk).map_err(EmbedError::Io)?;
        downloaded += chunk.len() as u64;
        if show_progress {
            let mb = downloaded / 1_048_576;
            if mb >= last_reported + 50 {
                match total_mb {
                    Some(t) => info!("{remote_path}: {mb}/{t} MB"),
                    None => info!("{remote_path}: {mb} MB"),
                }
                last_reported = mb;
            }
        }
    }
    drop(file);
    std::fs::rename(&tmp, dest).map_err(EmbedError::Io)?;
    info!("saved {} ({} MB)", dest.display(), downloaded / 1_048_576);
    Ok(())
}

async fn ensure_model_files(model_dir: &Path, show_progress: bool) -> Result<(), EmbedError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(EmbedError::Http)?;

    for &f in REQUIRED_FILES {
        download_hf_file(&client, f, &model_dir.join(f), false, show_progress).await?;
    }
    for &f in OPTIONAL_FILES {
        download_hf_file(&client, f, &model_dir.join(f), true, show_progress).await?;
    }
    Ok(())
}

fn download_model_blocking(model_dir: &Path, show_progress: bool) -> Result<(), EmbedError> {
    if model_dir.join("onnx").join("model.onnx").exists() {
        return Ok(());
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| {
                handle.block_on(ensure_model_files(model_dir, show_progress))
            })
        }
        Ok(_) => {
            // current-thread runtime: can't block_in_place from within it.
            let model_dir = model_dir.to_path_buf();
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| EmbedError::Internal(format!("create tokio runtime: {e}")))?
                    .block_on(ensure_model_files(&model_dir, show_progress))
            })
            .join()
            .map_err(|_| EmbedError::Internal("download thread panicked".into()))?
        }
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| EmbedError::Internal(format!("create tokio runtime: {e}")))?
            .block_on(ensure_model_files(model_dir, show_progress)),
    }
}

// ---------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------

fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::RuntimeFlavor;
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Local pplx-embed-v1-0.6b embedder using direct ORT inference.
///
/// Produces 1024-dimensional int8 embeddings (cast to f32) from the official
/// Perplexity ONNX export.  Downloads model files on first use (~2.4 GB total);
/// requires `HF_TOKEN` env var on first run.
pub struct PplxOnnxEmbedder {
    session: Mutex<ort::session::Session>,
    tokenizer: Tokenizer,
}

impl PplxOnnxEmbedder {
    /// Create a new embedder, downloading model files into `cache_dir` if absent.
    ///
    /// `cache_dir`: parent directory for the model cache; the model is stored
    /// under `<cache_dir>/pplx-embed-v1-0.6b/`.  Defaults to the platform
    /// model cache (`~/Library/Caches/localdb/models/` on macOS).
    ///
    /// `show_progress`: emit download progress via `tracing::info!`.
    pub fn new(cache_dir: Option<PathBuf>, show_progress: bool) -> Result<Self, EmbedError> {
        let model_dir = cache_dir
            .unwrap_or_else(crate::model_cache::ModelCache::default_cache_dir)
            .join("pplx-embed-v1-0.6b");

        download_model_blocking(&model_dir, show_progress)?;

        let tokenizer_path = model_dir.join("tokenizer.json");
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            EmbedError::ModelMissing(format!(
                "failed to load tokenizer.json from {}: {e}",
                tokenizer_path.display()
            ))
        })?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_SEQ_LEN,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
                direction: TruncationDirection::Right,
            }))
            .map_err(|e| EmbedError::Internal(format!("configure tokenizer truncation: {e}")))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "<pad>".to_string(),
        }));

        let model_onnx = model_dir.join("onnx").join("model.onnx");
        info!(model = "pplx-embed-v1-0.6b", "loading ORT session");
        let session = ort::session::Session::builder()
            .map_err(|e| EmbedError::Internal(format!("ORT SessionBuilder: {e}")))?
            .commit_from_file(&model_onnx)
            .map_err(|e| {
                EmbedError::ModelMissing(format!(
                    "failed to load pplx ONNX model from {}: {e}",
                    model_onnx.display()
                ))
            })?;
        info!(model = "pplx-embed-v1-0.6b", "ORT session ready");

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    fn embed_texts_sync(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let encodings = self
            .tokenizer
            .encode_batch(texts.iter().map(|s| s.as_str()).collect::<Vec<_>>(), false)
            .map_err(|e| EmbedError::Internal(format!("tokenize batch: {e}")))?;

        let batch_size = encodings.len();
        let seq_len = encodings[0].get_ids().len();

        let mut input_ids_flat: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        let mut attention_mask_flat: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        for enc in &encodings {
            input_ids_flat.extend(enc.get_ids().iter().map(|&x| x as i64));
            attention_mask_flat.extend(enc.get_attention_mask().iter().map(|&x| x as i64));
        }

        let ids_arr = Array2::from_shape_vec((batch_size, seq_len), input_ids_flat)
            .map_err(|e| EmbedError::Internal(format!("shape ids: {e}")))?;
        let mask_arr = Array2::from_shape_vec((batch_size, seq_len), attention_mask_flat)
            .map_err(|e| EmbedError::Internal(format!("shape mask: {e}")))?;

        let ids_tensor = ort::value::Tensor::from_array(ids_arr)
            .map_err(|e| EmbedError::Internal(format!("ids tensor: {e}")))?;
        let mask_tensor = ort::value::Tensor::from_array(mask_arr)
            .map_err(|e| EmbedError::Internal(format!("mask tensor: {e}")))?;

        let mut session = self
            .session
            .lock()
            .map_err(|e| EmbedError::Internal(format!("session mutex poisoned: {e}")))?;

        let outputs = session
            .run(ort::inputs![ids_tensor, mask_tensor])
            .map_err(|e| EmbedError::Internal(format!("ORT run: {e}")))?;

        let view: ArrayViewD<i8> = outputs[INT8_OUTPUT_IDX]
            .try_extract_array()
            .map_err(|e| EmbedError::Internal(format!("extract int8 array: {e}")))?;

        // Shape: [B, 1024] — one row per text.
        (0..batch_size)
            .map(|b| {
                Ok(view
                    .index_axis(Axis(0), b)
                    .iter()
                    .map(|&x| x as f32)
                    .collect())
            })
            .collect()
    }
}

#[async_trait]
impl Embedder for PplxOnnxEmbedder {
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, CoreError> {
        if docs.is_empty() {
            return Ok(vec![]);
        }

        let mut all_texts: Vec<String> = Vec::new();
        let mut doc_offsets: Vec<(usize, usize)> = Vec::new();

        for doc in &docs {
            let start = all_texts.len();
            all_texts.extend(doc.chunks.iter().cloned());
            doc_offsets.push((start, doc.chunks.len()));
        }

        if all_texts.is_empty() {
            return Ok(docs.iter().map(|_| vec![]).collect());
        }

        let all_embeddings: Vec<Vec<f32>> =
            run_blocking(|| self.embed_texts_sync(&all_texts).map_err(CoreError::from))?;

        Ok(doc_offsets
            .into_iter()
            .map(|(start, len)| all_embeddings[start..start + len].to_vec())
            .collect())
    }

    fn embedding_dim(&self) -> usize {
        EMBED_DIM
    }

    fn model_id(&self) -> &str {
        "pplx-embed-v1-0.6b"
    }

    fn vector_encoding(&self) -> localdb_core::VectorEncoding {
        localdb_core::VectorEncoding::Binary
    }

    fn token_counter(&self) -> Option<TokenCounter> {
        let tok = self.tokenizer.clone();
        Some(Arc::new(move |t: &str| {
            tok.encode(t, false).map(|e| e.get_ids().len()).unwrap_or(0)
        }))
    }
}
