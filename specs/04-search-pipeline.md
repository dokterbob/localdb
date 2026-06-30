# Spec 04 — Ingestion & Retrieval Pipeline

> Status: accepted draft, 2026-06-30.

Pipeline: **acquire → extract → blocks → chunks → embed → index** (write side), and
**query → BM25 + dense → RRF fuse → filter → citations** (read side).

## 1. Acquisition

Acquisition is driven by **ingestors** (`Ingestor` trait in `core`). The ingestor kind
determines how content reaches the pipeline and what kind of Resource it produces.

### Ingestor kinds

- **`file` ingestor:** Scans paths on demand (`localdb index`); the daemon additionally
  watches continuously via `notify` (FSEvents on macOS, inotify on Linux), with debounce so
  editor save-storms coalesce. Include/exclude globs from the source spec. Runs the parser
  chain on each file, receives a `ParsedDocument`, and converts it to a `Resource` with typed
  blocks via `markdown_to_blocks()`. The parser chain is an implementation detail of this
  ingestor, not a top-level pipeline concept.
- **`url` ingestor:** HTTP fetch; runs the same parser chain (readability-style main-content
  extraction → Markdown); converts `ParsedDocument` to a `Resource` with blocks via
  `markdown_to_blocks()`. Conditional GET (ETag/Last-Modified) when available. Refreshes on
  the configured interval (daemon) or on explicit `localdb index` (embedded).
- **Future ingestors** (notion, telegram, signal, email, transcription, feed): produce
  `Resource` objects directly with native block types (e.g. `Message`, `Segment`,
  `Attachment`), bypassing the parser chain entirely. The `Ingestor` trait's contract is to
  deliver a `Resource`; how it gets there is ingestor-internal.

### Incremental re-index

A resource is re-processed only when its `content_hash` changes. `content_hash` is a blake3
hash of the ordered canonical texts of all blocks in the resource (not a hash of a Markdown
string). Unchanged → skip; changed → replace-by-URI: delete the old resource's chunks,
insert the new ones (new content-addressed IDs, [02-domain-model.md](02-domain-model.md) §3).

Resources also carry:
- **`external_etag`:** for URL sources, the server-supplied ETag or Last-Modified value; used
  for conditional GET to avoid re-fetching unchanged content.
- **`extractor_version`:** a version stamp on the parser/block-conversion logic. When parser
  or `markdown_to_blocks()` logic improves, bumping `extractor_version` enables selective
  reprocessing of resources whose content has not changed but whose block representation may
  improve (without a full policy-version reindex).

### Deletes

File deleted / URL gone (404 or 410 after retry) / source removed → delete that resource's
chunks and the resource itself from the backend. Deletes are data-modifying: ≥ 90% coverage
gate ([01-architecture.md](01-architecture.md) §7).

## 2. Extraction (v1 matrix)

The parser chain is an implementation detail of the `file` and `url` ingestors. Parsers
still return a `ParsedDocument` (Markdown string + title + `DocumentMetadata`), which the
ingestor then converts to a `Resource` with typed blocks via `markdown_to_blocks()`. Future
ingestors that natively produce structured content may emit blocks directly without going
through a Markdown intermediate.

| Format | Approach | Notes |
|---|---|---|
| Markdown | pulldown-cmark parser (passthrough) | Headings → heading blocks; code fences → code blocks. |
| Plain text | direct passthrough | Treated as Markdown verbatim. |
| HTML | readability-style main-content selection → Markdown | Used for both `url` fetches and `.html` files. |
| PDF (text layer) | Rust PDF text extraction | Scanned/text-less PDFs are rejected (no OCR). |
| Office (DOCX/PPTX/CSV) | `anytomd` (v1.3.0) → Markdown | Production-ready. XLSX/XLS disabled (see below). |
| EPUB | `rbook` spine walk → per-chapter XHTML → Markdown via the internal HTML converter | Reading order preserved; OPF Dublin Core → `DocumentMetadata`. Extension-gated (`.epub`). DRM'd / image-only books → `ExtractionFailed`. |

**Out of scope (explicit):** OCR / scanned PDFs and images. EPUB is the only ebook format
supported; **MOBI/AZW/AZW3** (PalmDOC/KF8 compression, frequent DRM — realistically need a
Calibre shell-out) and **FB2/CBZ** (on `rbook`'s roadmap, not yet implemented) are deferred.
Rationale and the full deferred list: [06-roadmap.md](06-roadmap.md) §5. Unsupported files are
skipped and counted in IndexJob stats, not errors.

**XLSX/XLS explicitly disabled:** Despite anytomd supporting XLSX/XLS in principle, extraction
for these formats is disabled in `OfficeParser` pending an upstream performance fix.
`anytomd::convert_bytes` on an 87K-row XLSX (6.9 MB) took >16 minutes in production (vs. <1 s
for the equivalent CSV). The file is counted as `unsupported_format`, not an error. Use CSV
export as a workaround. Tracking: https://github.com/developer0hye/anytomd-rs/issues/94

**Extension-gated acceptance:** The `PlaintextParser` (and by extension the full parser chain)
only accepts files whose extension or basename matches the list published by
`extract::supported_extensions()` (text and code/data extensions, plus known lockfile
basenames such as `Cargo.lock`). Files with unknown or binary extensions — `.exe`, `.png`,
`.bin`, etc. — are declined at the parser level and counted as `unsupported_format` in
`IndexJobStats` without ever entering the chunker or embedder. This prevents indexing
hangs caused by chunkers receiving arbitrarily large binary blobs.

**Default include allowlist for directory sources:** When a `path` source points to a
directory and no explicit `include` globs have been set, `cli` automatically applies
`DEFAULT_PATH_INCLUDES` — a glob list derived from `extract::supported_extensions()` —
so that file-system enumeration skips unsupported files before they ever reach the
extraction layer. Single-file sources are not affected (they carry an exact filename
glob). Sources added via explicit `include` override this default entirely.

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

Chunking operates on **blocks**, not on a raw Markdown string. Each resource carries an
ordered sequence of typed blocks; the chunker dispatches on `BlockKind` to produce
block-appropriate chunks.

### Block-dispatch rules

| Block kinds | Chunker | Behavior |
|---|---|---|
| `Heading`, `Paragraph`, `Quote`, `List` | prose chunker | `MarkdownSplitter` subdivides within the block on semantic boundaries (sentences, words); target ≈ 256 tokens, overlap ≈ 0 tokens. |
| `Code` | code chunker | Line-based subdivision within the block; target ≈ 60 lines. |
| `Message`, `Segment` | messages chunker | Multi-block windowed: sliding window over consecutive Message/Segment blocks (see below). |
| `Table` | table-aware chunker | Row-based subdivision; one or more rows per chunk. |
| `Reference`, `Attachment`, `Frontmatter`, `Image` | one chunk per block | These block types are typically small; no further subdivision. |

The source-level preset (from config or the `--preset` CLI flag) acts as a hint for
ambiguous cases; block-kind dispatch takes precedence for unambiguous kinds.

**Invariant:** chunks never cross block boundaries, EXCEPT message-window chunks, which
explicitly span multiple consecutive `Message`/`Segment` blocks. This invariant makes
`heading_path` attribution deterministic (see below) and ensures context-expansion
queries are well-defined.

### heading_path attribution

`heading_path` is derived from the block tree: heading blocks that precede a content block
in the resource's ordered block sequence are collected into the path for all chunks produced
from that content block. There is no re-parsing of Markdown — the heading structure comes
directly from the block representation.

### Messages chunker

The `messages` preset is implemented as a sliding window over `Message`/`Segment` blocks:

- **`window_turns`** (default 6): number of consecutive turns per chunk.
- **`stride_turns`** (default 3): step between windows.
- Windows are additionally bounded by token count so that no single chunk exceeds the
  embedding model's context limit.
- Each window chunk carries the `heading_path` of the containing thread/resource.

### Prose chunker details

Chunk sizing for `prose` is **token-accurate**, measured using the embedding model's own
tokenizer (the default model `pplx-embed-context-v1-0.6b` supports up to 32K tokens;
localdb caps its late-chunking window at 4096 tokens = 16 × 256-token chunks). When no
local tokenizer is available (e.g. hosted/API embedders), it falls back to a character
approximation (~4 chars/token). The 256-token / 0-overlap defaults mirror the contextual
late-chunking model's training regime: Perplexity's contextualized-embeddings model is
trained on documents partitioned into 256-token chunks (16 chunks per 4096-token document)
with **no** intra-document overlap, because late chunking shares context across chunks from
the same document (chunks must be sent in source-document order) and so supplies cross-chunk
context itself. Aligning the chunker to that regime gives smaller, precise chunks — better
citation granularity — while the model handles cross-chunk context, with no overlap needed.
These are defaults to beat with evaluation, not dogma.

**`chunk_prose` structureless fallback:** When `MarkdownSplitter` produces a single chunk
covering the whole block (a sign that the content lacks structure), `chunk_prose` falls back
to the `code` chunker so that the content is still indexed in bounded chunks.

**`chunk_code` long-line split:** `chunk_code` enforces a per-line byte limit. Lines
exceeding the limit are split into fixed-width sub-segments before chunking, preventing
single-line binary or minified content from producing unbounded chunk sizes.

### Spreadsheet routing

Spreadsheet formats (`.xlsx`, `.xls`) route to the code chunker. These files produce
extracted text that is dense tabular content (similar to CSV), so the fast line-based
chunker is used instead of the prose splitter to avoid hangs on large tables. Note:
XLSX/XLS extraction is currently disabled (see §2), so this routing is moot until the
upstream fix lands.

## 4. Embedding

### Document-aware interface

**Decision:** the `Embedder` trait in `core` receives **chunks grouped by resource, with
resource and block context** — not a flat list of strings:

```
embed_documents(docs: [{document_context, chunks: [chunk_text, ...]}, ...])
    -> [[vector, ...], ...]
```

"Document context" for embedding is constructed from the resource's block sequence.
Concretely, an embedding renderer may serialize nearby blocks into a Markdown-like context
string as an implementation detail — the trait shape is stable regardless of how context is
assembled. Classic per-chunk embedding is the degenerate case (context ignored, one chunk
per call batch). **Rationale:** contextualized/late-chunking models need the surrounding
document to embed each chunk; retrofitting a flat trait later would touch every call site.
The message-store case (thread as context for each turn window) is the same shape
([02-domain-model.md](02-domain-model.md) §5). **Rejected:** flat `embed(texts) -> vectors`
trait — locks the architecture to context-free embedding.

### Models and providers

| Role | Choice | Notes |
|---|---|---|
| **Default (headline)** | `pplx-embed-context-v1-0.6b`, local via ONNX | Open-weight, MIT, explicit late-chunking support (verified mid-2026). Confirmed as default; see benchmark section for performance gates. |
| Lightweight preset / fallback | bge-small-class dense model | For weak hardware; classic per-chunk path. |
| Hosted contextualized | Perplexity `/v1/contextualizedembeddings`; Voyage `voyage-context-3` | Same nested API shape as the trait — direct mapping. |
| Generic hosted | Any OpenAI-compatible `/v1/embeddings` endpoint | Degenerate (flat) path; one provider abstraction for embeddings, LLMs stay out of the core process entirely. |

Models are **downloaded on first run** (with progress UI, checksum verification, resumable)
into the model cache ([03-config.md](03-config.md) §4) — never bundled into the binary.

### Local backends: ONNX (CPU) and CoreML (ANE/GPU)

The default `pplx-embed-context-v1-0.6b` runs on two interchangeable local backends,
selected by the `local` / `local-coreml` / `local-onnx` provider values
([03-config.md](03-config.md) §7).

- **ONNX (CPU):** the reference path. Late-chunking is run in `embed`: the model emits
  token embeddings, then Rust does mean-pooling over each chunk's token span and `tanh` int8
  quantization before binarization.
- **CoreML (ANE/GPU):** macOS-only, behind the opt-in `local-coreml` cargo feature
  (requires Rust ≥ 1.85). Executes on Apple Silicon's ANE/GPU via `objc2-core-ml`. Pooling
  and `tanh` int8 quantization happen **inside the model** — it consumes a `pool_matrix`
  input and outputs int8 `(32, 1024)` directly, so the in-Rust mean-pool + quant of the
  ONNX path is not needed. The CoreML bundle is the context (late-chunking) variant,
  downloaded from HF repo `dokterbob/pplx-embed-coreml` (pinned revision) via `hf-hub` 1.0,
  whose built-in XET transfers deduplicate the shared ~1.15 GB encoder weights across
  sequence-length buckets. Buckets are fixed ANE sequence lengths `L ∈ {512, 1024, 2048,
  4096}` (whichever are published — currently only `context/L512-int8`) plus an optional
  dynamic GPU catch-all.

Both backends are **index-interchangeable**: same `model_id`, 1024-dim, `Binary` encoding,
sign-compatible vectors. Measured on Apple Silicon (CoreML fp16/ANE vs ONNX fp32/CPU on
identical chunks): **cosine parity ~0.995–0.9995** (the full-precision direction is
essentially identical), and **per-dimension sign/Hamming agreement ~98–99%** (0.982–0.994
observed). The few flips (~5–11 of 1024 dims) are dimensions whose pre-tanh value sits
within fp16-rounding distance of zero and so round to a different int8 sign at the tie
point — they carry negligible magnitude. An index built by one backend is queryable by the
other with no reindex (the choice of backend does not affect `policy_version`);
cross-backend Hamming distances carry ~1–2% backend-induced bit noise on near-zero
dimensions, which is small relative to inter-document distances.

### Gating benchmark for the default model

Before `pplx-embed-context-v1-0.6b` is confirmed as default, measure on a mid-range laptop
(Apple Silicon, 16 GB): index a ~2 000-file / ~100 MB mixed corpus. **Gate:** sustained
≥ 15 chunks/s end-to-end and first-index ≤ 30 min; if missed, the bge-small-class preset
becomes the default and the 0.6b model the opt-in quality preset. Either outcome is config,
not architecture.

### Policy versioning

`policy_version = hash(canonical serialization of the store's effective {chunking, embedding, parsers})`.
Stored on every chunk. On store open / config change, if the effective policy hash differs
from the indexed one, the store is marked stale and a reindex job is created (daemon:
automatic; embedded: on next `localdb index`, with a warning from `status`). Chunker,
embedder, and parser list change **together** — there is no partial invalidation
([03-config.md](03-config.md) §2). The `parsers` list is hashed **order-sensitively**
(unlike `chunking`/`embedding`, which use order-independent key serialization), so
reordering parsers alone marks the store stale and schedules a reindex.

`content_hash` is a blake3 hash of the ordered canonical texts of all blocks in a resource
(not of a Markdown string). `extractor_version` on resources enables selective reprocessing
when parser or `markdown_to_blocks()` logic improves, without requiring a full
policy-version reindex.

## 5. Retrieval

**Decision:** hybrid **BM25 + dense, fused with RRF** (k = 60), implemented **in our code**
above the `RetrievalStore` trait: query both legs (top-K each, default K = 50), fuse, then
shape results.

**Rationale:** hybrid-by-default is a day-one requirement; RRF is robust, parameter-light,
and score-scale-free. Owning fusion keeps it identical across future backends. **Rejected:**
score interpolation (needs per-model calibration); backend-native fusion (backend-dependent
behavior).

- **Filtering:** store filter (one, several, or all stores — fan out per-store queries, fuse
  with global RRF), plus metadata filters (mime, path prefix, fetched_at range) pushed down
  to the backend where supported.
- **Result shaping:** top-N (default 10) → Citation objects
  ([02-domain-model.md](02-domain-model.md) §6), with per-leg scores retained for debugging
  (`score: {fused, dense, bm25}`). Citations carry a **block reference** and chunk position
  within that block, not just a Markdown span.
- **Reranking: explicitly post-MVP** ([06-roadmap.md](06-roadmap.md) §5). The pipeline
  leaves a seam (rerank stage between fuse and shape) but ships nothing.
- Query rewriting and answer generation are **not** backend-core concerns — they belong to
  downstream consumers (agents, future UI). URL/image as *query* modes: out of scope v1.

### Context expansion

Context expansion is a first-class retrieval capability, available after initial ranking:

- **Neighboring chunks in the same block:** retrieve the chunks immediately before and after
  a result chunk within the same block, to provide sentence-level continuity.
- **Nearby blocks in the same resource:** retrieve adjacent blocks (by block tree position)
  in the same resource, for section-level context.
- **Full resource block sequence:** retrieve all blocks from a resource in order, for
  document-level context (e.g. for a summarization or answer-synthesis consumer).

Context expansion is exposed as explicit query operations, not applied automatically to
search results.

### Dense search (DiskANN / libsql)

**Decision:** the store backend is libsql (Turso's SQLite fork) with built-in vector
search. Dense vectors are stored as `F32_BLOB` (float32) or `F1BIT_BLOB` (binary) column
types, with DiskANN indexing via `libsql_vector_idx`.

- **Float32 path:** embedding column is `F32_BLOB(dim)`. Search via `vector_top_k(table,
  col, query_blob, k)` which uses the DiskANN index automatically. Score conversion: cosine
  distance → score via `1.0 - distance / 2.0 ∈ [0, 1]`.
- **Binary path:** when the embedder's `vector_encoding()` returns `Binary`, the store
  writes an `F1BIT_BLOB(dim)` column. Binarization: `bit = (x ≥ 0.0)`, packed MSB-first
  (dim 0 → bit 7 of byte 0). A 1024-dim float vector becomes 128 bytes. Search uses Hamming
  distance. Score formula: `1.0 − hamming_dist / nbits ∈ [0, 1]`.
- **Index maintenance:** DiskANN indexes are auto-maintained by libsql — no manual
  `create_vector_index` calls are needed. The index is created implicitly when
  `vector_top_k` is first used.
- **BM25 via FTS5:** full-text search uses libsql's FTS5 extension with `bm25()` scoring.
  FTS5 indexes are auto-maintained — no manual `create_fts_index` calls. The FTS5 virtual
  table is created alongside the chunks table and kept in sync via triggers.
- **Supported embedders:** pplx local-ONNX models (`pplx-embed-context-v1-0.6b`,
  `pplx-embed-v1-0.6b`) override `vector_encoding()` to return `Binary`. `FakeEmbedder`
  keeps `Float32`.
- **Expected recall drop (binary):** ~2–4 pts on MTEB-ML vs float32 at 1024 dim; cushioned
  by the BM25+RRF hybrid. Future rerank via an int8 copy can recover the gap.
