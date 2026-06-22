# Spec 04 — Ingestion & Retrieval Pipeline

> Status: accepted draft, 2026-06-10.

Pipeline: **acquire → extract → markdown → chunks → embed → index** (write side), and
**query → BM25 + dense → RRF fuse → filter → citations** (read side).

## 1. Acquisition

- **Files (`path` sources):** embedded mode scans on demand (`localdb index`); the daemon
  additionally watches continuously via `notify` (FSEvents on macOS, inotify on Linux), with
  debounce so editor save-storms coalesce. Include/exclude globs from the source spec.
- **URLs (`url` sources):** HTTP fetch + readability-style main-content extraction; refresh on
  the configured interval (daemon) or on explicit `localdb index` (embedded). Conditional GET
  (ETag/Last-Modified) when available.
- **Incremental re-index:** a document is re-processed only when its `content_hash` changes.
  Unchanged → skip; changed → replace-by-URI: delete the old document's chunks, insert the new
  ones (new content-addressed IDs, [02-domain-model.md](02-domain-model.md) §3).
- **Deletes:** file deleted / URL gone (404·410 after retry) / source removed → delete its
  documents and chunks from the backend. Deletes are data-modifying: ≥90% coverage gate
  ([01-architecture.md](01-architecture.md) §7).

## 2. Extraction (v1 matrix)

**Decision:** v1 extracts **Markdown, plain text, HTML, and text-layer PDF** into a normalized
Markdown string plus `DocumentMetadata` extracted from frontmatter (Dublin Core fields).

| Format | Approach | Notes |
|---|---|---|
| Markdown | pulldown-cmark parser | Passed through as-is after normalization; headings and code fences preserved as Markdown. |
| Plain text | direct | Rendered as plain Markdown paragraphs. |
| HTML | readability-style main-content + DOM walk | Used for both `url` fetches and `.html` files; converted to Markdown. |
| PDF (text layer) | Rust PDF text extraction | Converted to Markdown; page structure preserved where detectable. |

**Out of scope for v1 (explicit):** OCR, scanned PDFs, DOCX/PPTX/XLSX, images. Rationale: no
single mature Rust extraction stack covers these well; shipping a sharp matrix
beats shipping a ragged one. Unsupported files are skipped and counted in IndexJob stats, not
errors. Roadmap: [06-roadmap.md](06-roadmap.md) §5.

**Extension-gated acceptance:** The `PlaintextParser` (and by extension the full parser chain)
only accepts files whose extension or basename matches the list published by
`extract::supported_extensions()` (text and code/data extensions, plus known lockfile
basenames such as `Cargo.lock`).  Files with unknown or binary extensions — `.exe`, `.png`,
`.bin`, etc. — are declined at the parser level and counted as `unsupported_format` in
`IndexJobStats` without ever entering the chunker or embedder.  This prevents indexing
hangs caused by chunkers receiving arbitrarily large binary blobs.

**Default include allowlist for directory sources:** When a `path` source points to a
directory and no explicit `include` globs have been set, `cli` automatically applies
`DEFAULT_PATH_INCLUDES` — a glob list derived from `extract::supported_extensions()` —
so that file-system enumeration skips unsupported files before they ever reach the
extraction layer.  Single-file sources are not affected (they carry an exact filename
glob).  Sources added via explicit `include` override this default entirely.

**Three-way per-document classification:**

| Outcome | Error variant | Counter | Behavior |
|---|---|---|---|
| Format not handled (e.g. scanned PDF, binary `.html`) | `UnsupportedFormat` | `unsupported_format_count` | Silent; no WARN. |
| Supported format, broken instance (e.g. corrupt DOCX) | `ExtractionFailed` | `error_count` | WARN logged per file; counted as failure. |
| Unexpected panic in parser/chunker | `Internal` (via `catch_unwind`) | `error_count` | WARN logged per file; counted as failure. |

In all three cases the ingestion loop continues with the next file; the process does **not** abort.

**`--strict` opt-in:** by default `index` is best-effort (exits `0` regardless of per-file
failures). Pass `--strict` to exit `2` after the run completes when `error_count > 0`. Unsupported
files do not trigger `--strict`; only `ExtractionFailed` / `Internal` errors do.

**Binary / non-UTF-8 input:** All parser implementations (`MarkdownParser`, `HtmlParser`,
`PlaintextParser`) decline non-UTF-8 bytes by returning `Ok(None)` rather than
`Err(InvalidRequest)`. A file with a recognized extension that contains binary or
mis-encoded bytes therefore falls through the entire parser chain and is counted as
`unsupported_format` in `IndexJobStats`, not as an error.

**Per-document panic isolation:** `index_document` wraps the synchronous extraction and
chunking calls in `std::panic::catch_unwind`. Any unexpected panic in a parser or chunker
is caught, converted to `Err(Error::Internal)`, logged as a per-file WARN, and counted in
`error_count`. The ingestion loop continues with the next file; the process does not abort.

## 3. Chunking

Chunking is half of the per-store `indexing` policy ([03-config.md](03-config.md) §2).
Per-source-kind presets, applied per source:

| Preset | Strategy | Defaults |
|---|---|---|
| `prose` (default) | `MarkdownSplitter` (`benbrandt/text-splitter`) on Markdown semantic boundaries — headings, code blocks, lists, paragraphs, sentences, words; preserves document order; `heading_path` attributed per chunk | target ≈ 256 tokens, overlap ≈ 0 tokens |
| `messages` (reserved for connectors) | Thread/turn windows: N consecutive messages per chunk, sliding | window 6 turns, stride 3 |
| `code` | Structural (function/item-level) where parseable, else line blocks | target ≈ 60 lines |

**Per-file auto-selection of chunk preset:** Rather than applying one preset uniformly
across all files in a source, `index_document` in `core` selects the chunk preset
**per file** based on the detected file type:

- Markdown, HTML, PDF, and plain-text prose files → `prose` preset (`MarkdownSplitter`).
- Code, data, and lockfiles (`.rs`, `.py`, `.json`, `.toml`, `Cargo.lock`, etc.) → `code` preset.

The source-level preset (from config or the `--preset` CLI flag) acts as a default/override:
if it is explicitly set to something other than `prose`, the source preset takes precedence.
This per-file routing avoids feeding large minified JSON or source files into the prose
splitter (which can hang on structureless, line-free content).

**`chunk_prose` structureless fallback:** When `MarkdownSplitter` produces a single chunk
covering the whole document (a sign that the content lacks Markdown structure), `chunk_prose`
falls back to the `code` chunker so that the file is still indexed in bounded chunks.

**`chunk_code` long-line split:** `chunk_code` enforces a per-line byte limit. Lines
exceeding the limit are split into fixed-width sub-segments before chunking, preventing
single-line binary or minified content from producing unbounded chunk sizes.

Chunk sizing for `prose` is **token-accurate**, measured using the embedding model's own tokenizer
(the default model `pplx-embed-context-v1-0.6b` supports up to 32K tokens; localdb caps its
late-chunking window at 4096 tokens = 16 × 256-token chunks). When no local
tokenizer is available (e.g. hosted/API embedders), it falls back to a character approximation
(~4 chars/token). The 256-token / 0-overlap defaults mirror the contextual late-chunking
model's training regime: Perplexity's contextualized-embeddings model is trained on documents
partitioned into 256-token chunks (16 chunks per 4096-token document) with **no** intra-document
overlap, because late chunking shares context across chunks from the same document (chunks must be
sent in source-document order) and so supplies cross-chunk context itself. Aligning the chunker to
that regime gives smaller, precise chunks — better citation granularity — while the model handles
cross-chunk context, with no overlap needed. These are defaults to beat with evaluation, not dogma.

## 4. Embedding

### Document-aware interface (day one)

**Decision:** the `Embedder` trait in `core` receives **chunks grouped by document, with the
document context** — nested chunks-per-document — not a flat list of strings:

```
embed_documents(docs: [{document_context, chunks: [chunk_text, ...]}, ...])
    -> [[vector, ...], ...]
```

Classic per-chunk embedding is the degenerate case (context ignored, one chunk per call batch).
**Rationale:** contextualized/late-chunking models need the surrounding document to embed each
chunk; retrofitting a flat trait later would touch every call site. The future message-store case
(thread as context for each turn window) is the same shape ([02-domain-model.md](02-domain-model.md) §5).
**Rejected:** flat `embed(texts) -> vectors` trait — locks the architecture to context-free
embedding.

### Models and providers

| Role | Choice | Notes |
|---|---|---|
| **Default (headline)** | `pplx-embed-context-v1-0.6b`, local via ONNX | Open-weight, MIT, explicit late-chunking support (verified mid-2026). Confirmed as default; see benchmark section for performance gates. |
| Lightweight preset / fallback | bge-small-class dense model | For weak hardware; classic per-chunk path. |
| Hosted contextualized | Perplexity `/v1/contextualizedembeddings`; Voyage `voyage-context-3` | Same nested API shape as the trait — direct mapping. |
| Generic hosted | Any OpenAI-compatible `/v1/embeddings` endpoint | Degenerate (flat) path; one provider abstraction for embeddings, LLMs stay out of the core process entirely. |

Models are **downloaded on first run** (with progress UI, checksum verification, resumable) into
the model cache ([03-config.md](03-config.md) §4) — never bundled into the binary.

### Local backends: ONNX (CPU) and CoreML (ANE/GPU)

The default `pplx-embed-context-v1-0.6b` runs on two interchangeable local backends, selected by
the `local` / `local-coreml` / `local-onnx` provider values ([03-config.md](03-config.md) §7).

- **ONNX (CPU):** the reference path. Late-chunking is run in `embed`: the model emits token
  embeddings, then Rust does mean-pooling over each chunk's token span and `tanh` int8
  quantization before binarization.
- **CoreML (ANE/GPU):** macOS-only, behind the opt-in `local-coreml` cargo feature
  (requires Rust ≥ 1.85). Executes on Apple Silicon's ANE/GPU via `objc2-core-ml`. Pooling and
  `tanh` int8 quantization happen **inside the model** — it consumes a `pool_matrix` input and
  outputs int8 `(32, 1024)` directly, so the in-Rust mean-pool + quant of the ONNX path is not
  needed. The CoreML bundle is the context (late-chunking) variant, downloaded from HF repo
  `dokterbob/pplx-embed-coreml` (pinned revision) via `hf-hub` 1.0, whose built-in XET transfers
  deduplicate the shared ~1.15 GB encoder weights across sequence-length buckets. Buckets are
  fixed ANE sequence lengths `L ∈ {512, 1024, 2048, 4096}` (whichever are published — currently
  only `context/L512-int8`) plus an optional dynamic GPU catch-all.

Both backends are **index-interchangeable**: same `model_id`, 1024-dim, `Binary` encoding,
sign-compatible vectors. Measured on Apple Silicon (CoreML fp16/ANE vs ONNX fp32/CPU on identical
chunks): **cosine parity ~0.995–0.9995** (the full-precision direction is essentially identical),
and **per-dimension sign/Hamming agreement ~98–99%** (0.982–0.994 observed). The few flips (~5–11 of
1024 dims) are dimensions whose pre-tanh value sits within fp16-rounding distance of zero and so
round to a different int8 sign at the tie point — they carry negligible magnitude. An index built by
one backend is queryable by the other with no reindex (the choice of backend does not affect
`policy_version`); cross-backend Hamming distances carry ~1–2% backend-induced bit noise on near-zero
dimensions, which is small relative to inter-document distances.

### Gating benchmark for the default model

Before `pplx-embed-context-v1-0.6b` is confirmed as default, measure on a mid-range laptop
(Apple Silicon, 16 GB): index a ~2 000-file / ~100 MB mixed corpus. **Gate:** sustained ≥ 15
chunks/s end-to-end and first-index ≤ 30 min; if missed, the bge-small-class preset becomes the
default and the 0.6b model the opt-in quality preset. Either outcome is config, not architecture.

### Policy versioning

`policy_version = hash(canonical serialization of the store's effective {chunking, embedding, parsers})`.
Stored on every chunk. On store open / config change, if the effective policy hash differs from
the indexed one, the store is marked stale and a reindex job is created (daemon: automatic;
embedded: on next `localdb index`, with a warning from `status`). Chunker, embedder, and parser
list change **together** — there is no partial invalidation ([03-config.md](03-config.md) §2).
The `parsers` list is hashed **order-sensitively** (unlike `chunking`/`embedding`, which use
order-independent key serialization), so reordering parsers alone marks the store stale and
schedules a reindex.

## 5. Retrieval

**Decision:** hybrid **BM25 + dense, fused with RRF** (k = 60), implemented **in our code** above
the `RetrievalStore` trait: query both legs (top-K each, default K = 50), fuse, then shape
results.

**Rationale:** hybrid-by-default is a day-one requirement; RRF is robust, parameter-light,
and score-scale-free. The LanceDB Rust API does not reliably provide server-side hybrid fusion
(trails the Python API), and owning fusion keeps it identical across future backends.
**Rejected:** score interpolation (needs per-model calibration); backend-native fusion (backend-dependent
behavior).

- **Filtering:** store filter (one, several, or all stores — fan out per-store queries, fuse with
  global RRF), plus metadata filters (mime, path prefix, fetched_at range) pushed down to the
  backend where supported.
- **Result shaping:** top-N (default 10) → Citation objects ([02-domain-model.md](02-domain-model.md) §6),
  with per-leg scores retained for debugging (`score: {fused, dense, bm25}`).
- **Reranking: explicitly post-MVP** ([06-roadmap.md](06-roadmap.md) §5). The pipeline leaves a
  seam (rerank stage between fuse and shape) but ships nothing.
- Query rewriting and answer generation are **not** backend-core concerns — they belong to
  downstream consumers (agents, future UI). URL/image as *query* modes: out of scope v1.

### Binary dense search (IVF_FLAT / Hamming)

**Decision:** when the embedder's `vector_encoding()` returns `Binary`, the store writes
a `FixedSizeList<UInt8>` column instead of `FixedSizeList<Float32>`:

- **Binarization:** `bit = (x ≥ 0.0)`, packed MSB-first (dim 0 → bit 7 of byte 0), matching
  `np.packbits(x >= 0, axis=-1)`. A 1024-dim float vector becomes 128 bytes.
- **Storage:** `embedding` column is `FixedSizeList<UInt8>(dim/8)`. Existing f32 stores are
  rejected on open with an `InvalidConfig` error (mismatch guard in `open()`).
- **Index:** `IVF_FLAT` with `DistanceType::Hamming` via the normal lancedb API. No-op when
  the table has fewer than 256 rows (flat Hamming scan is used instead).
- **Query bypass:** `nearest_to` hard-codes Float32, so the binary path goes through
  `Table::dataset()` → `Dataset::scan()` → `Scanner::nearest(col, &UInt8Array, k)`, which
  auto-selects Hamming distance. Score formula: `1.0 − hamming_dist / nbits ∈ [0, 1]`.
- **Supported embedders:** pplx local-ONNX models (`pplx-embed-context-v1-0.6b`,
  `pplx-embed-v1-0.6b`) override `vector_encoding()` to return `Binary`.
  `FakeEmbedder` keeps `Float32`.
- **Expected recall drop:** ~2–4 pts on MTEB-ML vs float32 at 1024 dim; cushioned by the
  BM25+RRF hybrid. Future rerank via an int8 copy can recover the gap.
