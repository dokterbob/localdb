# Spec 02 — Canonical Domain Model

> Status: accepted draft, revised 2026-07-07. All entities live in the `core` crate and are
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
| `acl` | Reserved; empty in MVP. **Stays reserved and unused** now that auth exists — `StoreGrant` rows, not `acl`, are the real access-control mechanism (D7, see `StoreGrant` below and [05-surfaces.md](05-surfaces.md) §3.1). |

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

**Ordering semantics** depend on `ResourceKind`:
- `document` — logical reading order
- `conversation` — chronological message order
- `transcription` — transcript time order

### §2a. BlockKind

| Kind | Text content | Metadata fields | Typical sources |
|---|---|---|---|
| `Heading` | Heading text | `level: u8` (1–6) | Documents, Notion pages |
| `Paragraph` | Prose text | — | Documents, HTML, Notion |
| `Code` | Code content | `language: Option<String>` | Markdown fences, Notion code blocks |
| `Quote` | Quoted text | — | Documents |
| `List` | List items as text | `ordered: bool` | Documents |
| `Table` | Text rendering of table | `headers: Vec<String>`, `rows: usize` | Documents, spreadsheets |
| `Message` | Message body text | `sender: String`, `timestamp: Option<String>`, `message_id: Option<String>`, `reply_to: Option<String>` | Conversations (chat, email) |
| `Segment` | Transcript segment text | `speaker: Option<String>`, `start_ms: u64`, `end_ms: u64` | Transcriptions (SRT, VTT, Whisper) |
| `Reference` | `"[label](target)"` | `target: String`, `label: Option<String>`, `ref_type: Option<String>` | Wikilinks, Notion mentions, citations |
| `Attachment` | `"filename: description"` | `filename: String`, `mime: Option<String>`, `size_bytes: Option<u64>` | Email attachments, Notion files |
| `Frontmatter` | Raw frontmatter text | `format: String` (yaml/toml/json) | Markdown, Obsidian |
| `Image` | Alt text or OCR text | `alt: Option<String>`, `src: Option<String>` | Documents with images |

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
| `id` | **Content-addressed**: `blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)` — stable across re-runs over identical content. |
| `resource_id`, `store_id` | Ownership. |
| `block_id` | Reference to the parent block (blocks.rowid). |
| `block_seq` | Denormalized block sequence number (for efficient ordering without join). |
| `seq_in_block` | Chunk position within the block (0-indexed). |
| `text` | Chunk text (also feeds BM25). |
| `heading_path` | Derived from the block tree: heading blocks preceding this content block. JSON array. |
| `embedding` | Dense vector (in backend, not in core serialization). |
| `location` | `ChunkLocation` — refined sub-block position (optional). |

**Invariant:** a chunk is a subdivision of exactly one block. Chunk location =
`{resource_id, block_id, chunk_seq_in_block}`. Chunks never cross block boundaries.

**Span semantics:** Chunk spans (`Span.start`, `Span.end`) are **block-relative byte offsets** —
they index into the parent block's `text`, not the full document Markdown. Combined with
`block_seq`, they provide a complete location: `(resource_id, block_seq, span)`. Document-relative
offsets are not stored or computed.

**Exception — message-window chunks:** the `messages` chunking preset creates chunks that span
multiple `Message`/`Segment` blocks via a sliding window. This is an explicit multi-block
chunking mode. The `ChunkLocation` carries references to all participating blocks.

### Citation
Not a stored entity: the **canonical result shape** every surface uses (§6).

### IndexJob
A unit of indexing work with observable state. Fields: `id` (ULID), `store_id`, `scope` (full
store / one source / one resource), `state` (`pending` → `running` → `done` | `failed`),
`stats` (resources seen/indexed/deleted, chunks written), `error`, timestamps. Embedded mode
runs jobs synchronously but still records them; the daemon queues them
([05-surfaces.md](05-surfaces.md) §3).

### User
An authenticated principal. Lives in `core::auth`; persisted via the `AuthStore` trait
([05-surfaces.md](05-surfaces.md) §3.1).

| Field | Notes |
|---|---|
| `id` | ULID. |
| `name` | Human-readable, unique per instance. |
| `role` | `admin` \| `member`. Admins see and manage every store; members see/search only `shared` stores they hold a `StoreGrant` for (D7). |
| `created_at` | RFC 3339. |

### AuthToken
An issued credential. Access, refresh, and API-key tokens all share one table/type
(`kind` discriminates), since they differ only in TTL, rotation behavior, and issuance flow, not
in shape (D1).

| Field | Notes |
|---|---|
| `id` | ULID. |
| `user_id` | Owning user. |
| `kind` | `access` \| `refresh` \| `api_key`. Access tokens: 1h TTL. Refresh tokens: 30d TTL, rotated on every use. API keys: no default expiry. |
| `secret_hash` | blake3 hash of the opaque, `ldb_`-prefixed, 32-byte `OsRng` secret. The secret itself is shown once at issuance and never persisted. |
| `expires_at` | Nullable — unset for API keys. |
| `last_used_at` | Updated on each successful authentication; primarily meaningful for API keys. |
| `revoked_at` | Nullable; set on explicit revocation or reuse-detected rotation (below). |
| `family_id` | Groups a refresh token with all tokens descended from it via rotation. |
| `rotated_from` | The `AuthToken.id` this row was rotated from, if any. Presenting an already-rotated (no longer current) refresh token is treated as credential theft: the entire `family_id` is revoked and authentication fails with `Unauthorized`. |

### Invite
A minted, shareable credential that lets a new user join without an existing admin creating
their account directly. `store_grants` named against a `private` store are rejected at CREATE
time (`Error::Forbidden`, reusing D7's grant rule) — a bad invite fails loudly for the admin who
typo'd a store name, not silently for whoever redeems it later. The redeem/approve state machine
(T6, `core::auth::AuthService::redeem_invite`/`approve_request`/`deny_request`/`poll_request`) is
described in full in [05-surfaces.md](05-surfaces.md) §2/§3.1.

| Field | Notes |
|---|---|
| `id` | ULID. |
| `token_hash` | blake3 hash of the invite's opaque secret; the secret is shown once, at creation. |
| `mode` | `open` (redeeming creates the user immediately) \| `closed` (redeeming creates an `AccessRequest` pending admin approval). |
| `store_grants` | JSON array of store names the resulting user is granted on redemption/approval. |
| `max_uses`, `uses` | `max_uses` defaults to 1. `uses` is incremented before the redemption's user-create/access-request-file step (see `redeem_invite`'s doc comment) — a concurrent burst can over-count `uses` past `max_uses` (cosmetic, self-limiting) but can never double-mint a user, which the `users.name` UNIQUE constraint independently prevents. |
| `expires_at`, `revoked_at` | Nullable. |
| `created_by` | Issuing user's ID. |
| `created_at` | RFC 3339. |

### AccessRequest
A pending join request created by redeeming a `closed`-mode `Invite`, awaiting admin approval
or denial (T6).

| Field | Notes |
|---|---|
| `id` | ULID. |
| `invite_id` | The `Invite` this request was made against. |
| `requested_name` | The name the requester asked for. |
| `secret_hash` | blake3 hash of the request secret minted at redemption time and shown once to the requester then. This secret is poll-only — it is never promoted to a credential (it travels as a URL query parameter on every poll, which would otherwise leak into access logs/proxies/shell history as a long-lived, live-from-approval-time API key). A *fresh* API key is minted instead, in `AuthService::poll_request`, the first time a poll observes the `Approved` state and collects it — see `collected_at` below. |
| `state` | `pending` \| `approved` \| `denied`. |
| `resulting_user_id` | Set once approved and the `User` row is created. |
| `created_at`, `decided_at` | RFC 3339; `decided_at` nullable until the request leaves `pending`. |
| `collected_at` (T6, schema v6) | Nullable RFC 3339. Set exactly once, atomically (`AuthStore::mark_access_request_collected`, mirroring `consume_auth_code`'s single-use guard), the first time a poll observes the `Approved` state and hands back the credential — every later poll observes `AlreadyCollected` instead of re-issuing it. |

### StoreGrant
A normalized row granting one user read access to one `shared`-visibility store (D7). Grants on
`private` stores are rejected — `private` stores are admin-only and ungrantable.

| Field | Notes |
|---|---|
| `store_name`, `user_id` | Composite key; one grant per (store, user) pair. |
| `granted_by` | The admin user who created the grant. |
| `created_at` | RFC 3339. |

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

```
Citation {
  chunk_id, resource_id, store: {id, name},
  uri,                  // resource URI — the user-actionable locator
  title, heading_path,
  block: {seq, kind},   // which block the chunk came from
  chunk_position: {seq_in_block},
  snippet,              // chunk text (possibly trimmed)
  score: {fused, dense, bm25},
  provenance: {fetched_at, content_hash},
  metadata,             // full Metadata (Dublin Core base + resource-kind-specific)
  location              // BlockLocation + ChunkLocation for navigation
}
```

Surface mappings — defined here once, referenced by [05-surfaces.md](05-surfaces.md):
**HTTP** returns the structure verbatim as JSON. **CLI** renders `uri` + heading path + snippet
(and full JSON with `--json`). **MCP** returns it as structured tool output content, never as
prose-only text, so agents can cite mechanically.

**Context expansion:** given a search hit, the backend supports:
1. Neighboring chunks in the same block (`chunks WHERE block_id = ? ORDER BY seq_in_block`)
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
on each resource record in libsql. The discriminant is the `Metadata` enum variant tag.

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

`ParsedDocument` remains as the parser output (Markdown string + title + Dublin Core metadata).
The file ingestor converts it to a `Resource` by:
1. Running `markdown_to_blocks()` on the Markdown string to produce typed blocks.
2. Wrapping the Dublin Core metadata into `Metadata::Document(DocumentMetadata { ... })`.
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
- **Block Reference on Chunks:** Each chunk references its parent block via `block_id`
  (blocks.rowid) and denormalized `block_seq`, enabling block-level context expansion without
  an extra join.
- **FTS5 Content Keying:** The FTS5 virtual table `chunks_fts` uses external content keying
  over `chunks.text`. Filtering by `store_id` is performed on the `chunks` join.
- **Cascade Chain:** Foreign keys with `ON DELETE CASCADE` across the chain:
  `stores → sources → resources → blocks → chunks`. Deleting a store cleans up everything.
- **Schema Versioning:** The database uses `PRAGMA user_version` to track the schema version.
  Upgrades are **stepwise and additive-only**: `store-libsql::schema::MIGRATIONS` is an ordered
  list of `Migration { from, to, run }` steps, each executed inside its own transaction that
  also bumps `PRAGMA user_version` atomically, so fresh-create and migrate-from-previous converge
  on an identical schema. A database whose version has no migration path — including any
  pre-migration-list version — is a **hard error** at startup (`Error::InvalidConfig`) instructing
  the user to recreate the database or restore from backup; the file is left completely untouched
  before that error is raised. There is deliberately no silent drop-and-recreate. `SCHEMA_VERSION`
  bumped `4 → 5` adding the auth tables (`users`, `auth_tokens`, `oauth_clients`, `auth_codes`,
  `store_grants`, `invites`, `access_requests` — see §2 above) is the first migration step shipped
  under this runner.
- **Extractor Versioning:** `resources.extractor_version` tracks which parser/ingestor version
  produced the blocks, enabling selective reprocessing when extraction logic improves.
