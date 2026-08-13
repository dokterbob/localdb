# ADR 07 — Blocks Replace Markdown as Canonical IR

> Status: accepted, 2026-06-30. Supersedes the "normalized Markdown is the sole content IR" decision
> in [02-domain-model.md](02-domain-model.md) §2.

## Context

The MVP pipeline normalizes every source format to a single Markdown string
(`ParsedDocument.markdown`). Chunks are byte-range slices of that string, and `heading_path` is
derived on demand from Markdown headings. This was intentional: the prior `Block`/`BlockKind`
representation was removed in the Markdown-native migration (commit `3da56d0`) to simplify the
pipeline.

The project is now expanding beyond page-like documents to conversations (Telegram, Signal, email),
feeds (Atom/RSS), transcripts, and API objects (Notion, HackMD). These content shapes expose
fundamental limitations of the Markdown-as-IR model:

1. **Conversations, feeds, and transcripts are distorted when forced into Markdown and chunked as
   prose.** A chat thread is not a document with headings; a transcript is not a sequence of
   paragraphs.

2. **Page-like documents contain multiple distinct text regions** (body, tables, sidebars, captions,
   headers) that are not well-represented by a single Markdown string.

3. **Source-location metadata** (page number, bounding box, transcript timestamp, message ID) needs
   first-class representation, not derivation from Markdown byte offsets.

4. **The ingestor/parser closest to the source semantics is best placed to emit meaningful blocks.**
   Forcing every parser to serialize to Markdown and then re-parsing for structure loses information
   at the boundary.

5. **Content should be indexable even when not part of "main text" rendering** (e.g. table captions,
   OCR sidebars, attachment descriptions).

## Decision

Markdown is no longer the canonical intermediate representation. The new pipeline is:

```
Ingestor → Resource → ordered Blocks → block-local Chunks → Embeddings/Search → Citations
```

Markdown becomes an optional rendering/export format and may be used as contextual input for
embedders, but it does not define canonical structure, citation anchors, or chunk boundaries.

### Core Invariants

1. A **Resource** has a stable identity (`Uri` + `external_id`), metadata (`Metadata` enum), and an
   ordered list of **Blocks**.

2. A **Block** has a stable ID within the resource (`resource_id + block_seq`), a `BlockKind`,
   canonical text content, optional source-location data (`BlockLocation`), and an explicit sequence
   number.

3. Blocks are ordered within a resource. The order semantics depend on `ResourceKind`: logical
   reading order (documents), conversation order (chats/email), transcript time order
   (transcription), or source-defined API order.

4. A **Chunk** is a subdivision of exactly one block. Chunk location =
   `(store_id, resource_id, block_seq, seq_in_block)` — there is no `block_id` row reference; the
   block is addressed by sequence number (#128). Chunks never cross block boundaries.

5. **Exception: message-window chunks** span multiple `Message`/`Segment` blocks. The sliding window
   is an explicit multi-block chunking mode, not a violation of the invariant — the window itself is
   the logical unit being indexed.

6. Every chunk can resolve back to `{resource, block, chunk position}` for citation/navigation.

7. Context expansion is a first-class read-side capability: given a hit, the backend can fetch
   neighboring chunks in the same block, nearby blocks in the same resource, and the containing
   section/hierarchy.

### Ontology axes: kind ⊥ role ⊥ group (2026-07-20)

> Status: `kind` accepted and implemented. `role` and `group` are named future direction, not
> implemented.

The block ontology has three **orthogonal** axes. Today, only `kind` is specified and implemented;
`role` and `group` are recorded here as the accepted shape to grow into, so that a future
layout-aware extractor has a frame to fill in rather than inventing one ad hoc.

- **`kind`** — the structural type of an element: `Heading`, `Text`, `Table`, `Code`, `Image`, and
  reserved `Message`/`Segment`/`Frontmatter`/`Reference`/`Attachment`. **`Text`** is the coarse
  running-body-text kind that folds the former `Paragraph`/`Quote`/`List` variants (see "Coarsening
  the text kind" below). A `kind` names _what an element is_, never its writing style or layout role
  — this is precisely why the coarse text kind is called `Text`, not `Prose` or `Body`: a pull-quote
  or an inset are still `Text` in `kind`, just a different `role`.
- **`role`** — layout function within the page: `Body` (default), plus future
  `Inset`/`Sidebar`/`Caption`/`PullQuote`/`Footnote`/etc. **Future direction, not implemented.**
  Only a layout-aware extractor (e.g. a PDF layout parser, an HTML reader with region detection) can
  populate anything other than `Body`; Markdown-sourced output is always `Body`.
- **`group`** — article/column containment, for documents that hold more than one logical article or
  column on a single page (e.g. a newspaper-style layout, a multi-story feed page rendered as one
  document). **Future direction, not implemented.** No producer exists for it until a layout-aware
  source is built.

`role` and `group` are deliberately **not** pre-baked into the schema or block types now, without a
real layout-aware extractor available to validate the shape against actual data; adding them later,
once such an extractor exists, is an accepted future migration rather than a gap in this ADR. They
are tracked in a dedicated spec issue (#160).

**Markdown is the anchor ontology.** `markdown_to_blocks` is the reference producer of the `kind`
axis. When other formats eventually move off Markdown-as-IR to native block emission (the per-format
block emission evolution already described under "Alternatives considered" below), those producers
must **reproduce this same `kind` ontology** — Markdown's block output does not change as a result.
This is what keeps `kind` a stable, producer-independent axis.

**Coarsening the text kind.** The concrete `kind`-axis behavior implemented today is the coarsening
boundary rule: a `Text` block is the run of consecutive running-text content (paragraphs, lists,
blockquotes, HTML blocks) between structural boundaries — a `Heading`, a `Table`/`Code`/`Image`
block, or the start/end of the document always breaks a run into a new block. See
[04-search-pipeline.md](04-search-pipeline.md) §3 ("Block-dispatch rules") for the chunker-facing
consequences (prose chunking within a `Text` block, headings staying discrete and chunked).

### Canonical Text, Hashing, Embedding

- Every block has a **canonical text** representation, including tables (text rendering), references
  (target + label), attachments (filename + description), and OCR regions.

- **Resource content hash** = blake3 of ordered block canonical contents concatenated. Not dependent
  on Markdown rendering.

- **Chunk hash** = blake3 of `resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block`, computed after
  `block_seq`/`seq_in_block` are assigned.

- **Block hash** is not content-addressed globally — blocks are identified by `(resource_id, seq)`,
  which is stable as long as the resource content doesn't change.

- **Embedding input:** The embedder receives chunks grouped by resource, with block/resource context
  for late-chunking. An embedding renderer may serialize nearby blocks or the full resource into
  Markdown-like context as an implementation detail. Actual indexed chunks remain block-local.

### Block Durability and Versioning

Blocks are **derived content** (extracted from source material by parsers/ingestors), not
authoritative source truth. They can be regenerated by re-running the ingestor — unless the source
can no longer be re-acquired.

The `resources.extractor_version` column tracks which parser/ingestor version produced the blocks.
When parser logic improves, resources with a stale `extractor_version` can be marked for
reprocessing.

### Metadata Taxonomy

Two categories, kept separate:

- **Search/filter metadata** (indexed columns): resource kind, ingestor kind, language, date,
  participants, channel, mime type, tags, external identity.

- **Location/navigation metadata** (on blocks and chunks, for citation/navigation): page number,
  bounding box, transcript timestamp, message ID, URI fragment, table cell range. Not primary search
  filters.

## Alternatives considered

**Parsers emit blocks directly, instead of `ParsedDocument` + `markdown_to_blocks()`.** Considered
and rejected for now. The `Parser` trait keeps returning `ParsedDocument` (a Markdown string +
title + `DublinCoreMetadata`, [02-domain-model.md](02-domain-model.md) §8); ingestors convert it to
blocks via `markdown_to_blocks()` at the ingestion boundary. Blocks are still the canonical IR at
that boundary — `ParsedDocument` never leaks past the file ingestor — but every existing
format-specific parser (Markdown, plain text, HTML, PDF) keeps its simple "produce normalized
Markdown" contract instead of each one independently emitting typed blocks. **Rejected (for now):**
blocks-emitting parsers — more accurate per-format structure (e.g. a table parser emitting a real
`Table` block instead of a Markdown table rendering), but it multiplies the work of every future
parser and duplicates block-construction logic across parsers instead of centralizing it in one
`markdown_to_blocks()` implementation. Revisit per-format block emission opportunistically (e.g.
HTML, PDF) once the block model has proven itself; it does not require another ADR to introduce for
a single format, since `Parser` returning blocks directly is already an accepted future evolution of
this same interface.

## Consequences

- `ParsedDocument` (Markdown string + title + `DublinCoreMetadata`) is no longer the canonical
  pipeline representation — `Resource` (metadata + ordered blocks) is. `ParsedDocument` survives
  only as the file ingestor's internal parser-output type (see "Alternatives considered" above).
- `Document` (the stored entity) is replaced by `Resource`.
- The chunker receives blocks, not a Markdown string. Chunk dispatch is by `BlockKind`, not by
  source preset.
- `heading_path` is derived from the block tree (heading blocks preceding content blocks), not from
  re-parsing Markdown.
- The `Span` type (byte range into a Markdown string) is replaced by `ChunkLocation` (block ref +
  position within block).
- Citation anchors reference `{resource, block, chunk}` instead of byte offsets into Markdown.
- The schema gains a `blocks` table (normalized, for context expansion) and the `documents` table
  becomes `resources`.
- Existing parsers continue to emit Markdown, which is converted to blocks via
  `markdown_to_blocks()`. Per-format block extraction improves iteratively.
- The `messages` chunking preset becomes implementable: it operates on `MessageBlock`/`SegmentBlock`
  sequences with sliding windows.

## Status

Accepted. This ADR is the design authority for the block model. Implementation details are in
[02-domain-model.md](02-domain-model.md) (types), [04-search-pipeline.md](04-search-pipeline.md)
(pipeline), and [01-architecture.md](01-architecture.md) (crate boundaries).
