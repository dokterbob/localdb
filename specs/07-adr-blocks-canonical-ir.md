# ADR 07 — Blocks Replace Markdown as Canonical IR

> Status: accepted, 2026-06-30. Supersedes the "normalized Markdown is the sole content IR"
> decision in [02-domain-model.md](02-domain-model.md) §2.

## Context

The MVP pipeline normalizes every source format to a single Markdown string
(`ParsedDocument.markdown`). Chunks are byte-range slices of that string, and
`heading_path` is derived on demand from Markdown headings. This was
intentional: the prior `Block`/`BlockKind` representation was removed in the
Markdown-native migration (commit `3da56d0`) to simplify the pipeline.

The project is now expanding beyond page-like documents to conversations
(Telegram, Signal, email), feeds (Atom/RSS), transcripts, and API objects
(Notion, HackMD). These content shapes expose fundamental limitations of the
Markdown-as-IR model:

1. **Conversations, feeds, and transcripts are distorted when forced into
   Markdown and chunked as prose.** A chat thread is not a document with
   headings; a transcript is not a sequence of paragraphs.

2. **Page-like documents contain multiple distinct text regions** (body,
   tables, sidebars, captions, headers) that are not well-represented by a
   single Markdown string.

3. **Source-location metadata** (page number, bounding box, transcript
   timestamp, message ID) needs first-class representation, not derivation
   from Markdown byte offsets.

4. **The ingestor/parser closest to the source semantics is best placed to
   emit meaningful blocks.** Forcing every parser to serialize to Markdown and
   then re-parsing for structure loses information at the boundary.

5. **Content should be indexable even when not part of "main text" rendering**
   (e.g. table captions, OCR sidebars, attachment descriptions).

## Decision

Markdown is no longer the canonical intermediate representation. The new
pipeline is:

```
Ingestor → Resource → ordered Blocks → block-local Chunks → Embeddings/Search → Citations
```

Markdown becomes an optional rendering/export format and may be used as
contextual input for embedders, but it does not define canonical structure,
citation anchors, or chunk boundaries.

### Core Invariants

1. A **Resource** has a stable identity (`Uri` + `external_id`), metadata
   (`Metadata` enum), and an ordered list of **Blocks**.

2. A **Block** has a stable ID within the resource (`resource_id + block_seq`),
   a `BlockKind`, canonical text content, optional source-location data
   (`BlockLocation`), and an explicit sequence number.

3. Blocks are ordered within a resource. The order semantics depend on
   `ResourceKind`: logical reading order (documents), conversation order
   (chats/email), transcript time order (transcription), or source-defined
   API order.

4. A **Chunk** is a subdivision of exactly one block. Chunk location =
   `{resource_id, block_id, chunk_seq_in_block}`. Chunks never cross block
   boundaries.

5. **Exception: message-window chunks** span multiple `Message`/`Segment`
   blocks. The sliding window is an explicit multi-block chunking mode, not a
   violation of the invariant — the window itself is the logical unit being
   indexed.

6. Every chunk can resolve back to `{resource, block, chunk position}` for
   citation/navigation.

7. Context expansion is a first-class read-side capability: given a hit, the
   backend can fetch neighboring chunks in the same block, nearby blocks in
   the same resource, and the containing section/hierarchy.

### Canonical Text, Hashing, Embedding

- Every block has a **canonical text** representation, including tables (text
  rendering), references (target + label), attachments (filename +
  description), and OCR regions.

- **Resource content hash** = blake3 of ordered block canonical contents
  concatenated. Not dependent on Markdown rendering.

- **Chunk hash** = blake3 of `{resource_id, block_id, chunk_text, chunk_range}`.

- **Block hash** is not content-addressed globally — blocks are identified by
  `(resource_id, seq)`, which is stable as long as the resource content
  doesn't change.

- **Embedding input:** The embedder receives chunks grouped by resource, with
  block/resource context for late-chunking. An embedding renderer may
  serialize nearby blocks or the full resource into Markdown-like context as
  an implementation detail. Actual indexed chunks remain block-local.

### Block Durability and Versioning

Blocks are **derived content** (extracted from source material by
parsers/ingestors), not authoritative source truth. They can be regenerated
by re-running the ingestor — unless the source can no longer be re-acquired.

The `resources.extractor_version` column tracks which parser/ingestor version
produced the blocks. When parser logic improves, resources with a stale
`extractor_version` can be marked for reprocessing.

### Metadata Taxonomy

Two categories, kept separate:

- **Search/filter metadata** (indexed columns): resource kind, ingestor kind,
  language, date, participants, channel, mime type, tags, external identity.

- **Location/navigation metadata** (on blocks and chunks, for
  citation/navigation): page number, bounding box, transcript timestamp,
  message ID, URI fragment, table cell range. Not primary search filters.

## Consequences

- `ParsedDocument` (Markdown string + title + Dublin Core metadata) is
  replaced by `Resource` (metadata + ordered blocks).
- `Document` (the stored entity) is replaced by `Resource`.
- The chunker receives blocks, not a Markdown string. Chunk dispatch is by
  `BlockKind`, not by source preset.
- `heading_path` is derived from the block tree (heading blocks preceding
  content blocks), not from re-parsing Markdown.
- The `Span` type (byte range into a Markdown string) is replaced by
  `ChunkLocation` (block ref + position within block).
- Citation anchors reference `{resource, block, chunk}` instead of byte
  offsets into Markdown.
- The schema gains a `blocks` table (normalized, for context expansion) and
  the `documents` table becomes `resources`.
- Existing parsers continue to emit Markdown, which is converted to blocks
  via `markdown_to_blocks()`. Per-format block extraction improves
  iteratively.
- The `messages` chunking preset becomes implementable: it operates on
  `MessageBlock`/`SegmentBlock` sequences with sliding windows.

## Status

Accepted. This ADR is the design authority for the block model. Implementation
details are in [02-domain-model.md](02-domain-model.md) (types),
[04-search-pipeline.md](04-search-pipeline.md) (pipeline), and
[01-architecture.md](01-architecture.md) (crate boundaries).
