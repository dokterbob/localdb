---
name: localdb
description:
  Search and index local document collections with the localdb CLI or MCP server — hybrid (keyword +
  semantic) search with byte-exact citations over Markdown, HTML, PDF, Office, EPUB, and plain-text
  files, URLs, and feeds.
license: AGPL-3.0-or-later
---

## When to use

Use localdb when you need to retrieve passages from a local corpus with verifiable citations. It
indexes Markdown, plain text, HTML, text-layer PDF, Office documents (DOCX/PPTX/XLSX/XLS/CSV), EPUB,
and URL/feed sources, and returns structured `Citation` objects with the source URI, exact snippet,
byte span, per-component relevance scores (BM25, dense, RRF-fused), provenance hashes, and Dublin
Core document metadata. Hybrid search runs entirely in-process — no daemon or GPU needed. The first
indexing or search operation downloads the default local embedding model (~706 MB, one time, no API
key).

---

## CLI crib sheet

```bash
# 0. Shortcut: create the default store, add a folder, and index it in one step
localdb add ~/notes

# 1. Or explicitly: create a store
localdb store add notes

# 2. Register a directory as a source on that store
localdb source add ~/notes --store notes

# 2a. Or a URL / feed source
localdb source add https://example.com/doc --store notes

# 3. Index all sources in the store (incremental; re-run any time)
localdb index --store notes

# 4. Search and get JSON citations (flags go BEFORE the query text)
localdb search --store notes --json "reciprocal rank fusion"

# 5. Extract URI + snippet from each citation with jq
localdb search --store notes --json "your query" \
  | jq -r '.citations[] | "\(.uri)\n  \(.snippet)"'
```

---

## Reading citations

`localdb search --json` returns an object with a `citations` array. Key fields per citation:

| Field                     | Type           | Meaning                                                                                                                     |
| ------------------------- | -------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `uri`                     | string         | `file://` (or `https://`) URI of the source document                                                                        |
| `snippet`                 | string         | Extracted text passage matching the query                                                                                   |
| `location.span`           | `{start, end}` | Byte offsets of the snippet within the document                                                                             |
| `score.fused`             | float          | Reciprocal-rank-fusion score (higher = more relevant)                                                                       |
| `score.bm25`              | float          | BM25 component                                                                                                              |
| `score.dense`             | float          | Dense component — normalized Hamming similarity with the default binary-quantized local model; cosine for float32 embedders |
| `resource_id`             | string         | Blake3 content-addressed document ID — pass as `get_document`'s `id` / `get_chunks`' `resource_id`                          |
| `chunk_id`                | string         | Content-addressed ID of this specific chunk                                                                                 |
| `provenance.content_hash` | string         | Blake3 hash of the source content, plus `provenance.fetched_at`                                                             |
| `heading_path`            | array          | Heading breadcrumbs above the snippet (may be empty)                                                                        |
| `store`                   | object         | `{ id, name }` of the store the hit came from                                                                               |
| `metadata`                | object         | Dublin Core document metadata (`title`, `creator`, `date`, `format`, …); fields are `null`/empty when not present           |

There is no top-level `document_id` or `span` field — use `resource_id` and `location.span`.

---

## MCP tool shapes

When localdb is registered as an MCP server (`localdb mcp` over stdio, or HTTP at `/mcp` on a
running `localdb serve` daemon), five read-only tools are available:

```
search(query: string, stores?: string[], limit?: int, content_length?: int, ...filters)
  → citations array (same shape as CLI --json). Filters include mime, path (URI
    prefix), and added/modified/document date bounds (RFC 3339, partial dates,
    or relative durations like "7d").

get_document(id: string, store?: string)
  → { resource_id, uri, text, title, chunk_count, provenance, store, metadata }
  Lookup is by content-addressed ID (a search result's resource_id); URI-based
  lookup is not supported. `store` (id or name) disambiguates an ID present in
  several stores.

get_chunks(resource_id: string, store?: string, offset?: int, limit?: int,
           anchor_chunk_id?: string, anchor_block_seq?: int)
  → { resource_id, title, store, total_chunks, offset, limit, returned, chunks }
  Paginated in storage order; anchor_* center the window on a position instead
  of offset. An out-of-range offset returns an empty chunks array, not an error.

list_stores()
  → { stores: [{ id, name, visibility, document_count, chunk_count }] }

list_documents(store: string, source?: string, offset?: int, limit?: int)
  → paginated listing of everything indexed in one store (store is REQUIRED here,
    unlike the optional store/stores everywhere else).
```

Tool results are returned as a `text` content item whose `text` field contains pretty-printed JSON.
All tools are read-only in v1 — `--allow-write` is accepted but registers no writing tools yet. If
`localdb serve` is already running, `localdb mcp` proxies to its `/mcp` route automatically rather
than conflicting with it.

---

## Config snippet

Minimal config with custom data directory (`version: 1` is required):

```yaml
version: 1
paths:
  data: /path/to/your/localdb-data
```

Pass it to any command with `--config /path/to/config.yaml`.

---

## Troubleshooting

| Symptom                                                     | Cause                                                             | Fix                                                                        |
| ----------------------------------------------------------- | ----------------------------------------------------------------- | -------------------------------------------------------------------------- |
| `error: store not found: handbook` (exit 3)                 | The store has not been created (stores are not declared in YAML)  | `localdb store add handbook`, then `localdb source add … --store handbook` |
| `database schema version N is behind this build` (exit 2)   | The database predates this binary's schema; `open` never migrates | Run `localdb db migrate` (check first with `localdb db status`)            |
| `error: daemon is unreachable` (exit 5)                     | Stale `daemon.sock` left after a daemon crash or `SIGKILL`        | `rm <data_dir>/daemon.sock`                                                |
| Empty search results                                        | Store has not been indexed yet                                    | Run `localdb index --store <name>`                                         |
| `error: invalid request: store 'X' already exists` (exit 2) | `store add` called for a store that already exists                | Use the existing store; list stores with `localdb store list`              |
| `source add /does/not/exist` fails (exit 2)                 | Path existence is validated at add time                           | Fix the path; only existing files/directories can be registered            |
| First search/index takes minutes                            | One-time ~706 MB embedding-model download on first use            | Wait for the download to finish; later runs reuse the cached model         |
