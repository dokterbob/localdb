# Spec 02 — Canonical Domain Model

> Status: accepted draft, revised 2026-06-30. All entities live in the `core` crate and are
> shared by every surface. Field lists are normative for meaning, not for exact Rust types.
>
> **Supersedes:** the Markdown-native IR model (commit `3da56d0`). The block model is
> reintroduced as the canonical intermediate representation — see
> [07-adr-blocks-canonical-ir.md](07-adr-blocks-canonical-ir.md) for the decision record.

## 1. Entity overview

```
Store 1──* Source 1──* Resource 1──* Block 1──* Chunk
                           │                       │
                      IndexJob            Citation (view over Chunk + Resource)
```

Ingestors produce **Resources** containing ordered **Blocks**. Each block has a `BlockKind`,
canonical text, and optional source-location metadata. The chunker operates on blocks (not a
Markdown string), and `heading_path` is derived from the block tree (heading blocks preceding
content blocks). Chunks never cross block boundaries, with one explicit exception: message-window
chunks span multiple `Message`/`Segment` blocks.

## 2. Entities

### Store
A named knowledge base. Unit of sharing, ACLs, indexing policy, and federation.

| Field | Notes |
|---|---|
| `id` | Stable ULID, minted at creation; never reused. |
| `name` | Human-readable, unique per instance. |
| `visibility` | `private` \| `shared`. MVP: only `private` functional; field exists from day one ([01-architecture.md](01-architecture.md) §5). |
| `backend` | Backend kind + connection info; default `libsql`. |
| `indexing` | Indexing policy: `{chunking, embedding, parsers}` as one unit ([03-config.md](03-config.md) §2). |
| `acl` | Reserved; empty in MVP. |

### Source
Where a store's content comes from. Each source is driven by an **ingestor** that knows how to
acquire and structure its content.

| Field | Notes |
|---|---|
| `id` | ULID. |
| `store_id` | Owning store. |
| `ingestor_kind` | Which ingestor drives this source: `file`, `url`, and future connectors (`notion`, `telegram`, `signal`, `hackmd`, `email`, `transcription`, `feed`). See [01-architecture.md](01-architecture.md) §1 for the `IngestorKind` enum. |
| `spec` | Kind-specific configuration: root path + globs, URL + refresh interval, API token reference, etc. Stored as JSON; validated by the ingestor's `IngestorConfig`. |
| `config_json` | Ingestor-specific configuration fields (typed per ingestor). |
| `source_kind_preset` | Which indexing preset applies (`prose`, `messages`, `code`) — see [03-config.md](03-config.md) §2. |

**Runtime representation:** `SourceRow` in `core::backend` is the concrete
Rust type for sources persisted in the unified database (`localdb.db`). Source CRUD is exposed
via `StoreBackend` methods (`upsert_source`, `delete_source`, `list_sources`, `get_source`,
`find_source_by_root_or_url`).

### Resource
One logical content unit produced by an ingestor. Replaces the former `Document` entity.
A resource is: a file, a fetched page, a Notion page, a conversation thread, a transcript,
a feed entry.

| Field | Notes |
|---|---|
| `id` | **Content-addressed**: `blake3(uri ‖ content_hash)` — see §3. |
| `source_id`, `store_id` | Ownership. |
| `ingestor_kind` | Which ingestor produced this resource (denormalized from source for queries). |
| `resource_kind` | `document` \| `conversation` \| `transcription`. Determines block ordering semantics. |
| `uri` | `Uri` newtype wrapping `url::Url`. Canonical locator (absolute path as `file://`, URL, or connector-defined scheme like `notion://`, `telegram://`). |
| `external_id` | Arbitrary source-system ID (Notion page ID, Telegram message ID, email Message-ID). Optional. |
| `external_etag` | Change detection token from the source system (HTTP ETag, Notion `last_edited_time`, file mtime). Optional. |
| `content_hash` | blake3 of ordered block canonical texts concatenated. Drives incremental re-index. Not dependent on Markdown rendering. |
| `title`, `mime`, `language` | From extraction. `language` is BCP 47. |
| `date_original` | Dublin Core date string (may be partial, e.g. `2026` or `2026-06`). |
| `date_parsed` | Best-effort ISO 8601 parse of `date_original` (sortable). |
| `added_at` | When first indexed (our timestamp, RFC 3339). |
| `modified_at` | When content last changed (RFC 3339). |
| `thread_id` | Conversation thread identifier (conversation resources only). |
| `channel` | Channel/folder/chat name (conversation resources only). |
| `participants` | JSON array of participant names/IDs (conversation resources only). |
| `metadata` | `Metadata` enum — see §7. Contains Dublin Core base fields plus resource-kind-specific fields. |
| `provenance` | See §4. |
| `extractor_version` | Version string of the parser/ingestor that produced the blocks. Enables reprocessing when extraction logic improves. |

### Block
A typed, ordered unit of content within a resource.

| Field | Notes |
|---|---|
| `resource_id` | Parent resource. |
| `seq` | Ordering within the resource (0-indexed). Stable as long as resource content doesn't change. |
| `kind` | `BlockKind` — see §2a. |
| `text` | Canonical text content of the block. Every block kind has a text representation. |
| `metadata_json` | Kind-specific structured metadata (e.g. heading level, sender, timestamp). |
| `location` | `BlockLocation` — optional source-location data for citation/navigation (§2b). |

**Identity:** blocks are identified by `(resource_id, seq)`, not content-addressed. They are
derived content that can be regenerated by re-running the ingestor.

### Feed connector (`SourceSpec::Feed`)

`ingestor_kind = feed` parses RSS 2.0, Atom 1.0, and JSON Feed via `feed-rs`, with `feed-rs`'s
`sanitize` feature (an `ammonia` pass over embedded entry HTML) always applied.

| Field | Notes |
|---|---|
| `url` | Feed URL. |
| `max_entries` | `Option<u32>`. Cap on entries considered per fetch, applied after the sort described below. `0` is rejected at config time, not treated as "index nothing." |
| `fetch_full_content` | `bool`, default `true`. Selects **discovery mode** (`true`) vs. **single-document mode** (`false`) — see below. |
| `refresh_interval_secs` | `Option<u64>`. Same shape and column (`sources.refresh`) as `SourceSpec::Url`. |

`{max_entries, fetch_full_content}` persist as JSON in `sources.config_json` — the column exists
at baseline (v4), so the feed connector needed no schema migration. `refresh_interval_secs`
persists in the pre-existing `sources.refresh` column alongside `url` sources.

**Discovery mode (default).** A feed is treated as a URL-discovery meta-wrapper, not itself the
indexed content: parse the feed, then for each entry resolve the entry's link and run it through
the *same* per-URL pipeline `UrlIngestor` uses for `url` sources (fetch page → parser chain →
blocks → `Resource`), via a shared `ingest/src/url_pipeline.rs` helper — a feed entry and a
directly-configured `url` source produce identically-shaped Resources. Entry metadata enriches the
resulting Resource: `external_id` = the entry's feed-native ID, `creator` = entry authors, the
metadata date = the entry's published/updated timestamp, `metadata.source` = the feed URL
(provenance back to the discovering feed), `external_etag` captured from the entry-link fetch. The
Resource's `uri` (and therefore its content-addressed `id`, §3) keys off the entry's
**pre-redirect, feed-declared link**, not wherever it 30x's to, so re-fetching the same feed
resolves to the same Resource identity regardless of redirect-target churn. That is a statement
about *identity*, distinct from *link resolution*: a relative entry link (e.g.
`<link>article.html</link>`) is resolved by `feed-rs` against the feed's **effective
(post-redirect) URL**, not the configured `feed_url` — a feed that 301's to a new host must
still resolve its entries' relative links against that new host, not the stale one it was
configured with. `xml:base`, where present in the feed XML, still takes precedence over that
base URI (feed-rs's own resolution rule). Once resolved, the link is the feed-declared link that
identity keys off, per the paragraph above — resolution decides *where a relative link points*;
identity decides *what URI names the Resource*, and stays pinned to that resolved link
regardless of where the linked page itself later redirects. Entries with no link get a fragment
URI instead: `{feed_url}#entry:{entry.id}`.

**General connector pattern.** Discovery mode plus the fragment-URI fallback is not feed-specific
— it's the expected shape for any ingestor that discovers sub-resource URIs from a parent
resource: enrich the discovered Resource from the parent's metadata, key its identity off the
discovered URI, and fall back to `{parent_uri}#fragment:{id}` when no addressable URI exists. Two
connectors on the roadmap ([06-roadmap.md](06-roadmap.md)) are expected to follow it: email
(#114, discovering message/attachment URIs from a mailbox) and conversation exports (#129,
discovering per-message permalinks from a thread).

**Single-document mode** (`fetch_full_content: false`): the whole feed becomes **one** Resource,
`uri` = the feed URL itself, assembled deterministically rather than fetched:

```
# {feed.title | "Untitled Feed"}

{feed.description, if present}

## {entry.title | "Untitled Entry"}

*By {authors} — {date RFC3339} — {link}*

{entry body: content, else summary, else nothing}

## {next entry.title | "Untitled Entry"}
...
```

The byline line omits missing parts outright — no placeholder text, and the entry's guid never
appears in it.

**Both modes:** entries are stable-sorted by `published.or(updated)` descending (entries with
neither date sort last, stable among themselves), then **deduplicated by resolved resource
URI** — the same URI that becomes the Resource's identity (the entry's resolved link, or the
`{feed_url}#entry:{entry.id}` fragment for a link-less entry) — keeping the first (i.e. newest)
survivor and dropping the rest, then truncated to `max_entries`. `max_entries` therefore counts
*distinct* resource URIs, not raw entry count: two entries that resolve to the same URI cost one
slot, not two. First-wins (not last-wins) is deliberate: when a feed lists the same URI more
than once, the newest listing is the feed's most current claim about that URI, so it is the one
that should win whichever content ends up indexed for it.

**Timestamps.** Feed-produced Resources map times as follows. `added_at` is always ingestion-time
`now()` — it records when *our store* first saw the resource, never a feed-claimed date.
`modified_at` comes from the feed when it says anything: per entry `updated.or(published)`
(discovery mode and the embedded fallback), and in single-document mode `feed.updated`, else the
newest entry's date, else `now()`. Creation/publication stays in `dublin_core.date` =
`published.or(updated)` (the conventional DC slot) — note the opposite preference order from
`modified_at`, matching each field's semantics. Like all enrichment, an already-indexed entry
whose content hash is unchanged does not retroactively pick these up (the pipeline's
incremental-skip runs before any store write).

**Fallback and error handling:**
- Discovery mode, entry-link fetch returns `Gone`/`Unsupported`: falls back to indexing the
  entry's own embedded content/summary at the same URI, instead of dropping the entry.
- Discovery mode, entry-link fetch returns a transient `FetchError`/`ParseFailed`: no fallback —
  reported as a per-item error and skipped this run. Falling back here would flip the Resource's
  content hash between "full page" and "feed summary" on every transient outage, forcing needless
  re-embedding; the existing good index is left alone instead.
- A fetched entry page — or, having fallen back that far, the entry's own embedded content — that
  extracts to empty or whitespace-only Markdown is **unusable, not empty**: it falls through the
  same `content → summary → title` chain that `Gone`/`Unsupported` use, and never yields a
  zero-block Resource. A zero-block Resource reaching `index_resource` is indistinguishable from
  a legitimate empty replacement and deletes the previously indexed document (see
  [04-search-pipeline.md](04-search-pipeline.md) §1) — exactly the hazard the embedded-content
  chain's own empty-Markdown guards (`feed_ingestor.rs`'s `entry_routed_content`) exist to avoid,
  extended here to the fetched-page path that shares the same fallthrough logic.
- An invalid feed URL in config fails the whole source run fast (`invalid_config`). Everything
  else data-driven — malformed feed XML, malformed entries, entry-link fetch failures — is
  per-item `on_skipped(Error)` + continue. An empty feed (zero entries) is valid, not an error.
- Feed autodiscovery from HTML pages (`<link rel="alternate">`) is out of scope.

**Retention:** feed sources are exempt from the pipeline's delete-sweep — a feed exposes only its
most recent entries, so an entry falling out of the feed does not mean it was deleted upstream,
and a feed `304`/transient-empty-parse would otherwise wipe an entire source's index in one sweep.
`source remove` still cascades normally. See
[docs/architecture.md#known-gaps](../docs/architecture.md#known-gaps) for the resulting archive
semantics and the pruning follow-up.

**Ordering semantics** depend on `ResourceKind`:
- `document` — logical reading order
- `conversation` — chronological message order
- `transcription` — transcript time order

### §2a. BlockKind

| Kind | Text content | Metadata fields | Typical sources |
|---|---|---|---|
| `Heading` | Heading text | `level: u8` (1–6) | Documents, Notion pages |
| `Text` | Running body text (coarse) | — | Documents, HTML, Notion |
| `Code` | Code content | `language: Option<String>` | Markdown fences, Notion code blocks |
| `Table` | Text rendering of table | `headers: Vec<String>`, `rows: usize` | Documents, spreadsheets |
| `Message` | Message body text | `sender: String`, `timestamp: Option<String>`, `message_id: Option<String>`, `reply_to: Option<String>` | Conversations (chat, email) |
| `Segment` | Transcript segment text | `speaker: Option<String>`, `start_ms: u64`, `end_ms: u64` | Transcriptions (SRT, VTT, Whisper) |
| `Reference` | `"[label](target)"` | `target: String`, `label: Option<String>`, `ref_type: Option<String>` | Wikilinks, Notion mentions, citations |
| `Attachment` | `"filename: description"` | `filename: String`, `mime: Option<String>`, `size_bytes: Option<u64>` | Email attachments, Notion files |
| `Frontmatter` | Raw frontmatter text | `format: String` (yaml/toml/json) | Markdown, Obsidian |
| `Image` | Alt text or OCR text | `alt: Option<String>`, `src: Option<String>` | Documents with images |

**Coarse `Text` kind:** `Text` is the single running-body-text kind; it folds the former
`Paragraph`/`Quote`/`List` variants. `markdown_to_blocks` emits one `Text` block per run of
consecutive running-text content (paragraphs, lists, blockquotes, HTML) between structural
boundaries (`Heading`/`Table`/`Code`/`Image`/document start-end). Rationale and chunker
consequences: [04-search-pipeline.md](04-search-pipeline.md) §3 and
[07-adr-blocks-canonical-ir.md](07-adr-blocks-canonical-ir.md) ("Ontology axes: kind ⊥ role ⊥
group").

### §2b. BlockLocation

Source-location metadata for citation and navigation. Not all fields apply to every block kind.

| Field | Notes |
|---|---|
| `page` | Page number (1-indexed, for PDFs and paginated documents). |
| `bbox` | Bounding box `{x, y, width, height}` (for PDFs with layout). |
| `section` | Section identifier or path (e.g. `["Chapter 1", "Introduction"]`). |
| `line_start`, `line_end` | Line range in source file (for code and plain text). |
| `uri_fragment` | URI fragment (e.g. `#heading-id` for HTML). |

### Chunk
The retrieval unit: what gets embedded and indexed.

| Field | Notes |
|---|---|
| `id` | **Content-addressed**: `blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)` — stable across re-runs over identical content. Computed *after* `block_seq`/`seq_in_block` are assigned, so both are inputs to the hash, not derived from it. |
| `resource_id`, `store_id` | Ownership. |
| `block_seq` | Sequence number of the parent block (denormalized, for efficient ordering without a join). |
| `seq_in_block` | Chunk position within the block (0-indexed). |
| `text` | Chunk text (also feeds BM25). |
| `heading_path` | Derived from the block tree: heading blocks preceding this content block. JSON array. |
| `embedding` | Dense vector (in backend, not in core serialization). |
| `location` | `ChunkLocation` — refined sub-block position (optional). Persisted as `location_json`: `{"start": N, "end": N, "window_block_seqs": [..]}`, with `window_block_seqs` absent/optional for non-window chunks. `ChunkRecord` carries this as `window_block_seqs: Vec<u32>` (`#[serde(default)]`). |

**Invariant:** a chunk is a subdivision of exactly one block. The canonical block reference is the
triple **`(store_id, resource_id, block_seq)`** — there is no `block_id` column; blocks are looked
up by sequence number, not by a synthetic row reference. The chunks index is
`(store_id, resource_id, block_seq, seq_in_block)`. Chunks never cross block boundaries.

**Why not `block_id`:** an earlier revision of this schema referenced the parent block via
`chunks.block_id` (a `blocks.rowid` foreign key). That column is dropped (#128): rowids are not
stable across a replace (delete+insert of a resource mints new block rows), and window chunks
(#129) need to reference a *set* of block sequence numbers, which a single scalar foreign key
cannot express. `(store_id, resource_id, block_seq)` is stable and generalizes to sets.

**Span semantics:** Chunk spans (`location.start`, `location.end`) are **block-relative byte
offsets** — they index into the parent block's `text`, not the full document Markdown. Combined
with `block_seq`, they provide a complete location: `(resource_id, block_seq, span)`.
Document-relative offsets are not stored or computed.

**Exception — message-window chunks:** the `messages` chunking preset creates chunks that span
multiple `Message`/`Segment` blocks via a sliding window. This is an explicit multi-block
chunking mode. The `ChunkLocation` carries `window_block_seqs`: the set of participating block
sequence numbers. `window_block_seqs` is present only for this chunk shape — absent for
single-block chunks.

### Citation
Not a stored entity: the **canonical result shape** every surface uses (§6).

### IndexJob
A unit of indexing work with observable state. Fields: `id` (ULID), `store_id`, `scope` (full
store / one source / one resource), `state` (`pending` → `running` → `done` | `failed`),
`stats` (resources seen/indexed/deleted, chunks written), `error`, timestamps. Embedded mode
runs jobs synchronously but still records them; the daemon queues them
([05-surfaces.md](05-surfaces.md) §3).

## 3. ID scheme

**Decision:** entities that exist by fiat (Store, Source, IndexJob) get **ULIDs**; entities
derived from content (Resource, Chunk) get **content-addressed blake3 IDs** as defined above.

**Rationale:** content-addressed IDs are the federation prerequisite — two nodes indexing the same
content derive the same chunk identity, enabling dedup, provenance comparison, and integrity
checks without coordination ([VISION.md](../VISION.md)). They also make re-indexing idempotent.
**Rejected:** auto-increment rows (meaningless off-node); UUIDv4 for resources/chunks (stable
only by table lookup, not by content).

Consequence: a resource edit produces a *new* resource ID; the pipeline treats it as
replace-by-URI (delete chunks of the old ID, insert new) — see
[04-search-pipeline.md](04-search-pipeline.md) §2.

**Block identity:** blocks are identified by `(resource_id, seq)`, not content-addressed.
They are derived content — stable as long as the resource content and extractor version don't
change. When the resource is re-ingested, blocks are replaced entirely.

## 4. Provenance

Every resource and every chunk carries:

| Field | Notes |
|---|---|
| `origin_store` | Store ID where it was first indexed (≠ current store after future federation). |
| `source_ref` | Source ID + ingestor kind. |
| `fetched_at` | Acquisition time (file mtime at scan / HTTP fetch time). |
| `content_hash` | blake3 of resource content (ordered block texts concatenated). |
| `share_path` | Reserved, empty in MVP: list of (node, store) hops for federated content. |

**Write path.** A chunk's `fetched_at` is always taken from its resource's `added_at`, never its
`modified_at` — it is persisted as `resources.added_at`, and that is the column
`MetadataFilter::FetchedAfter`/`FetchedBefore` filter on and every citation's
`provenance.fetched_at` reports.

## 5. Conversations and non-document resources

The resource model natively supports non-document content shapes:

- **Conversations** (chat, email): `resource_kind = conversation`. Each message is a `Message`
  block with sender, timestamp, and message ID. Thread identity via `thread_id` on the resource.
  Chunked by the `messages` preset (sliding turn windows).
- **Transcriptions** (SRT, VTT, Whisper JSON): `resource_kind = transcription`. Each segment is
  a `Segment` block with speaker, start/end timestamps. Chunked by time windows respecting
  speaker boundaries.
- **Documents** (files, web pages, Notion pages): `resource_kind = document`. Blocks follow
  logical reading order. Chunked by the `prose` or `code` presets dispatched per block kind.

Metadata is resource-kind-specific via the `Metadata` enum (§7), not open key-value `meta` keys.

## 6. Citation model

Every search hit, on every surface, resolves to the same citation structure:

```json
{
  "resource_id": "...",
  "uri": "...",
  "title": "...",
  "block": { "seq": 3, "kind": "paragraph" },
  "chunk_position": { "seq_in_block": 0 },
  "location": {
    "span": { "start": 120, "end": 512 },
    "window_block_seqs": [3, 4, 5]
  },
  "score": 0.87
}
```

That's the shape of the field list distinctive to the block model; the full `Citation` also
carries `chunk_id`, `store: {id, name}`, `heading_path`, `snippet` (chunk text, possibly trimmed),
the full `score: {fused, dense, bm25}` breakdown, `provenance: {fetched_at, content_hash}`, and
`metadata` (the tagged `Metadata` enum — Dublin Core base + resource-kind-specific fields, §7).
There is no top-level `document_id`, `block_seq`, `block_kind`, or `span` — those are superseded by
`resource_id`, the nested `block {seq, kind}`, `chunk_position {seq_in_block}`, and
`location {span, window_block_seqs}` respectively. `window_block_seqs` is present only for
message-window chunks (§2); absent otherwise.

Surface mappings — defined here once, referenced by [05-surfaces.md](05-surfaces.md):
**HTTP** returns the structure verbatim as JSON. **CLI** renders `uri` + heading path + snippet
(and full JSON with `--json`). **MCP** returns it as structured tool output content, never as
prose-only text, so agents can cite mechanically.

**Context expansion:** given a search hit, the backend supports:
1. Neighboring chunks in the same block (`chunks WHERE store_id = ? AND resource_id = ? AND block_seq = ? ORDER BY seq_in_block`)
2. Nearby blocks in the same resource (`blocks WHERE resource_id = ? AND seq BETWEEN ? AND ?`)
3. Full resource block sequence (`blocks WHERE resource_id = ? ORDER BY seq`)

## 7. Metadata taxonomy

### DublinCoreMetadata (base for all resource kinds)

Dublin Core Metadata Element Set 1.1 (DCMES), all 15 elements. Repeatable elements
(multi-valued) use `Vec<String>`; singleton elements use `Option<String>`.

| Element | Type | Notes |
|---|---|---|
| `title` | `Option<String>` | Title of the resource. |
| `creator` | `Vec<String>` | Repeatable: authors, creators. |
| `subject` | `Vec<String>` | Repeatable: topics, keywords. |
| `description` | `Option<String>` | Summary or abstract. |
| `publisher` | `Option<String>` | Entity responsible for making the resource available. |
| `contributor` | `Vec<String>` | Repeatable: additional contributors. |
| `date` | `Option<String>` | Date of creation or publication (ISO 8601 recommended). |
| `r#type` | `Option<String>` | Nature or genre of the resource. |
| `format` | `Option<String>` | File format or media type. |
| `identifier` | `Option<String>` | Unambiguous reference (URL, DOI, ISBN, …). |
| `source` | `Option<String>` | Source resource this document is derived from. |
| `language` | `Option<String>` | Language of the resource (BCP 47 recommended). |
| `relation` | `Vec<String>` | Repeatable: related resources. |
| `coverage` | `Option<String>` | Spatial or temporal extent. |
| `rights` | `Option<String>` | Rights statement or license. |

### Metadata enum

```rust
enum Metadata {
    Document(DocumentMetadata),       // DC base + document-specific fields
    Conversation(ConversationMetadata), // DC base + conversation-specific fields
    Transcription(TranscriptionMetadata), // DC base + transcription-specific fields
}
```

Each variant embeds `DublinCoreMetadata` and adds kind-specific fields:

- **DocumentMetadata**: `page_count: Option<u32>`, `word_count: Option<u32>`.
- **ConversationMetadata**: `platform: Option<String>`, `message_count: Option<u32>`,
  `date_range: Option<(String, String)>`.
- **TranscriptionMetadata**: `duration_ms: Option<u64>`, `speakers: Vec<String>`,
  `media_uri: Option<String>`.

All variants expose `fn dublin_core(&self) -> &DublinCoreMetadata` for uniform access to the
base metadata fields.

**Persistence:** `Metadata` is JSON-encoded into a single `TEXT` column named `metadata_json`
on each resource record in libsql. The discriminant is the `Metadata` enum variant tag (e.g.
`{"kind":"document","dublin_core":{...},"page_count":...}`).

**Metadata unification (#130):** the flat, parser-level `DocumentMetadata` struct (a bare
15-element Dublin Core struct that lived in the parser boundary and was easily confused with the
same-named `DocumentMetadata` variant payload above) is retired. `ParsedDocument.metadata` is
`DublinCoreMetadata` directly — the same base type every `Metadata` variant embeds — so there is
exactly one Dublin-Core-shaped struct in the codebase, not two. Resources, chunks, and citations
all carry the tagged `Metadata` enum; nothing downstream of parsing sees the untagged flat form.

## 8. Extraction & parsing

### Ingestor trait (acquisition + structuring)

The `Ingestor` trait (`core/src/ingestor.rs`) is the abstraction for content acquisition and
structuring. Each ingestor knows how to connect to a source, enumerate content, and produce
`Resource`s with typed blocks.

| Method | Signature | Notes |
|---|---|---|
| `kind` | `(&self) -> IngestorKind` | Which ingestor kind this is. |
| `ingest` | `(&self, source, config) -> impl Stream<Item = Result<Resource, Error>>` | Async stream yielding resources. |

**IngestorKind** enum: `File`, `Url`, `Notion`, `Telegram`, `Signal`, `HackMd`, `Email`,
`Transcription`, `Feed`. The enum lives in `core`; concrete ingestor implementations live
outside `core` (in `cli`, dedicated crates, or a future `ingest` crate).

**Crate boundary:** `core::Ingestor` is the contract (yields `Resource`s). Terminal interaction,
credential prompts, HTTP/API clients, and source-specific setup live outside `core`, consistent
with the "no I/O frameworks in core" invariant ([01-architecture.md](01-architecture.md) §1).

### Parser chain (file-ingestor implementation detail)

The `Parser` trait remains as the abstraction for format-specific text extraction within the
**file ingestor**. Parsers now return `Resource` (with typed blocks) instead of
`ParsedDocument`. The `markdown_to_blocks()` helper converts Markdown pulldown-cmark events
to typed blocks, so existing parsers can emit Markdown as before and convert at the boundary.

Each `Parser` is `Send + Sync` and runs synchronously (CPU-bound); callers run it
under `spawn_blocking`. Two methods:

| Method | Signature | Notes |
|---|---|---|
| `id` | `(&self) -> &'static str` | Stable string used in the `parsers:` config list and diagnostics. |
| `parse` | `(&self, &Probe) -> Result<Option<ParsedDocument>, Error>` | See contract below. |

**Contract — three outcomes:**

- `Ok(None)` — decline; this parser does not handle the input. Control passes to the next
  parser in the chain.
- `Ok(Some(doc))` — handled successfully. First match wins; remaining parsers are not tried.
- `Err(e)` — the format was recognized but parsing failed. **Short-circuits the chain** —
  remaining parsers are NOT tried, because the failure is definitive, not a format mismatch.

`ChainParser` implements this same `Parser` trait (Composite pattern), holding an ordered
`Vec<Box<dyn Parser>>`. It is itself a `Parser` and can be nested. `build_chain(ids)` in
`extract/src/registry.rs` maps the config `parsers:` strings to concrete `Parser` instances.

### Probe

`Probe` is the fully-buffered input presented to each parser. The streaming or HTTPS read
happens once at the ingestion boundary; parsers never seek or re-fetch.

| Field / method | Notes |
|---|---|
| `bytes` | Full document bytes. |
| `path_hint: Option<&str>` | Original filename or URL path — used for file-extension hints. Advisory; may be absent. |
| `sniffed_mime: Option<&str>` | MIME type inferred before parsing. Advisory; may be wrong or `None`. |
| `header()` | Up to `PROBE_HEADER_LEN` (8 192) leading bytes for cheap magic-byte sniffing. |

### ParsedDocument → Resource conversion

`ParsedDocument` remains as the parser output: a Markdown string + title + `DublinCoreMetadata`.
The file ingestor converts it to a `Resource` by:
1. Running `markdown_to_blocks()` on the Markdown string to produce typed blocks.
2. Wrapping `ParsedDocument.metadata` (`DublinCoreMetadata`) into
   `Metadata::Document(DocumentMetadata { dublin_core, page_count, word_count })` — see §7.
3. Computing the content hash from ordered block texts.

This conversion is a compatibility bridge. Future parsers and ingestors can emit blocks directly.

## 9. Storage schema design rationale

The unified database schema uses several design patterns to ensure referential integrity and
query performance:

- **Composite Uniqueness:** The `resources` and `chunks` tables use composite `(store_id, id)`
  uniqueness. Content-addressed IDs can collide across stores by design. Each store maintains
  its own rows. Cross-store deduplication is deferred to query-time `GROUP BY` operations.
- **Normalized Blocks:** The `blocks` table stores individual blocks as rows (not a JSON blob),
  enabling efficient context expansion queries (fetch neighboring blocks for a search hit).
- **Denormalised Store ID:** The `store_id` column is denormalised onto the `chunks` table for
  per-store filtering directly on the rowid lookup after vector or FTS5 searches.
- **Block Reference on Chunks:** Each chunk references its parent block via denormalized
  `block_seq` (no `block_id`/rowid foreign key — see §2), enabling block-level context expansion
  without an extra join, on the composite index `idx_chunks_store_resource_pos
  (store_id, resource_id, block_seq, seq_in_block)`.
- **FTS5 Content Keying:** The FTS5 virtual table `chunks_fts` uses external content keying
  over `chunks.text`. Filtering by `store_id` is performed on the `chunks` join.
- **Cascade Chain:** Foreign keys with `ON DELETE CASCADE` across the chain:
  `stores → sources → resources → blocks → chunks`. Deleting a store cleans up everything.
- **Schema Versioning:** A `schema_migrations` table is the **source of truth** for schema
  version; `PRAGMA user_version` is kept in lockstep as a cheap head marker but is never
  authoritative. Columns:

  | Column | Notes |
  |---|---|
  | `version` | `INTEGER PRIMARY KEY`. Baseline is 4 (`BASELINE_VERSION`, the last pre-migration schema); the chain starts at 5. |
  | `name` | Short migration identifier. |
  | `applied_at` | RFC 3339 timestamp. |
  | `down_sql` | JSON array of statements that reverse this migration, or `NULL` if not mechanically reversible. |
  | `down_unsupported_reason` | Human-readable reason downgrade past this step is refused, or `NULL` if `down_sql` is set. |
  | `checksum` | `blake3` over the migration's version, name, rendered up-SQL, and rendered down-SQL (or reason). Verified on every open, bounded to the already-applied prefix before `db migrate` applies anything new, and again over the full chain afterward; a mismatch is an `internal` error, not a silent continue. Verification also requires a row to *exist* for the baseline and every applicable chain version (not just checking whatever rows happen to be present), and that each row's stored `name`/`down_sql`/`down_unsupported_reason` still match the compiled migration even when its `checksum` column reads correctly — a missing or tampered-but-checksum-intact row is treated the same as a checksum mismatch. `db downgrade` similarly requires the row history between its target and the current version to be contiguous before replaying anything. |

  `CHECK` constraint: exactly one of `down_sql` / `down_unsupported_reason` is set per row.

  **Open never migrates**, in either direction, on any surface. A version mismatch on open is a
  refusal (`invalid_config`, exit 2) with an actionable hint, not an automatic fix:
  - Legacy `0 < version < 4` (v1–v3): refused; hint points at `localdb db migrate` (which rebuilds
    destructively, behind a confirmation prompt) or deleting the database. Previously these
    versions triggered silent reinitialization on open; nothing is silent now.
  - `4 <= version < head` (pending migrations): refused; hint points at `localdb db migrate`.
  - `version > head` (store newer than this binary): refused; hint points at
    `localdb db downgrade` or upgrading localdb.

  Migrations are applied only via the explicit `localdb db migrate` / `localdb db downgrade [--to
  N]` CLI commands ([05-surfaces.md](05-surfaces.md) §2) — never by the HTTP daemon or MCP, which
  only ever surface the refusal-with-hint.

  **Downgradable by older binaries:** every migration's rendered down-SQL is stored *as data* in
  `schema_migrations.down_sql`, so an older binary can replay it without knowing the newer
  schema. Migrations that are irreversible or expressed as Rust functions instead record
  `down_unsupported_reason`; `db downgrade` past such a step is refused cleanly, naming the
  migration and the reason, without touching the store. Freshly created stores are seeded with a
  `schema_migrations` row (including down-SQL) for every chain migration, so a brand-new store on
  the latest binary is downgradable too.

  **Three weight classes**, by authoring cost and what's allowed to run inside `db migrate`:
  1. **Fast schema DDL** — ordinary transactional runner steps.
  2. **In-DB rebuilds** (FTS5 rebuild, DiskANN index drop + recreate) — single-statement runner
     steps that may take minutes; acceptable because `db migrate` is explicit and reports
     per-step progress.
  3. **Re-embedding / re-extraction** — not runnable by the store itself, since it needs the
     embedder/extractors that live above `store-libsql`. The migration only *marks* the work
     (bumps the required `policy_version`/`extractor_version`, truncates derived rows); the
     existing staleness machinery and incremental `localdb index` do the actual work, resumably
     and with progress. `db migrate` ends with a `localdb index` hint whenever it applied a
     migration of this class.

  **Write-twice rule:** `create_schema()` always represents *head* DDL directly (not by replaying
  the chain) — every migration is written twice, once as a chain entry and once folded into
  `create_schema()`. A CI drift-guard test asserts baseline schema + chain output is identical to
  `create_schema()`'s output, so the two can't silently diverge.
- **Extractor Versioning:** `resources.extractor_version` tracks which parser/ingestor version
  produced the blocks, enabling selective reprocessing when extraction logic improves.

### Schema v5 (2026-07)

Schema version 5 — the first entry in the migration chain above (§9's `schema_migrations` table),
`drop_chunks_block_id_and_retag_resource_metadata` — ships this refactor's storage changes:

- `chunks.block_id` is dropped (§2, #128); the parent block is looked up by `block_seq`, not a
  row reference.
- New composite index `idx_chunks_store_resource_pos (store_id, resource_id, block_seq,
  seq_in_block)` replaces the old `block_id`-keyed lookup.
- `resources.metadata_json` carries the tagged `Metadata` enum encoding (§7), not the retired flat
  `DocumentMetadata`.
- `chunks.location_json` gains the optional `window_block_seqs` array (§2, #129).

**A v4 store refuses to open until migrated:** as with every schema change under the migration
framework, opening a store still at v4 fails with `invalid_config` (exit 2) pointing at
`localdb db migrate` — nothing is wiped implicitly. Running `localdb db migrate` applies this
migration (drops `chunks.block_id`, swaps the index, retags `resources.metadata_json`) in one
transaction and lands the store at v5.

**This migration is not downgradable:** `chunks.block_id` cannot be reconstructed from what
remains once dropped, so its `Down` is `Unsupported` — `localdb db downgrade` refuses cleanly past
this step, naming the reason. It also sets `needs_reindex: true`: applying it marks existing
chunks stale (see the chunk-ID paragraph below), and `db migrate` prints a `localdb index` hint
after applying it.

**Old chunk IDs are tolerated, not migrated:** chunk IDs computed under the pre-#128 formula
(keyed off `block_id`) are not translated to the new `blake3(resource_id ‖ block_seq ‖ chunk_text
‖ seq_in_block)` formula. Instead, the chunking policy identifier bumps
(`textsplitter-md-v3` → `textsplitter-md-v4`), which changes every chunk's `policy_version`. The
existing incremental-skip check already treats a `policy_version` mismatch as "needs reindex," so
the next `localdb index` re-chunks and re-derives every chunk ID under the new formula without any
special-cased migration logic.
