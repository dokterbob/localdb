# MCP Server

localdb ships an MCP server that exposes your indexed stores to any MCP-capable AI
agent (Claude Desktop, Claude Code, custom agents). It runs on stdio, speaks the
[MCP 2024-11-05 protocol](https://modelcontextprotocol.io/), and opens stores in
embedded mode — no daemon required.

For design rationale and the trust model see [../specs/05-surfaces.md](../specs/05-surfaces.md) §4.

---

## Setup

### Claude Desktop / any JSON-configured host

Add a block to your host's `.mcp.json` (or `claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "localdb": {
      "command": "localdb",
      "args": ["mcp"]
    }
  }
}
```

To use a custom config file:

```json
{
  "mcpServers": {
    "localdb": {
      "command": "localdb",
      "args": ["mcp", "--config", "/path/to/config.yaml"]
    }
  }
}
```

### Claude Code (CLI)

```
claude mcp add localdb -- localdb mcp
```

With a custom config:

```
claude mcp add localdb -- localdb mcp --config /path/to/config.yaml
```

---

## Tools

The server exposes three read-only tools. Write tools are reserved for a future
`--allow-write` release; `--allow-write` is accepted by the CLI today for
forward-compatibility but all mutating operations are rejected in v1.

### `search`

Hybrid search (BM25 + dense vector) across indexed stores. Returns a ranked list
of citations in the canonical localdb Citation JSON shape.

> **Note:** in v0.1.0 the dense component uses a placeholder embedder (scores
> appear as `dense: 1.0`). Ranking is effectively BM25-driven. See
> [../specs/04-search-pipeline.md](../specs/04-search-pipeline.md) for the
> intended pipeline and [../specs/06-roadmap.md](../specs/06-roadmap.md) for the
> real-embedder milestone.

**Input schema:**

```json
{
  "type": "object",
  "required": ["query"],
  "properties": {
    "query": {
      "type": "string",
      "description": "Natural language search query"
    },
    "stores": {
      "type": "array",
      "items": { "type": "string" },
      "description": "Optional list of store names to search. Defaults to all stores."
    },
    "limit": {
      "type": "integer",
      "minimum": 1,
      "maximum": 100,
      "description": "Maximum number of results to return (default: 10, max: 100)"
    }
  }
}
```

**Example call:**

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "tools/call",
  "params": {
    "name": "search",
    "arguments": { "query": "reciprocal rank fusion", "limit": 1 }
  }
}
```

**Example result** (the `text` field carries pretty-printed JSON):

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "isError": false,
    "content": [
      {
        "type": "text",
        "text": "{\n  \"citations\": [\n    {\n      \"chunk_id\": \"eff4065c...\",\n      \"document_id\": \"a9bb80b7...\",\n      \"heading_path\": [],\n      \"provenance\": {\n        \"content_hash\": \"929258b8...\",\n        \"fetched_at\": \"2026-06-11T14:17:30Z\"\n      },\n      \"score\": {\n        \"bm25\": 3.0748,\n        \"dense\": 1.0,\n        \"fused\": 0.032786\n      },\n      \"snippet\": \"Meeting 2026-06-02: decided to adopt reciprocal rank fusion...\",\n      \"span\": { \"start\": 0, \"end\": 138 },\n      \"store\": { \"id\": \"01KTVGQ62...\", \"name\": \"notes\" },\n      \"title\": null,\n      \"uri\": \"file:///home/user/notes/meeting.txt\"\n    }\n  ],\n  \"total_candidates\": 3\n}"
      }
    ]
  }
}
```

The citation shape is identical to `localdb search --json`. See
[../specs/02-domain-model.md](../specs/02-domain-model.md) §6 for field definitions.

---

### `get_document`

Fetch the normalized text and metadata for a document by its ID.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "id": {
      "type": "string",
      "description": "Document ID (content-addressed blake3 hash)"
    },
    "uri": {
      "type": "string",
      "description": "Document URI (e.g. file:///path/to/doc or URL)"
    }
  }
}
```

> **v1 limitation:** `uri`-based lookup is not supported. Pass the `document_id`
> from a `search` citation. Sending a `uri` returns `isError: true` with the
> message: `"uri-based get_document is not supported in v1; use the document 'id'
> from a search result"`.

**Example call:**

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "tools/call",
  "params": {
    "name": "get_document",
    "arguments": { "id": "a9bb80b7ae3ab7fa65b2181542690785d79e04c4497b59d401583e2358e77ca4" }
  }
}
```

**Example result:**

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "isError": false,
    "content": [
      {
        "type": "text",
        "text": "{\n  \"chunk_count\": 1,\n  \"document_id\": \"a9bb80b7...\",\n  \"provenance\": { \"content_hash\": \"929258b8...\", \"fetched_at\": \"2026-06-11T14:17:30Z\" },\n  \"store\": { \"id\": \"01KTVGQ62...\", \"name\": \"notes\" },\n  \"text\": \"Meeting 2026-06-02: decided to adopt reciprocal rank fusion...\",\n  \"title\": null,\n  \"uri\": \"file:///home/user/notes/meeting.txt\"\n}"
      }
    ]
  }
}
```

---

### `list_stores`

List all available stores with their names, visibility, and document/chunk counts.

**Input schema:** `{}` (no arguments)

**Example call:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": { "name": "list_stores", "arguments": {} }
}
```

**Example result:**

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "isError": false,
    "content": [
      {
        "type": "text",
        "text": "{\n  \"stores\": [\n    {\n      \"chunk_count\": 3,\n      \"document_count\": 3,\n      \"id\": \"01KTVGQ62TQN8X6XN9E5FDZN67\",\n      \"name\": \"notes\",\n      \"visibility\": \"private\"\n    }\n  ]\n}"
      }
    ]
  }
}
```

---

## Embedded mode

When no daemon is running, `localdb mcp` opens the store databases in-process
(embedded mode). This is the normal operating mode and requires no prior setup
beyond having run `localdb index`.

If you also have `localdb serve` running, **stop it before using the MCP server**.
While the daemon holds the runtime-state DB lock, any embedded-mode process
(including `localdb mcp`) will fail to open the same database. See §
Troubleshooting below.

---

## Troubleshooting

### MCP server cannot open the database

```
error: internal error: cannot open runtime-state DB: Database already open. Cannot acquire lock.
```

The HTTP daemon (`localdb serve`) is running and holds the database lock. Stop it
before running `localdb mcp` in embedded mode. The two processes cannot share the
same data directory simultaneously in v0.1.0.

### `localdb search` / MCP search returns no results after `serve` was running

The daemon uses an in-memory store that does not see CLI-indexed LanceDB data
(experimental limitation — see [../specs/06-roadmap.md](../specs/06-roadmap.md)).
Stop the daemon, run `localdb index`, then re-run the MCP server.

### `daemon is unreachable` (exit 5) / stale socket

If the daemon was killed with `SIGKILL` (or crashed), it may leave a stale
`daemon.sock` file in the data directory. Remove it:

```
rm <data_dir>/daemon.sock
```

After removing the socket, `localdb status` should report `daemon: not running
(embedded mode)` and the MCP server will start normally.
