# Spec 02 — Canonical Domain Model

> Status: accepted draft, 2026-06-10. All entities live in the `core` crate and are shared by
> every surface. Field lists are normative for meaning, not for exact Rust types.

## 1. Entity overview

```
Store 1──* Source 1──* Document 1──* Chunk
                            │            │
                       IndexJob     Citation (view over Chunk + Document)
```

Extraction produces a single normalized **Markdown string** per document, not a nested
structural tree. The chunker (`MarkdownSplitter`) consumes that string directly, and per-chunk
`heading_path` is *derived* from the Markdown heading structure (`core/src/heading_index.rs`),
not from a stored intermediate. The older `Block`/`BlockKind` representation was removed in the
Markdown-native migration (commit `3da56d0`).

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
Where a store's content comes from.

| Field | Notes |
|---|---|
| `id` | ULID. |
| `store_id` | Owning store. |
| `kind` | MVP: `path` \| `url`. Roadmap (reserved identifiers, not implemented): `imap`, `mbox`, messenger connectors. |
| `spec` | Kind-specific: root path + include/exclude globs, or URL + refresh interval. |
| `source_kind_preset` | Which indexing preset applies (`prose`, `messages`, `code`) — see [03-config.md](03-config.md) §2. |

**Runtime representation:** `RuntimeSource` in `core::config::runtime_state` is the concrete
Rust type for sources persisted in the runtime-state DB (`runtime-state.db`). It is a core
domain type — not a CLI type — and includes fields `id`, `store_name`, `kind`, `root`,
`url`, `include`, `exclude`, and `preset`. Source CRUD is exposed via `RuntimeStateDb`
methods (`upsert_source`, `delete_source`, `list_sources`, `get_source`,
`find_source_by_root_or_url`).

### Document
One logical content unit produced from a source: a file, a fetched page, later one message/thread.

| Field | Notes |
|---|---|
| `id` | **Content-addressed**: `blake3(canonical_source_uri ‖ content_hash)` — see §3. |
| `source_id`, `store_id` | Ownership. |
| `uri` | Canonical locator (absolute path as `file://`, or URL). |
| `title`, `mime`, `lang` | From extraction. |
| `content_hash` | blake3 of extracted normalized text. Drives incremental re-index. |
| `provenance` | See §4. |
| `meta` | Open key-value extension point (string → JSON). Message fields live here later (§5). |

### Normalized Markdown (intermediate representation)
Extraction normalizes every format to a single **Markdown string** (`ParsedDocument.markdown`).
This string is the document's intermediate representation: the chunker indexes spans directly
into it, and `heading_path` (e.g. `["API", "Auth"]`) is derived on demand from the Markdown
heading structure via `core/src/heading_index.rs`. There is no stored block tree — the former
`Block`/`BlockKind` type was deleted in the Markdown-native migration (commit `3da56d0`).

### Chunk
The retrieval unit: what gets embedded and indexed.

| Field | Notes |
|---|---|
| `id` | **Content-addressed**: `blake3(document_id ‖ chunk_text ‖ span)` — stable across re-runs over identical content. |
| `document_id`, `store_id` | Ownership. |
| `text` | Chunk text (also feeds BM25). |
| `span` | Range in the normalized document text — the citation anchor. |
| `heading_path` | Derived from the Markdown heading structure (`core/src/heading_index.rs`); shown in citations. |
| `embedding` | Dense vector (in backend, not in core serialization). |
| `policy_version` | Hash of the indexing policy that produced it ([04-search-pipeline.md](04-search-pipeline.md) §4). |
| `provenance` | Copied from document (§4) — chunks must be self-describing for federation. |

### Citation
Not a stored entity: the **canonical result shape** every surface uses (§6).

### IndexJob
A unit of indexing work with observable state. Fields: `id` (ULID), `store_id`, `scope` (full
store / one source / one document), `state` (`pending` → `running` → `done` | `failed`),
`stats` (docs seen/indexed/deleted, chunks written), `error`, timestamps. Embedded mode runs jobs
synchronously but still records them; the daemon queues them ([05-surfaces.md](05-surfaces.md) §3).

## 3. ID scheme

**Decision:** entities that exist by fiat (Store, Source, IndexJob) get **ULIDs**; entities
derived from content (Document, Chunk) get **content-addressed blake3 IDs** as defined above.

**Rationale:** content-addressed IDs are the federation prerequisite — two nodes indexing the same
content derive the same chunk identity, enabling dedup, provenance comparison, and integrity
checks without coordination ([VISION.md](../VISION.md)). They also make re-indexing idempotent.
**Rejected:** auto-increment rows (meaningless off-node); UUIDv4 for documents/chunks (stable
only by table lookup, not by content).

Consequence: a document edit produces a *new* document ID; the pipeline treats it as
replace-by-URI (delete chunks of the old ID, insert new) — see [04-search-pipeline.md](04-search-pipeline.md) §2.

## 4. Provenance

Every document and every chunk carries:

| Field | Notes |
|---|---|
| `origin_store` | Store ID where it was first indexed (≠ current store after future federation). |
| `source_ref` | Source ID + kind. |
| `fetched_at` | Acquisition time (file mtime at scan / HTTP fetch time). |
| `content_hash` | blake3 of normalized content. |
| `share_path` | Reserved, empty in MVP: list of (node, store) hops for federated content. |

## 5. Message-shaped documents (extension point only)

MVP defines **no** message connectors, but the mapping is fixed now so `meta` doesn't ossify:

- One **thread** = one Document (URI = e.g. `imap://acct/folder;uid=...` or connector-defined);
  one **message** = one Markdown section (later chunked by thread/turn windows, see preset
  `messages` in [03-config.md](03-config.md) §2).
- Reserved `meta` keys (namespaced, validated when present): `msg.thread_id`,
  `msg.participants` (list), `msg.sent_at`, `msg.in_reply_to`, `msg.channel`.
- Thread context is exactly what contextualized embeddings consume
  ([04-search-pipeline.md](04-search-pipeline.md) §4) — the document-aware embedder interface is
  sized for this from day one.

## 6. Citation model

Every search hit, on every surface, resolves to the same citation structure:

```
Citation {
  chunk_id, document_id, store: {id, name},
  uri,                  // file path or URL — the user-actionable locator
  title, heading_path,
  span: {start, end},   // range in normalized text
  snippet,              // chunk text (possibly trimmed)
  score: {fused, dense, bm25},
  provenance: {fetched_at, content_hash},
  metadata              // full DCMES DocumentMetadata per §7; always present, empty when none extracted
}
```

Surface mappings — defined here once, referenced by [05-surfaces.md](05-surfaces.md):
**HTTP** returns the structure verbatim as JSON. **CLI** renders `uri` + heading path + snippet
(and full JSON with `--json`). **MCP** returns it as structured tool output content, never as
prose-only text, so agents can cite mechanically.

The `span` refers to the **normalized extracted text**, not raw bytes of the original file;
original-file line mapping is a roadmap item ([06-roadmap.md](06-roadmap.md) §5).

## 7. Extraction & parsing

### Parser chain (chain of responsibility)

The `Parser` trait (`core/src/parser.rs`) is the abstraction for format-specific text
extraction. Each `Parser` is `Send + Sync` and runs synchronously (CPU-bound); callers run it
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
  Two sub-cases, distinguished by the error variant:
  - `Error::ExtractionFailed` — the format is *supported* but this specific instance is broken
    (e.g. a corrupt or truncated DOCX/PDF). Counted in `error_count`; produces a WARN per file.
  - `Error::UnsupportedFormat` — the format is *not handled* by any parser in scope (e.g. a
    scanned PDF with no text layer). Counted in `unsupported_format_count`; silent.

`ChainParser` implements this same `Parser` trait (Composite pattern), holding an ordered
`Vec<Box<dyn Parser>>`. It is itself a `Parser` and can be nested. `build_chain(ids)` in
`extract/src/registry.rs` maps the config `parsers:` strings to concrete `Parser` instances.
Parser order and the valid IDs (`pdf`, `epub`, `office`, `html`, `markdown`, `plaintext`) are
configured in [03-config.md](03-config.md) §2.

### Probe

`Probe` is the fully-buffered input presented to each parser. The streaming or HTTPS read
happens once at the ingestion boundary; parsers never seek or re-fetch.

| Field / method | Notes |
|---|---|
| `bytes` | Full document bytes. |
| `path_hint: Option<&str>` | Original filename or URL path — used for file-extension hints. Advisory; may be absent. |
| `sniffed_mime: Option<&str>` | MIME type inferred before parsing. Advisory; may be wrong or `None`. Real format decisions happen inside `parse`, not here. |
| `header()` | Up to `PROBE_HEADER_LEN` (8 192) leading bytes for cheap magic-byte sniffing without reading the full document. |

### ParsedDocument

The successful output of a `parse` call.

| Field | Notes |
|---|---|
| `markdown` | Normalized document text as a Markdown string (§2). All chunk spans index into this string; it is the sole content IR — there is no separate block list. |
| `title` | Title from extraction (typed fast-path; also available via `metadata.title`). |
| `metadata` | `DocumentMetadata` — Dublin Core elements (see below). |

### DocumentMetadata

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
| `type` | `Option<String>` | Nature or genre of the resource. |
| `format` | `Option<String>` | File format or media type. |
| `identifier` | `Option<String>` | Unambiguous reference (URL, DOI, ISBN, …). |
| `source` | `Option<String>` | Source resource this document is derived from. |
| `language` | `Option<String>` | Language of the resource (BCP 47 recommended). |
| `relation` | `Vec<String>` | Repeatable: related resources. |
| `coverage` | `Option<String>` | Spatial or temporal extent. |
| `rights` | `Option<String>` | Rights statement or license. |

**Persistence:** `DocumentMetadata` is JSON-encoded into a single nullable `TEXT` column named
`metadata` on each chunk record in libsql. Threading path:
`Parser` → `ParsedDocument.metadata` → `ExtractionResult.metadata` → `ChunkRecord.metadata` →
libsql `metadata` column.

**Defensive read:** tables created before this column was added (pre-migration) may have a
missing column, a `NULL` value, or an unparseable payload. All three cases resolve to
`DocumentMetadata::default()` (all fields empty/`None`).

**Decision/Rationale:** a single structured column for all 15 DC elements keeps the schema
stable as parsers populate more fields over time; JSON encoding avoids a 15-column explosion
while remaining human-readable in the store. **Rejected:** one nullable column per DC element —
schema churn every time a new element is populated; flat string bag — loses type information
and makes the repeatable/singleton distinction invisible.

**Cross-reference:** `Document.meta` (§2, `meta` row) accepts open key-value pairs; 15 `dc.*`
keys are validated when present (`validate_dc_meta_key` in `core/src/types.rs`). These mirror
the 15 DCMES elements in `DocumentMetadata` and are the untyped extension point for surfaces
that need to set DC fields without going through a full `Parser`. The live ingestion path
populates `DocumentMetadata` (typed) rather than `meta` (untyped).
