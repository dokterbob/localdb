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
string). Unchanged → skip; changed → replace-by-URI: the old resource's chunks/blocks/resource
row are deleted AND the new ones (new content-addressed IDs,
[02-domain-model.md](02-domain-model.md) §3) are inserted in a **single atomic store
transaction**. A write failure during the replace (e.g. an embedding or constraint error)
leaves the old resource intact and searchable — the store never observes a state where the
old chunks are gone but the new ones failed to land.

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

A resource stays alive across a run if it is observed via **either** `on_resource` or
`on_skipped` — the delete-sweep removes only a URI that neither hook reported this run.
`on_skipped` takes an already-canonical `Uri`, the same representation `on_resource`'s
`Resource.uri` carries, so both hooks populate the sweep's "seen" set in one consistent key
space.

An extraction that produces **zero blocks** must never be yielded as a `Resource` to
`on_resource`: `index_resource`'s zero-chunk arm treats a zero-block Resource as an empty
replacement, deleting the previously indexed content for that URI and writing nothing in its
place — it cannot tell "the source is legitimately empty" apart from "extraction produced
nothing usable." Ingestors must therefore classify "extracted to nothing" as unusable and report
it via `on_skipped`, not as content via `on_resource`.
[Issue #156](https://github.com/dokterbob/localdb/issues/156) ("Delete-sweep wipes an entire
source when its root is unreachable (data loss)") is the source-level statement of the same
rule — zero-URIs-enumerated there, zero-blocks-extracted here — both are the same conflation of
"unavailable" with "legitimately empty," one level apart.

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
| PDF (text layer) | `pdf_oxide` per-page → Markdown, plus retrieval-oriented post-processing (see below) | Scanned/text-less PDFs are rejected (no OCR); Info dict + XMP → `DocumentMetadata`. |
| Office (DOCX/PPTX/CSV) | `anytomd` (v1.3.0) → Markdown | Production-ready. XLSX/XLS disabled (see below). |
| EPUB | `rbook` spine walk → per-chapter XHTML → Markdown via the internal HTML converter | Reading order preserved; OPF Dublin Core → `DocumentMetadata`. Extension-gated (`.epub`). DRM'd / image-only books → `ExtractionFailed`. |

#### PDF extraction is tuned for retrieval, not for visual fidelity

A PDF is converted one page at a time and the pages are concatenated, recording the byte offset
where each page begins so blocks can carry a `page` number (§2 of
[02-domain-model.md](02-domain-model.md)). Four deliberate deviations from the extractor's
defaults, plus three local repairs, exist because the goal is the words an author wrote — not a
faithful reproduction of the printed page:

- **Artifacts are dropped** (`/Artifact`-tagged running headers, footers, page-number folios and
  watermarks, per ISO 32000-1 §14.8.2.2.1). Without this, a folio becomes its own chunk.
- **Ligatures are expanded** (U+FB00–U+FB06 → `fi`/`fl`/`ff`/…), so BM25 tokenization and the
  embedder see real words.
- **Soft hyphens (U+00AD) are deleted.** They are discretionary line-break hints with no textual
  meaning (§14.8.2.2.3). Deletion alone reconstructs the word; line-break hyphens are
  deliberately *not* rejoined, as that is where `well-being` → `wellbeing` corruption comes from.
- **Pages with no text layer are dropped, not annotated.** A page that yields nothing contributes
  nothing; the whole set is reported once via a `WARN` naming the count and page ranges. A
  *mixed* document — real text on some pages, bare scans on others — still indexes its text.
  This is what keeps the "scanned PDFs are rejected, OCR is out of scope" statement below honest:
  previously a placeholder marker was emitted per skipped page and indexed as though it were
  content, which also let mixed documents evade scanned-PDF detection entirely.
- **Heading and code-block inference is guarded.** The extractor infers headings from font
  clustering and code blocks from monospace detection; both over-fire on real books. A heading
  that fails a sanity check is demoted to a paragraph (it stays indexed — it just stops becoming
  a `heading_path` breadcrumb for every following chunk), and a bare fence whose content reads as
  prose is un-fenced. Both guards are biased hard against false positives.

Geometric stripping of running headers in *untagged* PDFs is **not** enabled: the upstream
implementation matches glyph-run spans rather than lines and deletes body text from multi-column
documents. See `docs/followups-pdf-oxide-swap.md`.

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

Exactly one WARN is emitted per failing resource. `core::ingestion` owns that line — it is the
layer that accounts for ingestion outcomes — so ingestors log their extra framing at `debug!`
and fold anything the operator needs at default level (e.g. that the failure was a *panic*
rather than a returned error) into the `SkipReason::Error` payload itself.

**Partial success is a fourth, orthogonal case.** A document can extract successfully and still
lose content: a PDF whose text layer covers only some pages contributes nothing for the rest.
That is `Ok` — the document indexes, and counts as a success — but `extract` emits one WARN
naming how many pages were dropped and which. It is deliberately not an error variant: a mixed
scanned/text book must still be indexed for the text it does have.

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
| `Heading`, `Text` | prose chunker | `MarkdownSplitter` subdivides within the block on semantic boundaries (sentences, words); target ≈ 256 tokens, overlap ≈ 0 tokens. |
| `Code` | code chunker | Line-based subdivision within the block; target ≈ 60 lines. |
| `Message`, `Segment` | messages chunker | Multi-block windowed: sliding window over consecutive Message/Segment blocks (see below). |
| `Table` | table chunker | Row-based packing: rows are packed into chunks under the token target; every chunk re-emits the header row and separator so each chunk is a standalone valid table (see below). |
| `Reference`, `Attachment`, `Frontmatter`, `Image` | one chunk per block | These block types are typically small; no further subdivision. |

The source-level preset (from config or the `--preset` CLI flag) acts as a hint for
ambiguous cases; block-kind dispatch takes precedence for unambiguous kinds.

**Invariant:** chunks never cross block boundaries, EXCEPT message-window chunks, which
explicitly span multiple consecutive `Message`/`Segment` blocks. This invariant makes
`heading_path` attribution deterministic (see below) and ensures context-expansion
queries are well-defined.

**Coarse `Text` blocks:** `markdown_to_blocks` emits **one** `Text` block per run of
consecutive running-text content — paragraphs, lists, blockquotes, HTML blocks — between
structural boundaries (a `Heading`, a `Table`/`Code`/`Image` block, or the start/end of the
document). Block sizing is therefore purely structural, driven by element boundaries, not by
a token target; the prose chunker restores token-sizing by splitting a `Text` block into one
or more prose chunks via `MarkdownSplitter`. This is what lets prose chunks approach the
~256-token target instead of producing one tiny chunk per paragraph (the #158 fix) — a
multi-paragraph run of running text is chunked together, not paragraph-by-paragraph.
**Headings remain discrete blocks and are still chunked** — a `Heading` block is never folded
into an adjacent `Text` run, because it feeds the contextual embedder's document context and
`heading_path` attribution (see below). Filtering headings out of *search results* is a
separate, future search-side concern, not a chunking-time decision.

### heading_path attribution

`heading_path` is derived from the block tree: heading blocks that precede a content block
in the resource's ordered block sequence are collected into the path for all chunks produced
from that content block. There is no re-parsing of Markdown — the heading structure comes
directly from the block representation.

### Source preset override

`IngestionConfig` carries a `source_preset` field (`prose` (default) | `code` | `messages`),
resolved from the source's config (the source-level `preset` key,
[03-config.md](03-config.md) §2) or the CLI's `--preset` flag. Per-file automatic preset
routing — `preset_for`, which inspects a resource's `uri` and `mime` hint to route e.g.
JSON/YAML/lockfiles to the `code` preset — applies **only** when the source preset is the
default `prose`. An explicit `code` or `messages` source preset is authoritative and wins
over per-file detection: a source configured as `code` chunks every file in it with the code
chunker regardless of what `preset_for` would otherwise guess, and likewise a `messages`
source always uses the messages chunker. This preserves useful per-file auto-detection for
the common (prose) case while giving explicit non-default presets full, unambiguous control —
needed for message/transcript sources where `preset_for`'s filename/mime heuristics do not
apply.

### Messages chunker

The `messages` preset is implemented as a sliding window over `Message`/`Segment` blocks:

- **`window_turns`** (default 6): number of consecutive turns per chunk.
- **`stride_turns`** (default 3): step between windows.
- Windows are additionally bounded by token count so that no single chunk exceeds the
  embedding model's context limit.
- Each window chunk carries the `heading_path` of the containing thread/resource.
- **`window_block_seqs`:** each window chunk records the `block_seq` of every member block it
  spans in its `location` (`location_json = {start, end, window_block_seqs?}`), not just the
  denormalized `block_seq` of its first member — so context-expansion and `get_chunks`
  consumers can resolve every block a multi-block chunk touches.
- **Chunk ids after fix-up:** chunk ids are computed **after** the window fix-up pass (the
  pass that shrinks a window from the end to fit the token budget, or to keep the last window
  from running past the end of the turn sequence — see `chunk_messages`) — so the
  content-addressed id (§4 below;
  [02-domain-model.md](02-domain-model.md) §3) reflects the window's final membership, not a
  pre-fix-up candidate window.

### Table chunker

`Table` blocks are chunked by a dedicated row-based packer, not routed through the code
chunker:

- Rows are packed greedily into successive chunks, filling each chunk up to the (prose)
  token target.
- Every chunk re-emits the table's header row and the `|---|` separator row, so each chunk is
  a valid, independently renderable Markdown table — a chunk is never dependent on a sibling
  chunk to parse correctly.
- **Oversized single row:** a single data row that alone exceeds the token target cannot be
  packed into a standalone valid chunk at the target size; it falls back to `chunk_code`'s
  long-line split (see below), preserving the invariant that no single chunk grows
  unbounded.

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
  (requires Rust ≥ 1.85, subsumed by the workspace's 1.88 floor). Executes on Apple Silicon's
  ANE/GPU via `objc2-core-ml`. Pooling
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

The `chunking` sub-policy embeds a chunking algorithm identifier as part of what gets hashed;
bumping it forces a reindex even when no user-visible config field changed. Current value:
**`textsplitter-md-v5`** (bumped from `v4`). The `v5` bump covers the coarse `Text` block
ontology — `markdown_to_blocks` now emits one `Text` block per run of consecutive running-text
content, so prose chunks pack toward the ~256-token target instead of one-tiny-chunk-per-
paragraph (§3 above, "Coarse `Text` blocks"; the #158 fix), silently altering chunk boundaries.
The prior `v4` bump (from `v3`) covered two changes at once: the new `Chunk.id` formula —
`blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)`
([02-domain-model.md](02-domain-model.md) §2, Chunk) — and the addition of the table chunker.
Any of these changes alone would silently alter chunk boundaries and/or ids without a policy
bump, defeating incremental re-index's staleness detection.

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

- **Filtering:** store filter (one, several, or all stores). Multi-store queries fan out
  per-store BM25 + dense queries, then pool each leg's results across all queried stores into
  one globally rank-ordered list *before* a single RRF pass runs over the two pooled legs —
  never per-store RRF followed by a merge, which would let every store's local rank-0 chunk tie
  regardless of true quality (RRF scores are rank-based and scale-free). Fusion identity is the
  composite `(store_id, chunk_id)`, not `chunk_id` alone: chunk IDs are content-addressed
  ([02-domain-model.md](02-domain-model.md) §2), so the same document indexed into two stores
  yields the same chunk_id in both — a `chunk_id`-only key would silently merge two stores'
  distinct hits. Ties are broken deterministically: `fused_score` descending, then `store_id`
  ascending, then `chunk_id` ascending. Metadata filters (mime, path prefix, fetched_at range)
  are pushed down to the backend where supported.

  **Known limitation — cross-store score comparability.** Pooling ranks each leg by its raw
  backend score, which assumes every store queried together reports that leg's scores on the
  same scale. Two ways that assumption is imperfect:

  - **BM25** scores are corpus-relative (per-store IDF and average document length), so a
    pooled BM25 ranking compares numbers that are not strictly commensurable even when every
    store runs the same backend. Cross-store ordering within that leg is therefore
    approximate.
  - **Dense** scores land in `[0, 1]`, but not via one common mapping. `store-libsql`
    converts distance to score two ways, chosen per store by the encoding its embedder
    produced: `1 - d/2` from a continuous cosine distance (`VectorEncoding::Float32`), and
    `1 - d/nbits` from a sign-only binarized Hamming distance (`VectorEncoding::Binary`) —
    the latter being what the default `pplx-embed-context-v1-0.6b` emits. Same range,
    different distributions, so pooling across a Binary-encoded and a Float32-encoded store
    would favor whichever mapping runs hotter rather than whichever store is more relevant.
    The two shipped models differ in dimensionality (1024 vs 384) so one query cannot
    currently reach both, but nothing enforces that. Separately, `SearchResult.score`
    documents the leg as "cosine/dot-product": an unbounded dot-product would swamp a bounded
    score outright, and is in any case wrong for the default model, whose vectors are
    unnormalized and which documents cosine as required. Dense scores must be a bounded
    similarity in `[0, 1]` — a precondition of pooling, not a free choice.

  Global pooling is still strictly better than per-store RRF, which gave *every* store's
  local rank-0 chunk an identical score regardless of quality. But multi-store relevance is
  not fully solved until both legs are calibrated. Tracked by issue #40; see also §"Rejected"
  above on score interpolation.
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

#### Index tuning and the per-chunk cost model

libsql stores every DiskANN node as a **fixed-size** blob, allocated with
`sqlite3_bind_zeroblob(..., nBlockSize)` regardless of the node's actual degree. So the
index has an exact, unavoidable per-chunk cost:

```
block_size = (node_vec_size + 16) + max_neighbors × (edge_vec_size + 16)
```

`max_neighbors` multiplies the edge width, which makes `compress_neighbors` the single
largest lever on total database size. Hence the invariant:

> **`compress_neighbors` must never be wider than the embedding column's own encoding.**
> It is a *compression* only relative to a wider node vector. Against a narrower one it is
> an inflation that buys no recall.

Violating that invariant is what produced issue #179 (45 GB for ~600k chunks whose raw
vectors are under 1 GB) and issue #177. `compress_neighbors=float8` was chosen in PR #92
while a float32 column was in play, and was not revisited when the binary column landed. On
an `F1BIT_BLOB` column libsql converts each source bit to `±1` and quantizes *that* to a
byte, so every one of those 1032 edge bytes held only 0 or 255 — 8× the space for exactly
the information the 128-byte node vector already carried, and therefore zero recall benefit.
Pinning `max_neighbors=64` compounded it by overriding libsql's own guard rail, which caps
edge overhead at 50× node overhead.

Measured per-chunk cost at 1024 dimensions (read back from `libsql_vector_meta_shadow`, not
derived — see `store-libsql/tests/vector_index_cost.rs`):

| Column | `compress_neighbors` | `max_neighbors` | Bytes/chunk | 600k chunks |
|---|---|---|---|---|
| `F1BIT_BLOB` | *(none — schema v6)* | *(libsql default: 51)* | **7,488** | **4.5 GB** |
| `F1BIT_BLOB` | `float8` (schema v5) | 64 | 67,216 | 40.3 GB |
| `F32_BLOB` | `float8` | 64 | 71,184 | 42.7 GB |
| `F32_BLOB` | *(none)* | *(libsql default: 51)* | 213,824 | 128 GB |

The tuning is therefore **encoding-dependent**, and derived in exactly one place
(`store-libsql`'s `vectors::vector_index_params`): binary columns pass `metric=cosine`
alone, while float32 columns keep both parameters, because for a 4 KiB node vector `float8`
edges are a genuine 4× compression and the bare default would be 3× *worse*.

**Recall.** Dropping `compress_neighbors` is lossless by construction: libsql converts a 1-bit
source to float8 by mapping each bit to `±1` and quantizing, giving exactly byte 0 or 255, so
the float8 cosine distance is an exact zero-intercept linear rescaling of Hamming distance
(`F8_distance = (2/dims) × Hamming`). RobustPrune's keep/drop decision is a *ratio* between two
distances computed the same way, so it is invariant to the substitution. Dropping
`max_neighbors` is a real change — degree 64 → 51, with no compensating change to the search
beam (`insert_l`/`search_l` are fixed constants, unrelated to `max_neighbors`) — so it was
measured, not argued: 6,000 clustered 1024-bit vectors, 200 held-out queries, recall@10 against
exact brute-force ground truth, **6 independent index builds per configuration**:

| Configuration | mean recall@10 | sd | min | max |
|---|---|---|---|---|
| v5 (`float8`, 64) | 0.5714 | 0.1164 | 0.3190 | 0.6625 |
| v6 (1-bit, 51) | 0.5792 | 0.0609 | 0.5085 | 0.6695 |

The difference in means is 0.15 standard errors — no detectable regression, and v6 is nominally
better with half the variance.

> **Benchmarking DiskANN here requires repeated builds.** `diskAnnSelectRandomShadowRow` picks
> the graph traversal's entry point with SQL `RANDOM()`, and it is called *per inserted row* and
> *per query*. Index construction is therefore nondeterministic, and its quality is bimodal:
> across the 12 builds above, most landed near 0.6 but one v5 build landed at 0.32. A single
> before/after comparison samples that distribution once and can show a 2× swing in either
> direction — an earlier single-run pass of exactly this benchmark appeared to show a 32%
> regression that repeated trials showed did not exist. Never gate a tuning change on one build.

Schema v6 (`shrink_vector_index`) rebuilds an existing binary index into the new shape. The
rebuild reads `chunks.embedding` directly — **no re-embedding and no model download** — but
it is a DiskANN insert per chunk, so it is a long operation on a large store. The freed
pages land on SQLite's freelist, so the file does not shrink until `localdb db vacuum` runs;
this is why the `VACUUM` attempted in #177, *before* any rebuild had freed anything,
correctly reclaimed nothing.
