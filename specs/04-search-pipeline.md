# Spec 04 — Ingestion & Retrieval Pipeline

> Status: accepted draft, 2026-06-10.

Pipeline: **acquire → extract → blocks → chunks → embed → index** (write side), and
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
document (text + Blocks with heading paths and spans, [02-domain-model.md](02-domain-model.md)).

| Format | Approach | Notes |
|---|---|---|
| Markdown | pulldown-cmark-class parser | Headings → `heading_path`; code fences kept as blocks. |
| Plain text | direct | Paragraph blocks by blank lines. |
| HTML | readability-style main-content + DOM walk | Used for both `url` fetches and `.html` files. |
| PDF (text layer) | Rust PDF text extraction | Page numbers recorded in block metadata for citations. |

**Out of scope for v1 (explicit):** OCR, scanned PDFs, DOCX/PPTX/XLSX, images. Rationale: no
single mature Rust extraction stack covers these well; shipping a sharp matrix
beats shipping a ragged one. Unsupported files are skipped and counted in IndexJob stats, not
errors. Roadmap: [06-roadmap.md](06-roadmap.md) §5.

## 3. Chunking

Chunking is half of the per-store `indexing` policy ([03-config.md](03-config.md) §2).
Per-source-kind presets, applied per source:

| Preset | Strategy | Defaults |
|---|---|---|
| `prose` (default) | `MarkdownSplitter` (`benbrandt/text-splitter`) on Markdown semantic boundaries — headings, code blocks, lists, paragraphs, sentences, words; preserves document order; `heading_path` attributed per chunk | target ≈ 512 tokens, overlap ≈ 64 tokens |
| `messages` (reserved for connectors) | Thread/turn windows: N consecutive messages per chunk, sliding | window 6 turns, stride 3 |
| `code` | Structural (function/item-level) where parseable, else line blocks | target ≈ 60 lines |

Chunk sizing for `prose` is **token-accurate**, measured using the embedding model's own tokenizer
(the default model `pplx-embed-context-v1-0.6b` has an 8192-token context). When no local
tokenizer is available (e.g. hosted/API embedders), it falls back to a character approximation
(~4 chars/token). The 512-token / 64-overlap defaults are sized for the contextual late-chunking
model: Perplexity's contextualized-embeddings model shares context across chunks from the same
document (chunks must be sent in source-document order), so minimal overlap is sufficient —
smaller, precise chunks give better citation granularity while the model handles cross-chunk
context. These are defaults to beat with evaluation, not dogma.

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
| **Default (headline)** | `pplx-embed-context-v1-0.6b`, local via ONNX | Open-weight, MIT, explicit late-chunking support (verified mid-2026). **Pending the benchmark below.** |
| Lightweight preset / fallback | bge-small-class dense model | For weak hardware; classic per-chunk path. |
| Hosted contextualized | Perplexity `/v1/contextualizedembeddings`; Voyage `voyage-context-3` | Same nested API shape as the trait — direct mapping. |
| Generic hosted | Any OpenAI-compatible `/v1/embeddings` endpoint | Degenerate (flat) path; one provider abstraction for embeddings, LLMs stay out of the core process entirely. |

Models are **downloaded on first run** (with progress UI, checksum verification, resumable) into
the model cache ([03-config.md](03-config.md) §4) — never bundled into the binary.

### Gating benchmark for the default model

Before `pplx-embed-context-v1-0.6b` is confirmed as default, measure on a mid-range laptop
(Apple Silicon, 16 GB): index a ~2 000-file / ~100 MB mixed corpus. **Gate:** sustained ≥ 15
chunks/s end-to-end and first-index ≤ 30 min; if missed, the bge-small-class preset becomes the
default and the 0.6b model the opt-in quality preset. Either outcome is config, not architecture.

### Policy versioning

`policy_version = hash(canonical serialization of the store's effective {chunking, embedding})`.
Stored on every chunk. On store open / config change, if the effective policy hash differs from
the indexed one, the store is marked stale and a reindex job is created (daemon: automatic;
embedded: on next `localdb index`, with a warning from `status`). Chunker and embedder change
**together** — there is no partial invalidation ([03-config.md](03-config.md) §2).

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
