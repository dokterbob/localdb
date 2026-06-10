# Spec 02 — Canonical Domain Model

> Status: accepted draft, 2026-06-10. All entities live in the `core` crate and are shared by
> every surface. Field lists are normative for meaning, not for exact Rust types.

## 1. Entity overview

```
Store 1──* Source 1──* Document 1──* Block 1──* Chunk
                            │                      │
                       IndexJob               Citation (view over Chunk + Document)
```

## 2. Entities

### Store
A named knowledge base. Unit of sharing, ACLs, indexing policy, and federation.

| Field | Notes |
|---|---|
| `id` | Stable ULID, minted at creation; never reused. |
| `name` | Human-readable, unique per instance. |
| `visibility` | `private` \| `shared`. MVP: only `private` functional; field exists from day one ([01-architecture.md](01-architecture.md) §5). |
| `backend` | Backend kind + connection info; default `lancedb`. |
| `indexing` | Indexing policy: `{chunking, embedding}` as one unit ([03-config.md](03-config.md) §2). |
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

### Block
An intermediate structural unit from extraction (heading section, paragraph group, code block,
list). Blocks preserve document structure so chunkers can respect it; they are **not stored in the
retrieval backend**, only chunks are. Fields: `document_id`, `ordinal`, `kind`, `text`,
`span` (byte/char range in the normalized text), `heading_path` (e.g. `["API", "Auth"]`).

### Chunk
The retrieval unit: what gets embedded and indexed.

| Field | Notes |
|---|---|
| `id` | **Content-addressed**: `blake3(document_id ‖ chunk_text ‖ span)` — stable across re-runs over identical content. |
| `document_id`, `store_id` | Ownership. |
| `text` | Chunk text (also feeds BM25). |
| `span` | Range in the normalized document text — the citation anchor. |
| `heading_path` | Inherited from blocks; shown in citations. |
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
  one **message** = one Block (later chunked by thread/turn windows, see preset `messages` in
  [03-config.md](03-config.md) §2).
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
  provenance: {fetched_at, content_hash}
}
```

Surface mappings — defined here once, referenced by [05-surfaces.md](05-surfaces.md):
**HTTP** returns the structure verbatim as JSON. **CLI** renders `uri` + heading path + snippet
(and full JSON with `--json`). **MCP** returns it as structured tool output content, never as
prose-only text, so agents can cite mechanically.

The `span` refers to the **normalized extracted text**, not raw bytes of the original file;
original-file line mapping is a roadmap item ([06-roadmap.md](06-roadmap.md) §6).
