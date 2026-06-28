//! Local pplx-embed-context-v1-0.6b embedder via in-process CoreML inference.
//!
//! This is the CoreML twin of [`crate::pplx_context_onnx::PplxContextOnnxEmbedder`].
//! It uses the same late-chunking strategy — a document's chunks are joined with
//! the tokenizer's SEP token and embedded jointly so every chunk vector carries
//! cross-chunk context — but the pooling and int8 quantization happen *inside*
//! the CoreML model rather than in post-processing.
//!
//! # Bundle
//!
//! Downloads the `dokterbob/pplx-embed-coreml` bundle (pinned revision) via
//! `hf-hub`, selecting the published *context* buckets. Context buckets are
//! discovered dynamically from `manifest.json`; whichever fixed sizes are
//! present (among the requested `[512, 1024, 2048, 4096]`) plus any dynamic
//! catch-all are used. The largest present fixed bucket bounds the windowing.
//!
//! # CoreML I/O contract (per forward pass, context bucket of seq length L)
//!
//! | Tensor          | Shape     | dtype | Meaning                                   |
//! |-----------------|-----------|-------|-------------------------------------------|
//! | `input_ids`     | `(1, L)`  | int32 | token ids, zero-padded to L               |
//! | `attention_mask`| `(1, L)`  | fp16  | `1.0` for valid tokens, `0` for pad       |
//! | `pool_matrix`   | `(32, L)` | fp16  | row k = `1/n_k` over chunk k's span, else 0 |
//! | `embedding` (out)| `(32, 1024)`| int8 | first `n_chunks` rows valid (pre-pooled + tanh-int8) |

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use half::f16;
use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};
use tracing::info;

use localdb_core::{DocumentChunks, EmbeddedDocument, Embedder, Error as CoreError, TokenCounter};

use crate::coreml::runtime::{ComputeUnits, CoreMlModel, MlArray, Outputs};
use crate::error::EmbedError;
use futures_util::stream::{FuturesUnordered, StreamExt};
use objc2::rc::autoreleasepool;

const HF_REPO: &str = "dokterbob/pplx-embed-coreml";
const HF_REVISION: &str = "53428a93b1c2edea82174c6ec5f28a2b78cf34ec";
const EMBED_DIM: usize = 1024;
const MODEL_ID: &str = "pplx-embed-context-v1-0.6b";
/// `pool_matrix` has exactly 32 rows, so a window may hold at most 32 chunks.
const N_MAX_CHUNKS: usize = 32;
/// Requested fixed bucket sizes; only those actually present are used.
const WANT_BUCKETS: &[i64] = &[512, 1024, 2048, 4096];

// f16 bit pattern for 1.0 (sign=0, exp=01111, mantissa=0) — valid-token mask.
const F16_ONE_BITS: u16 = 0x3C00;

// ---------------------------------------------------------------------------
// Pure build helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Convert an `f32` to its IEEE 754 half-precision bit pattern.
fn f16_bits(x: f32) -> u16 {
    f16::from_f32(x).to_bits()
}

/// Build the `(1, L)` attention mask: first `n` positions are `1.0` (valid),
/// the remaining `L - n` are `0` (pad). Returned as f16 bit patterns.
fn build_attention_mask(n: usize, l: usize) -> Vec<u16> {
    let mut mask = vec![0u16; l];
    for slot in mask.iter_mut().take(n.min(l)) {
        *slot = F16_ONE_BITS;
    }
    mask
}

/// Build the flat `(32, L)` `pool_matrix` as f16 bit patterns.
///
/// For each chunk span `[s, e)` (capped at 32 chunks), row k holds `1/(e-s)`
/// across columns `[s, e)` and `0` elsewhere. Unused rows are all-zero. Spans
/// beyond row 31 or with `e <= s` contribute nothing.
fn build_pool_matrix(spans: &[(usize, usize)], l: usize) -> Vec<u16> {
    let mut m = vec![0u16; N_MAX_CHUNKS * l];
    for (k, &(s, e)) in spans.iter().enumerate().take(N_MAX_CHUNKS) {
        if e <= s {
            continue;
        }
        let inv = 1.0f32 / (e - s) as f32;
        let inv_bits = f16_bits(inv);
        let row = &mut m[k * l..(k + 1) * l];
        for slot in row.iter_mut().take(e.min(l)).skip(s) {
            *slot = inv_bits;
        }
    }
    m
}

/// Pick the fixed bucket index for `token_count`:
/// - smallest fixed bucket whose max seq len `>= token_count`, else
/// - the largest fixed bucket (input is truncated to its max seq len).
///
/// The context model uses fixed ANE buckets only, matching the Swift reference
/// `PplxEmbed.embedContextOne` (whose context path never routes to the dynamic
/// GPU bucket). `fixed_maxseqlens` must be sorted ascending and non-empty.
fn select_bucket(token_count: usize, fixed_maxseqlens: &[usize]) -> usize {
    for (i, &m) in fixed_maxseqlens.iter().enumerate() {
        if m >= token_count {
            return i;
        }
    }
    // Overflow: use the largest fixed bucket (input is truncated to it).
    fixed_maxseqlens.len() - 1
}

/// Convert a row of int8 embedding values to `f32`.
///
/// Used when reading raw int8 rows; the [`crate::coreml::runtime::Outputs`]
/// helper performs the same widening for its flattened reader.
#[cfg_attr(not(test), allow(dead_code))]
fn i8_row_to_f32(row: &[i8]) -> Vec<f32> {
    row.iter().map(|&v| v as f32).collect()
}

/// Partition N chunks into windows that each fit within `max_seq_len` tokens
/// **and** hold at most `max_chunks` chunks (the `pool_matrix` row limit).
///
/// A window of w chunks uses `sum(chunk_tokens[window]) + (w-1)` SEP tokens.
/// A single oversized chunk occupies its own window (truncated at inference).
/// Returns half-open index ranges, one per window, covering `0..N`.
fn plan_windows(
    chunk_tok_counts: &[usize],
    max_seq_len: usize,
    max_chunks: usize,
) -> Vec<Range<usize>> {
    if chunk_tok_counts.is_empty() {
        return vec![];
    }
    let mut windows = Vec::new();
    let mut window_start = 0usize;
    let mut window_tokens = 0usize;

    for (i, &tok_count) in chunk_tok_counts.iter().enumerate() {
        let in_window = i > window_start;
        let sep_cost = if in_window { 1 } else { 0 };
        let added = tok_count + sep_cost;
        let chunk_overflow = in_window && (i - window_start) >= max_chunks;
        let token_overflow = in_window && window_tokens + added > max_seq_len;
        if chunk_overflow || token_overflow {
            windows.push(window_start..i);
            window_start = i;
            window_tokens = tok_count;
        } else {
            window_tokens += added;
        }
    }
    windows.push(window_start..chunk_tok_counts.len());
    windows
}

// ---------------------------------------------------------------------------
// SEP resolution (mirrors pplx_context_onnx::resolve_sep)
// ---------------------------------------------------------------------------

/// Resolve the SEP token string and id from `special_tokens_map.json`,
/// falling back to id 151643 (Qwen `<|endoftext|>`).
fn resolve_sep(hf_model_dir: &Path, tokenizer: &Tokenizer) -> Result<(String, i64), EmbedError> {
    let map_path = hf_model_dir.join("special_tokens_map.json");
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

    let fallback_id: u32 = 151643;
    match tokenizer.id_to_token(fallback_id) {
        Some(tok) if !tok.is_empty() => Ok((tok, fallback_id as i64)),
        _ => Err(EmbedError::Internal(
            "cannot resolve sep_token: not found in special_tokens_map.json \
             and id 151643 is absent from the tokenizer vocabulary."
                .to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Bucket metadata
// ---------------------------------------------------------------------------

/// A present *fixed* context bucket discovered from a downloaded
/// `model_config.json`.
///
/// The context model uses fixed ANE buckets only — matching the Swift reference
/// `PplxEmbed.embedContextOne`, which never routes to the dynamic GPU bucket.
#[derive(Debug, Clone)]
struct BucketInfo {
    /// `.mlmodelc` directory for this bucket.
    mlmodelc: PathBuf,
    /// Maximum sequence length.
    max_seq_len: usize,
    /// Compute units to load this bucket with.
    compute_units: ComputeUnits,
}

#[derive(serde::Deserialize)]
struct ModelConfig {
    #[serde(default)]
    max_seq_len: usize,
    /// Whether this is the dynamic (variable seq-len) bucket; the context model
    /// uses fixed ANE buckets only, so dynamic buckets are skipped.
    #[serde(default)]
    dynamic: bool,
}

/// Discover *fixed* context buckets under `snapshot_root/context/`.
///
/// Reads each bucket dir's `model_config.json`, requiring an `encoder.mlmodelc`
/// and an `hf_model/` directory. Returns the fixed buckets (sorted ascending by
/// max seq len) and the tokenizer's `hf_model/` directory.
///
/// The context model uses fixed ANE buckets only (matching the Swift reference
/// `PplxEmbed.embedContextOne`), so the dynamic GPU catch-all bucket is skipped.
fn discover_buckets(snapshot_root: &Path) -> Result<(Vec<BucketInfo>, PathBuf), EmbedError> {
    let ctx_dir = snapshot_root.join("context");
    let mut fixed: Vec<BucketInfo> = Vec::new();
    let mut first_hf_model: Option<PathBuf> = None;

    let entries = std::fs::read_dir(&ctx_dir).map_err(|e| {
        EmbedError::ModelMissing(format!(
            "no context buckets under {}: {e}",
            ctx_dir.display()
        ))
    })?;

    for entry in entries {
        let dir = entry.map_err(EmbedError::Io)?.path();
        let mlmodelc = dir.join("encoder.mlmodelc");
        let hf_model = dir.join("hf_model");
        let cfg_path = dir.join("model_config.json");
        if !hf_model.is_dir() || !cfg_path.is_file() {
            continue;
        }
        // A bucket shipping only the uncompiled `.mlpackage` cannot be loaded
        // here: on-device compilation of `.mlpackage` is not yet supported.
        if !mlmodelc.is_dir() {
            if dir.join("encoder.mlpackage").is_dir() {
                // TODO(#106): compile .mlpackage on device via MLModel.compileModel
                tracing::warn!(
                    bucket = %dir.display(),
                    "skipping context bucket: only encoder.mlpackage present and \
                     on-device compilation of .mlpackage is not yet supported"
                );
            }
            continue;
        }
        let raw = std::fs::read_to_string(&cfg_path).map_err(EmbedError::Io)?;
        let cfg: ModelConfig = serde_json::from_str(&raw)?;

        // The context model uses fixed ANE buckets only; skip the dynamic bucket.
        if cfg.dynamic {
            continue;
        }

        // Reject a malformed config whose fixed bucket has no usable length.
        if cfg.max_seq_len == 0 {
            tracing::warn!(
                bucket = %dir.display(),
                "skipping context bucket: model_config.json has max_seq_len == 0 (malformed)"
            );
            continue;
        }

        if first_hf_model.is_none() {
            first_hf_model = Some(hf_model.clone());
        }

        fixed.push(BucketInfo {
            mlmodelc,
            max_seq_len: cfg.max_seq_len,
            compute_units: ComputeUnits::All,
        });
    }

    fixed.sort_by_key(|b| b.max_seq_len);

    if fixed.is_empty() {
        return Err(EmbedError::ModelMissing(format!(
            "no usable fixed context buckets found under {}",
            ctx_dir.display()
        )));
    }

    let hf_model = first_hf_model.ok_or_else(|| {
        EmbedError::ModelMissing("context bucket is missing hf_model/ tokenizer dir".to_string())
    })?;

    Ok((fixed, hf_model))
}

// ---------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------

/// Local pplx-embed-context-v1-0.6b embedder using late-chunking via CoreML.
pub struct PplxContextCoreMLEmbedder {
    tokenizer: Tokenizer,
    sep_str: String,
    sep_id: i64,
    /// Fixed buckets, sorted ascending by max seq len.
    fixed: Vec<BucketInfo>,
    /// Largest sequence length any window may span (bounds `plan_windows`).
    window_max_seq_len: usize,
    /// Lazily-loaded models keyed by bucket max seq len.
    models: Mutex<HashMap<usize, Arc<CoreMlModel>>>,
}

impl PplxContextCoreMLEmbedder {
    /// Download the CoreML bundle (if absent) and build the embedder.
    ///
    /// `cache_dir`: optional HF cache directory; `None` uses the default.
    /// `show_progress`: reserved for future progress wiring.
    pub fn new(cache_dir: Option<PathBuf>, show_progress: bool) -> Result<Self, EmbedError> {
        let snapshot_root = download_bundle_blocking(cache_dir, show_progress)?;
        let (fixed, hf_model_dir) = discover_buckets(&snapshot_root)?;

        // The largest present fixed bucket bounds the window (context = fixed
        // buckets only, matching the Swift reference).
        let window_max_seq_len = fixed.last().map(|b| b.max_seq_len).ok_or_else(|| {
            EmbedError::ModelMissing("no fixed context buckets available".to_string())
        })?;

        let tokenizer_path = hf_model_dir.join("tokenizer.json");
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            EmbedError::ModelMissing(format!(
                "failed to load tokenizer.json from {}: {e}",
                tokenizer_path.display()
            ))
        })?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: window_max_seq_len,
                strategy: TruncationStrategy::LongestFirst,
                stride: 0,
                direction: TruncationDirection::Right,
            }))
            .map_err(|e| EmbedError::Internal(format!("configure tokenizer truncation: {e}")))?;

        let (sep_str, sep_id) = resolve_sep(&hf_model_dir, &tokenizer)?;

        info!(
            model = MODEL_ID,
            fixed_buckets = fixed.len(),
            window_max_seq_len,
            "CoreML context embedder ready"
        );

        Ok(Self {
            tokenizer,
            sep_str,
            sep_id,
            fixed,
            window_max_seq_len,
            models: Mutex::new(HashMap::new()),
        })
    }

    /// Get (loading lazily) the model for a fixed bucket index.
    fn model_for(&self, fixed_idx: usize) -> Result<Arc<CoreMlModel>, EmbedError> {
        let bucket = &self.fixed[fixed_idx];
        let key = bucket.max_seq_len;

        let mut guard = self
            .models
            .lock()
            .map_err(|e| EmbedError::Internal(format!("models mutex poisoned: {e}")))?;
        if let Some(m) = guard.get(&key) {
            return Ok(m.clone());
        }
        info!(
            bucket_max_seq_len = bucket.max_seq_len,
            "loading CoreML bucket"
        );
        let model = Arc::new(CoreMlModel::load(&bucket.mlmodelc, bucket.compute_units)?);
        guard.insert(key, model.clone());
        Ok(model)
    }

    /// Tokenize `chunks`, select a CoreML bucket, and build the three input arrays.
    ///
    /// This is the CPU-side preparation for one window: it joins the chunks with
    /// the SEP token, tokenizes, selects the appropriate fixed bucket, recovers
    /// the SEP-delimited spans, and builds `input_ids`, `attention_mask`, and
    /// `pool_matrix` arrays ready for `CoreMlModel::start_prediction`.
    fn prep_window(&self, chunks: &[String]) -> Result<PreppedWindow, EmbedError> {
        debug_assert!(
            !chunks.is_empty(),
            "prep_window called with empty chunk slice"
        );

        // Join all chunks with SEP; tokenize once (no special tokens).
        let joined = chunks.join(&self.sep_str);
        let encoding = self
            .tokenizer
            .encode(joined.as_str(), false)
            .map_err(|e| EmbedError::Internal(format!("tokenize: {e}")))?;
        let ids: Vec<u32> = encoding.get_ids().to_vec();
        let token_count = ids.len();

        // Pick the fixed bucket for this token count (context = fixed ANE
        // buckets only, matching the Swift reference `embedContextOne`).
        let fixed_lens: Vec<usize> = self.fixed.iter().map(|b| b.max_seq_len).collect();
        let sel = select_bucket(token_count, &fixed_lens);
        let bucket_len = self.fixed[sel].max_seq_len;

        // Truncate to the bucket's sequence length.
        let valid = token_count.min(bucket_len);
        let ids = &ids[..valid];

        // Recover SEP spans within the valid range: [start, sep_pos), next
        // start = sep+1, final to last_valid. Cap at 32 spans.
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(chunks.len());
        let mut start = 0usize;
        for (i, &id) in ids.iter().enumerate() {
            if id as i64 == self.sep_id {
                spans.push((start, i));
                start = i + 1;
            }
        }
        spans.push((start, valid));
        if spans.len() > N_MAX_CHUNKS {
            spans.truncate(N_MAX_CHUNKS);
        }
        let n_chunks = spans.len();

        if n_chunks != chunks.len() {
            return Err(EmbedError::Internal(format!(
                "chunk count mismatch in prep_window: expected {} spans but produced {} \
                 (sep_spans={n_chunks}, token_count={token_count})",
                chunks.len(),
                n_chunks,
            )));
        }

        // Build the three inputs, padded to the bucket sequence length.
        let mut input_ids = vec![0i32; bucket_len];
        for (slot, &id) in input_ids.iter_mut().zip(ids.iter()) {
            *slot = id as i32;
        }
        let attention_mask = build_attention_mask(valid, bucket_len);
        let pool_matrix = build_pool_matrix(&spans, bucket_len);

        // CoreML validates against rank-2 shapes.
        let ids_arr = MlArray::int32(&[1, bucket_len], &input_ids)?;
        let mask_arr = MlArray::f16(&[1, bucket_len], &bits_to_f16(&attention_mask))?;
        let pool_arr = MlArray::f16(&[N_MAX_CHUNKS, bucket_len], &bits_to_f16(&pool_matrix))?;

        let inputs: Vec<(&'static str, MlArray)> = vec![
            ("input_ids", ids_arr),
            ("attention_mask", mask_arr),
            ("pool_matrix", pool_arr),
        ];

        Ok(PreppedWindow {
            fixed_idx: sel,
            inputs,
            n_chunks,
        })
    }
}

/// Convert a slice of f16 bit patterns into `half::f16` values for `MlArray::f16`.
fn bits_to_f16(bits: &[u16]) -> Vec<f16> {
    bits.iter().map(|&b| f16::from_bits(b)).collect()
}

/// Extract per-chunk embedding rows from a CoreML output.
///
/// The model outputs `(32, 1024)` int8; reads the first `n_chunks` rows and
/// converts each to `f32`, returning one `Vec<f32>` per chunk.
fn rows_from_output(out: &Outputs, n_chunks: usize) -> Result<Vec<Vec<f32>>, EmbedError> {
    let flat = out.int8_as_f32("embedding")?;
    if flat.len() < n_chunks * EMBED_DIM {
        return Err(EmbedError::Internal(format!(
            "embedding output too small: got {} values, need at least {} (n_chunks={n_chunks})",
            flat.len(),
            n_chunks * EMBED_DIM
        )));
    }
    debug_assert!(flat.len() >= n_chunks * EMBED_DIM);
    let mut rows = Vec::with_capacity(n_chunks);
    for row in 0..n_chunks {
        rows.push(flat[row * EMBED_DIM..(row + 1) * EMBED_DIM].to_vec());
    }
    Ok(rows)
}

/// CPU-side prepared inputs for one window: tokenized, bucket-selected, arrays built.
/// Returned by `prep_window`; passed to `CoreMlModel::start_prediction`.
struct PreppedWindow {
    /// Index into `self.fixed` for the selected bucket.
    fixed_idx: usize,
    /// The three model inputs (input_ids, attention_mask, pool_matrix) as owned MlArrays.
    inputs: Vec<(&'static str, MlArray)>,
    /// Number of valid embedding rows in the output (= number of chunk spans found).
    n_chunks: usize,
}

/// Blocking bridge to the async bundle download.
fn download_bundle_blocking(
    cache_dir: Option<PathBuf>,
    show_progress: bool,
) -> Result<PathBuf, EmbedError> {
    let fut = crate::coreml::download::download_bundle(
        HF_REPO,
        HF_REVISION,
        WANT_BUCKETS,
        cache_dir,
        show_progress,
    );

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(fut))
        }
        Ok(_) => {
            // current-thread runtime: spawn a dedicated thread with its own RT.
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| EmbedError::Internal(format!("create tokio runtime: {e}")))?
                    .block_on(fut)
            })
            .join()
            .map_err(|_| EmbedError::Internal("download thread panicked".into()))?
        }
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| EmbedError::Internal(format!("create tokio runtime: {e}")))?
            .block_on(fut),
    }
}

#[async_trait]
impl Embedder for PplxContextCoreMLEmbedder {
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, CoreError> {
        if docs.is_empty() {
            return Ok(vec![]);
        }

        // Pre-allocate result slots: results[doc_idx][chunk_idx] = embedding vector.
        let mut results: Vec<Vec<Vec<f32>>> =
            docs.iter().map(|d| vec![vec![]; d.chunks.len()]).collect();

        // Build worklist: plan windows for each doc, collect (doc_idx, chunk_start, window_chunks).
        // Tokenize each chunk to get token counts for plan_windows.
        struct WorkItem {
            doc_idx: usize,
            chunk_start: usize,
            chunks: Vec<String>,
        }
        let mut worklist: Vec<WorkItem> = Vec::new();
        for (doc_idx, doc) in docs.iter().enumerate() {
            if doc.chunks.is_empty() {
                continue;
            }
            let chunk_tok_counts = doc
                .chunks
                .iter()
                .map(|c| {
                    self.tokenizer
                        .encode(c.as_str(), false)
                        .map(|enc| enc.get_ids().len())
                        .map_err(|e| EmbedError::Internal(format!("tokenize: {e}")))
                })
                .collect::<Result<Vec<usize>, EmbedError>>()
                .map_err(CoreError::from)?;

            let windows = plan_windows(&chunk_tok_counts, self.window_max_seq_len, N_MAX_CHUNKS);
            for window in &windows {
                worklist.push(WorkItem {
                    doc_idx,
                    chunk_start: window.start,
                    chunks: doc.chunks[window.clone()].to_vec(),
                });
            }
        }

        // Bounded-in-flight async pipeline: prep CPU work (tokenize + build arrays) while
        // up to MAX_INFLIGHT-1 prior predictions run on the ANE. Completions arrive out of
        // order; doc_idx+chunk_start scatter is order-independent — each window knows exactly
        // which result slots to fill.
        //
        // Error discipline: we never early-return while `inflight` holds live
        // `PendingPrediction` futures.  CoreML reads the input feature provider after
        // `predictionFromFeatures_completionHandler` returns; dropping the provider while
        // the ANE is still computing would be a use-after-free.  On any error we record
        // it, stop enqueuing new work, and drain all in-flight futures before returning.
        const MAX_INFLIGHT: usize = 4;
        let mut inflight: FuturesUnordered<_> = FuturesUnordered::new();
        let mut it = worklist.into_iter();
        let mut first_err: Option<CoreError> = None;

        loop {
            // Fill up to MAX_INFLIGHT in-flight predictions (skip if an error was recorded).
            if first_err.is_none() {
                while inflight.len() < MAX_INFLIGHT {
                    match it.next() {
                        Some(w) => {
                            // Drain per-window autoreleased Obj-C temporaries (NSString,
                            // NSNumber, MLFeatureValue, transient MLMultiArrays) that
                            // accumulate in prep_window / build_feature_provider /
                            // start_prediction.  PendingPrediction holds only Retained<>
                            // objects, so it survives the pool drain.
                            let window_result = autoreleasepool(|_| -> Result<_, CoreError> {
                                let p = self.prep_window(&w.chunks).map_err(CoreError::from)?;
                                let fixed_idx = p.fixed_idx;
                                let n_chunks = p.n_chunks;
                                let doc_idx = w.doc_idx;
                                let chunk_start = w.chunk_start;
                                let model = self.model_for(fixed_idx).map_err(CoreError::from)?;
                                let pending =
                                    model.start_prediction(p.inputs).map_err(CoreError::from)?;
                                Ok((doc_idx, chunk_start, n_chunks, pending))
                            });
                            match window_result {
                                Ok((doc_idx, chunk_start, n_chunks, pending)) => {
                                    // Each future carries its own index tag so the
                                    // scatter below is order-independent.
                                    inflight.push(async move {
                                        (doc_idx, chunk_start, n_chunks, pending.await)
                                    });
                                }
                                Err(e) => {
                                    first_err = Some(e);
                                    break;
                                }
                            }
                        }
                        None => break,
                    }
                }
            }

            match inflight.next().await {
                Some((doc_idx, chunk_start, n_chunks, out)) => {
                    if first_err.is_none() {
                        // Drain per-completion autoreleased temporaries (NSString /
                        // MLFeatureValue / transient MLMultiArray) created while
                        // reading the output feature provider.
                        let rows_result = autoreleasepool(|_| {
                            out.map_err(CoreError::from).and_then(|o| {
                                rows_from_output(&o, n_chunks).map_err(CoreError::from)
                            })
                        });
                        match rows_result {
                            Ok(rows) => {
                                for (j, v) in rows.into_iter().enumerate() {
                                    results[doc_idx][chunk_start + j] = v;
                                }
                            }
                            Err(e) => {
                                first_err = Some(e);
                            }
                        }
                    }
                    // else: drain silently — keep awaiting until inflight empties.
                }
                None => break, // worklist drained and all in-flight predictions settled
            }
        }

        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(results)
    }

    fn embedding_dim(&self) -> usize {
        EMBED_DIM
    }

    fn model_id(&self) -> &str {
        MODEL_ID
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- f16_bits ----

    #[test]
    fn f16_bits_known_values() {
        assert_eq!(f16_bits(1.0), 0x3C00);
        assert_eq!(f16_bits(0.5), 0x3800);
        assert_eq!(f16_bits(0.25), 0x3400);
        assert_eq!(f16_bits(0.0), 0x0000);
    }

    // ---- build_attention_mask ----

    #[test]
    fn attention_mask_length_and_values() {
        let m = build_attention_mask(3, 5);
        assert_eq!(m.len(), 5);
        assert_eq!(m, vec![0x3C00, 0x3C00, 0x3C00, 0, 0]);
    }

    #[test]
    fn attention_mask_full() {
        let m = build_attention_mask(4, 4);
        assert_eq!(m, vec![0x3C00; 4]);
    }

    #[test]
    fn attention_mask_n_exceeds_l_is_clamped() {
        let m = build_attention_mask(10, 3);
        assert_eq!(m, vec![0x3C00; 3]);
    }

    // ---- build_pool_matrix ----

    #[test]
    fn pool_matrix_row_values() {
        // Two spans: [0,2) -> 1/2, [2,3) -> 1/1, over L=4.
        let l = 4;
        let m = build_pool_matrix(&[(0, 2), (2, 3)], l);
        assert_eq!(m.len(), N_MAX_CHUNKS * l);
        let half = f16_bits(0.5);
        let one = f16_bits(1.0);
        // Row 0: 1/2 at cols 0,1; 0 elsewhere.
        assert_eq!(&m[0..l], &[half, half, 0, 0]);
        // Row 1: 1/1 at col 2; 0 elsewhere.
        assert_eq!(&m[l..2 * l], &[0, 0, one, 0]);
        // Row 2 (unused) all zero.
        assert_eq!(&m[2 * l..3 * l], &[0, 0, 0, 0]);
    }

    #[test]
    fn pool_matrix_unused_rows_zero() {
        let l = 3;
        let m = build_pool_matrix(&[(0, 1)], l);
        // Every row past 0 is all-zero.
        assert!(m[l..].iter().all(|&x| x == 0));
    }

    #[test]
    fn pool_matrix_spans_over_32_ignored() {
        let l = 2;
        // 40 spans each [i*?]; just construct 40 unit spans of width 1.
        let spans: Vec<(usize, usize)> = (0..40).map(|_| (0, 1)).collect();
        let m = build_pool_matrix(&spans, l);
        assert_eq!(m.len(), N_MAX_CHUNKS * l);
        // Row 31 is written, rows beyond don't exist (matrix capped at 32).
        let one = f16_bits(1.0);
        assert_eq!(m[31 * l], one);
    }

    #[test]
    fn pool_matrix_empty_span_skipped() {
        let l = 3;
        let m = build_pool_matrix(&[(1, 1)], l);
        assert!(m.iter().all(|&x| x == 0));
    }

    // ---- select_bucket ----

    #[test]
    fn select_bucket_smallest_fits() {
        let fixed = [512, 1024, 2048, 4096];
        assert_eq!(select_bucket(300, &fixed), 0);
        assert_eq!(select_bucket(512, &fixed), 0);
        assert_eq!(select_bucket(513, &fixed), 1);
        assert_eq!(select_bucket(4096, &fixed), 3);
    }

    #[test]
    fn select_bucket_exact_boundary_interior() {
        // token_count exactly == an interior bucket's maxSeqLen selects that
        // bucket (not the next one up): the `>=` comparison is inclusive.
        let fixed = [512, 1024, 2048, 4096];
        assert_eq!(select_bucket(1024, &fixed), 1);
        assert_eq!(select_bucket(2048, &fixed), 2);
        // One past the boundary rolls to the next bucket.
        assert_eq!(select_bucket(1025, &fixed), 2);
    }

    #[test]
    fn select_bucket_overflow_uses_largest_fixed() {
        // Context path uses fixed buckets only; overflow truncates to the
        // largest fixed bucket (no dynamic routing).
        let fixed = [512, 1024];
        assert_eq!(select_bucket(9000, &fixed), 1);
        let single = [512];
        assert_eq!(select_bucket(9000, &single), 0);
    }

    // ---- i8_row_to_f32 ----

    #[test]
    fn i8_row_to_f32_converts() {
        let row: [i8; 4] = [0, 127, -128, -1];
        assert_eq!(i8_row_to_f32(&row), vec![0.0, 127.0, -128.0, -1.0]);
    }

    // ---- plan_windows (incl. 32-chunk cap) ----

    #[test]
    fn plan_windows_empty() {
        assert!(plan_windows(&[], 8192, N_MAX_CHUNKS).is_empty());
    }

    #[test]
    fn plan_windows_single_fits() {
        assert_eq!(plan_windows(&[100], 8192, N_MAX_CHUNKS), vec![0..1]);
    }

    #[test]
    fn plan_windows_single_oversized() {
        assert_eq!(plan_windows(&[10_000], 8192, N_MAX_CHUNKS), vec![0..1]);
    }

    #[test]
    fn plan_windows_all_fit() {
        assert_eq!(
            plan_windows(&[100, 100, 100], 8192, N_MAX_CHUNKS),
            vec![0..3]
        );
    }

    #[test]
    fn plan_windows_splits_at_token_boundary() {
        // 4096 + 4096 + 1 SEP = 8193 > 8192 -> two windows.
        assert_eq!(
            plan_windows(&[4096, 4096], 8192, N_MAX_CHUNKS),
            vec![0..1, 1..2]
        );
    }

    #[test]
    fn plan_windows_exact_fit() {
        // 4095 + 4096 + 1 SEP = 8192 -> one window.
        assert_eq!(plan_windows(&[4095, 4096], 8192, N_MAX_CHUNKS), vec![0..2]);
    }

    #[test]
    fn plan_windows_32_chunk_cap() {
        // 40 tiny chunks (5 tokens each), max_seq_len plenty -> token never the
        // limiter, but the 32-chunk cap forces at least two windows.
        let counts = vec![5usize; 40];
        let windows = plan_windows(&counts, 1000, N_MAX_CHUNKS);
        assert!(windows.len() >= 2, "expected >= 2 windows, got {windows:?}");
        for w in &windows {
            assert!(w.len() <= N_MAX_CHUNKS, "window {w:?} exceeds 32-chunk cap");
        }
        // First window holds exactly 32.
        assert_eq!(windows[0], 0..32);
    }

    #[test]
    fn plan_windows_exactly_32_one_window() {
        let counts = vec![1usize; 32];
        let windows = plan_windows(&counts, 1000, N_MAX_CHUNKS);
        assert_eq!(windows, vec![0..32]);
    }

    #[test]
    fn plan_windows_covers_all_chunks_no_gaps() {
        let counts = vec![5usize; 100];
        let windows = plan_windows(&counts, 1000, N_MAX_CHUNKS);
        let mut covered = vec![false; counts.len()];
        for w in &windows {
            assert!(w.len() <= N_MAX_CHUNKS);
            for i in w.clone() {
                assert!(!covered[i], "chunk {i} covered twice");
                covered[i] = true;
            }
        }
        assert!(covered.iter().all(|&c| c), "not all chunks covered");
    }

    #[test]
    fn bits_to_f16_roundtrips() {
        let bits = vec![0x3C00u16, 0x0000, 0x3800];
        let f = bits_to_f16(&bits);
        assert_eq!(f[0].to_f32(), 1.0);
        assert_eq!(f[1].to_f32(), 0.0);
        assert_eq!(f[2].to_f32(), 0.5);
    }

    // ---- discover_buckets (FIX 4 hardening) ----
    //
    // `discover_buckets` only reads JSON + checks directory existence, so it is
    // device-free: no model load and no tokenizer are required.

    /// Lay out one context bucket dir under `root/context/<name>/` with a
    /// `model_config.json` (raw JSON body), an `hf_model/` dir, and — when
    /// `with_mlmodelc` — an `encoder.mlmodelc/` dir. Returns the bucket dir.
    fn make_bucket(root: &Path, name: &str, config_json: &str, with_mlmodelc: bool) -> PathBuf {
        let dir = root.join("context").join(name);
        std::fs::create_dir_all(dir.join("hf_model")).unwrap();
        if with_mlmodelc {
            std::fs::create_dir_all(dir.join("encoder.mlmodelc")).unwrap();
        }
        std::fs::write(dir.join("model_config.json"), config_json).unwrap();
        dir
    }

    #[test]
    fn discover_skips_zero_max_seq_len() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        // Malformed fixed bucket: max_seq_len == 0 (has a valid .mlmodelc).
        make_bucket(
            root,
            "Lbad-int8",
            r#"{"max_seq_len":0,"bucket":0,"variant":"context"}"#,
            true,
        );
        // Valid fixed bucket.
        make_bucket(
            root,
            "L512-int8",
            r#"{"max_seq_len":512,"bucket":0,"variant":"context"}"#,
            true,
        );

        let (fixed, _hf) = discover_buckets(root).unwrap();
        let lens: Vec<usize> = fixed.iter().map(|b| b.max_seq_len).collect();
        assert!(
            !lens.contains(&0),
            "zero-max_seq_len bucket must be skipped, got {lens:?}"
        );
        assert!(
            lens.contains(&512),
            "valid 512 bucket must be kept, got {lens:?}"
        );
    }

    #[test]
    fn discover_skips_mlpackage_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        // Bucket shipping only encoder.mlpackage (no .mlmodelc) must be skipped.
        let pkg_dir = make_bucket(
            root,
            "L1024-int8",
            r#"{"max_seq_len":1024,"bucket":0,"variant":"context"}"#,
            false,
        );
        std::fs::create_dir_all(pkg_dir.join("encoder.mlpackage")).unwrap();

        // Sibling with a compiled .mlmodelc must be kept.
        make_bucket(
            root,
            "L512-int8",
            r#"{"max_seq_len":512,"bucket":0,"variant":"context"}"#,
            true,
        );

        let (fixed, _hf) = discover_buckets(root).unwrap();
        let lens: Vec<usize> = fixed.iter().map(|b| b.max_seq_len).collect();
        assert!(
            !lens.contains(&1024),
            ".mlpackage-only bucket must be skipped, got {lens:?}"
        );
        assert!(
            lens.contains(&512),
            ".mlmodelc bucket must be kept, got {lens:?}"
        );
    }

    // ---- Scatter / worklist regression guards ----
    //
    // These tests verify that the `embed_documents` pipeline correctly maps
    // (doc_idx, chunk_start) indices and that out-of-order completion still
    // places every vector in the right slot.  They run without any model or
    // Obj-C runtime.

    /// Simulate the scatter step from `embed_documents` with completions
    /// arriving out of order, and verify every slot receives the correct vector.
    #[test]
    fn scatter_out_of_order_places_vectors_correctly() {
        // Two docs: doc 0 has 2 chunks, doc 1 has 3 chunks.
        let mut results: Vec<Vec<Vec<f32>>> = vec![vec![vec![]; 2], vec![vec![]; 3]];

        // Windows (simulated, could arrive in any order):
        //   (doc=1, chunk_start=0, n_chunks=3) arrives first
        //   (doc=0, chunk_start=0, n_chunks=2) arrives second

        // Scatter doc 1, window 0..3
        let rows1 = vec![vec![1.0f32; 4], vec![2.0; 4], vec![3.0; 4]];
        for (j, v) in rows1.into_iter().enumerate() {
            results[1][j] = v;
        }
        // Scatter doc 0, window 0..2
        let rows0 = vec![vec![10.0f32; 4], vec![20.0; 4]];
        for (j, v) in rows0.into_iter().enumerate() {
            results[0][j] = v;
        }

        assert_eq!(results[0][0], vec![10.0f32; 4]);
        assert_eq!(results[0][1], vec![20.0f32; 4]);
        assert_eq!(results[1][0], vec![1.0f32; 4]);
        assert_eq!(results[1][1], vec![2.0f32; 4]);
        assert_eq!(results[1][2], vec![3.0f32; 4]);
    }

    /// Scatter with a doc that spans two windows (chunk_start > 0 for the
    /// second window) must place vectors at the correct offsets.
    #[test]
    fn scatter_second_window_uses_chunk_start_offset() {
        // One doc, 40 chunks split across two windows: 0..32 and 32..40.
        let mut results: Vec<Vec<Vec<f32>>> = vec![vec![vec![]; 40]];

        // Second window (32..40) arrives first.
        for j in 0..8usize {
            results[0][32 + j] = vec![(32 + j) as f32; 4];
        }
        // First window (0..32) arrives second.
        for (j, slot) in results[0].iter_mut().enumerate().take(32) {
            *slot = vec![j as f32; 4];
        }

        for (i, slot) in results[0].iter().enumerate() {
            assert_eq!(*slot, vec![i as f32; 4], "slot {i} wrong");
        }
    }

    // ---- Autorelease pool soak (Apple Silicon only, #[ignore]) ----
    //
    // Exercises the per-window and per-completion autoreleasepool drain with the
    // real CoreML model.  Without the fix, IOSurface handles accumulate and the
    // process eventually aborts with an NSGenericException; with the fix the
    // live-handle count stays flat across all windows.
    //
    // Run manually: cargo test -p localdb-embed -- --ignored
    // Skipped in CI (no Apple Silicon / no model bundle).
    #[test]
    #[ignore = "requires Apple Silicon and the downloaded model bundle (~706 MB)"]
    fn autorelease_pool_soak_many_windows() {
        let embedder = match PplxContextCoreMLEmbedder::new(None, false) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skip: model unavailable ({e})");
                return;
            }
        };
        // 300 short chunks → ~10 windows of 32 chunks (N_MAX_CHUNKS cap).
        let chunks: Vec<String> = (0..300)
            .map(|i| format!("chunk {i} the quick brown fox jumps over the lazy dog"))
            .collect();
        let docs = vec![DocumentChunks {
            document_context: String::new(),
            chunks,
        }];
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            embedder
                .embed_documents(docs)
                .await
                .expect("embedding must complete without aborting or erroring");
        });
    }

    /// Verify that `plan_windows` produces the expected (chunk_start) values
    /// for a multi-doc scenario, so worklist construction is correct.
    #[test]
    fn worklist_chunk_start_mapping_multi_doc() {
        // doc 0: 3 tiny chunks → one window starting at 0
        let doc0 = vec![5usize; 3];
        let w0 = plan_windows(&doc0, 8192, N_MAX_CHUNKS);
        assert_eq!(w0, vec![0..3]);

        // doc 1: 40 tiny chunks → two windows (32-chunk cap)
        let doc1 = vec![5usize; 40];
        let w1 = plan_windows(&doc1, 8192, N_MAX_CHUNKS);
        assert_eq!(w1.len(), 2);
        assert_eq!(w1[0].start, 0);
        assert_eq!(w1[1].start, 32);

        // Verify worklist would be:
        //   (doc=0, chunk_start=0), (doc=1, chunk_start=0), (doc=1, chunk_start=32)
        let expected: Vec<(usize, usize)> = vec![(0, 0), (1, 0), (1, 32)];
        let mut actual: Vec<(usize, usize)> = Vec::new();
        for window in &w0 {
            actual.push((0, window.start));
        }
        for window in &w1 {
            actual.push((1, window.start));
        }
        assert_eq!(actual, expected);
    }
}
