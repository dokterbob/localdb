---
name: localdb
description: Search and index local document collections with the localdb CLI or MCP server — hybrid search with citations over markdown, text, and PDF files.
---

## When to use

Use localdb when you need to retrieve passages from a local corpus (Markdown, plain text,
PDF, or indexed URLs) with verifiable citations. It returns structured `Citation` objects
with the source URI, exact text snippet, byte span, relevance scores (BM25, dense, fused),
and document metadata extracted from frontmatter. Hybrid search (BM25 + binary-quantized
dense) runs entirely in-process — no daemon or GPU needed.

---

## CLI crib sheet

```bash
# 1. Initialize config and data directory (first time only)
localdb init

# 2. Create a runtime store
localdb store add notes

# 3. Register a directory as a source on that store
localdb source add ~/notes --store notes

# 3a. Or index a URL source
localdb source add https://example.com/doc --store notes

# 4. Index all sources in the store
localdb index --store notes

# 5. Search and get JSON citations
localdb search "reciprocal rank fusion" --store notes --json

# 6. Extract URI + snippet from each citation with jq
localdb search "your query" --store notes --json \
  | jq -r '.citations[] | "\(.uri)\n  \(.snippet)"'
```

---

## Reading citations

`localdb search --json` returns an object with a `citations` array. Each citation:

| Field | Type | Meaning |
|---|---|---|
| `uri` | string | `file://` URI of the source document |
| `snippet` | string | Extracted text passage matching the query |
| `span` | `{start, end}` | Byte offsets of the snippet within the document |
| `score.fused` | float | Reciprocal-rank-fusion score (higher = more relevant) |
| `score.bm25` | float | BM25 component |
| `score.dense` | float | Dense vector component — normalized Hamming similarity (`1 − dist/bits`) from the binary-quantized local ONNX embedder |
| `document_id` | string | Blake3 content hash — pass to `get_document` MCP tool |
| `heading_path` | array | Markdown heading breadcrumbs (may be empty) |
| `metadata` | object | Dublin Core document metadata extracted from frontmatter: `title`, `creator`, `date`, `description`, etc. Fields are `null` when not present. |

---

## MCP tool shapes

When localdb is registered as an MCP server (`localdb mcp`), three tools are
available:

```
search(query: string, stores?: string[], limit?: int)
  → citations array (same shape as CLI --json)

get_document(id: string)
  → { document_id, uri, text, title, chunk_count, provenance, store, metadata }
  Note: uri-based lookup is NOT supported in v1; use document_id from a search result.

list_stores()
  → { stores: [{ id, name, visibility, document_count, chunk_count }] }
```

Tool results are returned as a `text` content item whose `text` field contains
pretty-printed JSON.

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

| Symptom | Cause | Fix |
|---|---|---|
| `error: store not found: handbook` on `localdb index` | Store was declared in YAML config — YAML-declared stores cannot be indexed | Use `localdb store add handbook` to create a runtime store, then `localdb source add` |
| `Database already open. Cannot acquire lock.` | `localdb serve` (HTTP daemon) is running and holds the DB lock | Stop the daemon; CLI and MCP work in embedded mode without it |
| `error: daemon is unreachable` (exit 5) | Stale `daemon.sock` left after daemon crash or `SIGKILL` | `rm <data_dir>/daemon.sock` |
| Empty search results | Store has not been indexed yet | Run `localdb index --store <name>` |
| `error: invalid request: store 'X' already exists` (exit 2) | `store add` called for a store that already exists | Use the existing store; list stores with `localdb store list` |
| `source add` on a non-existent path succeeds | Path existence is not validated at add time | The error will surface at index time |
