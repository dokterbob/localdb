//! Local pplx-embed-context-v1-0.6b embedder via direct ORT inference.
//!
//! Late-chunking: a document's chunks are joined with the tokenizer's SEP token,
//! fed through the model as a single sequence, then the per-region hidden states
//! are mean-pooled back into per-chunk vectors.  Every chunk vector thus carries
//! cross-chunk context.
//!
//! # Model
//!
//! `perplexity-ai/pplx-embed-context-v1-0.6b` — MIT-licensed ONNX export.
//! Public repo (not gated); no HF_TOKEN required.  Downloads ~706 MB
//! (`onnx/model_quantized.onnx` + optional shard) on first use.
//!
//! # ONNX output layout
//!
//! | Index | dtype   | shape        | meaning           |
//! |-------|---------|--------------|-------------------|
//! | 0     | float32 | [B, S, 1024] | last_hidden_state |
//!
//! No built-in int8 output (unlike the base model).  We mean-pool per-region
//! hidden states and then apply tanh-squash in post:
//! `(x.tanh() * 127).round().clamp(-128, 127)` stored as f32.
//! Cosine similarity is preserved at full precision; the round+clamp keeps
//! the geometry bit-exact with Perplexity's published int8 vectors.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use ndarray::{Array2, ArrayView2, ArrayViewD, Axis, Ix3};
use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};
use tracing::info;

use localdb_core::{DocumentChunks, EmbeddedDocument, Embedder, Error as CoreError};

use crate::error::EmbedError;

const HF_REPO: &str = "perplexity-ai/pplx-embed-context-v1-0.6b";
const EMBED_DIM: usize = 1024;
// Model supports 32K tokens; cap at 8 192 to bound CPU cost per document.
const MAX_SEQ_LEN: usize = 8192;
const MODEL_DIRNAME: &str = "pplx-embed-context-v1-0.6b";

const REQUIRED_FILES: &[&str] = &[
    "onnx/model_quantized.onnx",
    "tokenizer.json",
    "tokenizer_config.json",
    "special_tokens_map.json",
    "config.json",
];

// The external data shard may or may not be present in the quantized variant.
const OPTIONAL_FILES: &[&str] = &["onnx/model_quantized.onnx_data"];

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

    let url = format!("https://huggingface.co/{HF_REPO}/resolve/main/{remote_path}");
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
                "HTTP {} downloading {remote_path} from {HF_REPO}. \
                 The repo is public — check your network connection and retry. \
                 Alternatively use provider: local-onnx, model: bge-small-en-v1.5 \
                 (384-dim, no download required beyond the small model).",
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
    // Fast path: quantized model already cached.
    if model_dir.join("onnx").join("model_quantized.onnx").exists() {
        return Ok(());
    }

    let fut = ensure_model_files(model_dir, show_progress);
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| EmbedError::Internal(format!("create tokio runtime: {e}")))?
            .block_on(fut),
    }
}

// ---------------------------------------------------------------------------
// SEP token resolution
// ---------------------------------------------------------------------------

/// Resolve the SEP token string and vocabulary id from `special_tokens_map.json`.
///
/// The token entry may be a bare string `"<|endoftext|>"` or an object
/// `{"content": "<|endoftext|>", ...}`.  Falls back to id 151643 (Qwen
/// `<|endoftext|>`, documented for pplx-embed-1) when the map lacks a
/// `sep_token` entry.  Errors with guidance if neither resolves.
fn resolve_sep(model_dir: &Path, tokenizer: &Tokenizer) -> Result<(String, i64), EmbedError> {
    let map_path = model_dir.join("special_tokens_map.json");
    let raw = std::fs::read_to_string(&map_path).map_err(EmbedError::Io)?;
    let map: serde_json::Value = serde_json::from_str(&raw)?;

    let sep_str: Option<String> = map.get("sep_token").and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(obj) => obj
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
        _ => None,
    });

    if let Some(s) = sep_str {
        if let Some(id) = tokenizer.token_to_id(&s) {
            return Ok((s, id as i64));
        }
    }

    // Fallback: documented id for pplx-embed-1 (Qwen <|endoftext|>).
    let fallback_id: u32 = 151643;
    match tokenizer.id_to_token(fallback_id) {
        Some(tok) if !tok.is_empty() => Ok((tok, fallback_id as i64)),
        _ => Err(EmbedError::Internal(
            "cannot resolve sep_token: not found in special_tokens_map.json \
             and id 151643 is absent from the tokenizer vocabulary. \
             Verify that all model files downloaded correctly."
                .to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Inference helpers
// ---------------------------------------------------------------------------

fn mean_pool_region(hidden2d: ArrayView2<f32>, start: usize, end: usize) -> Vec<f32> {
    let count = (end - start).max(1) as f32;
    let mut sum = vec![0f32; EMBED_DIM];
    for row_idx in start..end {
        let row = hidden2d.row(row_idx);
        for (i, &v) in row.iter().enumerate() {
            sum[i] += v;
        }
    }
    sum.into_iter().map(|x| x / count).collect()
}

fn apply_quantize_int8_tanh(v: Vec<f32>) -> Vec<f32> {
    v.into_iter()
        .map(|x| (x.tanh() * 127.0).round().clamp(-128.0, 127.0))
        .collect()
}

// ---------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------

/// Local pplx-embed-context-v1-0.6b embedder using late-chunking via ORT.
///
/// Embeds each document's chunks jointly so every chunk vector carries
/// cross-chunk context.  Downloads ~706 MB from a public MIT repo on first
/// use; no API key or HF_TOKEN required.
pub struct PplxContextOnnxEmbedder {
    session: Mutex<ort::session::Session>,
    tokenizer: Tokenizer,
    sep_str: String,
    sep_id: i64,
}

impl PplxContextOnnxEmbedder {
    /// Create a new embedder, downloading model files into `cache_dir` if absent.
    ///
    /// `cache_dir`: parent directory; model stored under
    /// `<cache_dir>/pplx-embed-context-v1-0.6b/`.
    /// `show_progress`: emit download progress via `tracing::info!`.
    pub fn new(cache_dir: Option<PathBuf>, show_progress: bool) -> Result<Self, EmbedError> {
        let model_dir = cache_dir
            .unwrap_or_else(crate::model_cache::ModelCache::default_cache_dir)
            .join(MODEL_DIRNAME);

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

        let (sep_str, sep_id) = resolve_sep(&model_dir, &tokenizer)?;

        let model_onnx = model_dir.join("onnx").join("model_quantized.onnx");
        info!(model = "pplx-embed-context-v1-0.6b", "loading ORT session");
        let session = ort::session::Session::builder()
            .map_err(|e| EmbedError::Internal(format!("ORT SessionBuilder: {e}")))?
            .commit_from_file(&model_onnx)
            .map_err(|e| {
                EmbedError::ModelMissing(format!(
                    "failed to load ONNX model from {}: {e}",
                    model_onnx.display()
                ))
            })?;
        info!(model = "pplx-embed-context-v1-0.6b", "ORT session ready");

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            sep_str,
            sep_id,
        })
    }

    fn embed_doc_sync(&self, chunks: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if chunks.is_empty() {
            return Ok(vec![]);
        }

        // Join all chunks with SEP; single ONNX call encodes the whole document.
        let joined = chunks.join(&self.sep_str);

        let encoding = self
            .tokenizer
            .encode(joined.as_str(), false)
            .map_err(|e| EmbedError::Internal(format!("tokenize: {e}")))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();
        let seq_len = ids.len();

        let ids_arr = Array2::from_shape_vec((1, seq_len), ids.clone())
            .map_err(|e| EmbedError::Internal(format!("shape ids: {e}")))?;
        let mask_arr = Array2::from_shape_vec((1, seq_len), mask.clone())
            .map_err(|e| EmbedError::Internal(format!("shape mask: {e}")))?;

        let ids_tensor = ort::value::Tensor::from_array(ids_arr)
            .map_err(|e| EmbedError::Internal(format!("ids tensor: {e}")))?;
        let mask_tensor = ort::value::Tensor::from_array(mask_arr)
            .map_err(|e| EmbedError::Internal(format!("mask tensor: {e}")))?;

        let mut session_guard = self
            .session
            .lock()
            .map_err(|e| EmbedError::Internal(format!("session mutex poisoned: {e}")))?;

        let outputs = session_guard
            .run(ort::inputs![ids_tensor, mask_tensor])
            .map_err(|e| EmbedError::Internal(format!("ORT run: {e}")))?;

        let hidden: ArrayViewD<f32> = outputs[0]
            .try_extract_array()
            .map_err(|e| EmbedError::Internal(format!("extract hidden state: {e}")))?;

        // Shape: [1, S, 1024] → strip batch dim → [S, 1024].
        let hidden3d = hidden
            .into_dimensionality::<Ix3>()
            .map_err(|e| EmbedError::Internal(format!("expected [B,S,D] tensor: {e}")))?;
        let hidden2d = hidden3d.index_axis(Axis(0), 0);

        // Last valid token position (attention mask == 1).
        let last_valid = mask
            .iter()
            .rposition(|&m| m == 1)
            .map(|i| i + 1)
            .unwrap_or(0);

        // Positions of SEP tokens within the valid range.
        let sep_positions: Vec<usize> = ids[..last_valid]
            .iter()
            .enumerate()
            .filter(|(_, &id)| id == self.sep_id)
            .map(|(i, _)| i)
            .collect();

        // Pool each inter-SEP region → one vector per chunk.
        let mut chunk_embeddings: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
        let mut start = 0usize;

        for &sep_pos in &sep_positions {
            let region = if sep_pos > start {
                mean_pool_region(hidden2d, start, sep_pos)
            } else {
                vec![0f32; EMBED_DIM]
            };
            chunk_embeddings.push(apply_quantize_int8_tanh(region));
            start = sep_pos + 1;
        }
        // Final region (or the only region when there are no SEPs).
        let tail = if last_valid > start {
            mean_pool_region(hidden2d, start, last_valid)
        } else {
            vec![0f32; EMBED_DIM]
        };
        chunk_embeddings.push(apply_quantize_int8_tanh(tail));

        if chunk_embeddings.len() != chunks.len() {
            return Err(EmbedError::Internal(format!(
                "chunk count mismatch: expected {} regions but SEP-split produced {} \
                 (sep_count={}, seq_len={seq_len})",
                chunks.len(),
                chunk_embeddings.len(),
                sep_positions.len(),
            )));
        }

        Ok(chunk_embeddings)
    }
}

#[async_trait]
impl Embedder for PplxContextOnnxEmbedder {
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, CoreError> {
        if docs.is_empty() {
            return Ok(vec![]);
        }

        let mut results = Vec::with_capacity(docs.len());
        for doc in &docs {
            if doc.chunks.is_empty() {
                results.push(vec![]);
                continue;
            }
            let embeddings = tokio::task::block_in_place(|| {
                self.embed_doc_sync(&doc.chunks).map_err(CoreError::from)
            })?;
            results.push(embeddings);
        }
        Ok(results)
    }

    fn embedding_dim(&self) -> usize {
        EMBED_DIM
    }

    fn model_id(&self) -> &str {
        "pplx-embed-context-v1-0.6b"
    }
}
