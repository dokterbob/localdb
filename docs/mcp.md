# MCP Server

localdb ships an MCP server that exposes your indexed stores to any MCP-capable AI
agent (Claude Desktop, Claude Code, custom agents). It's built on the official
[`rmcp`](https://docs.rs/rmcp) SDK and speaks the
[MCP 2025-06-18 protocol](https://modelcontextprotocol.io/). Two transports are
available, both serving the same four read-only tools:

- **Stdio** (`localdb mcp`) — the default, no daemon required.
- **HTTP** (`/mcp`, mounted on a running `localdb serve` daemon) — for connecting a
  remote MCP client, or one running on a different machine on your network/Tailscale.

For design rationale and the trust model see [../specs/05-surfaces.md](../specs/05-surfaces.md) §4.

---

## Setup

### Claude Desktop / any JSON-configured host (stdio)

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

### Claude Code CLI (stdio)

```
claude mcp add localdb -- localdb mcp
```

With a custom config:

```
claude mcp add localdb -- localdb mcp --config /path/to/config.yaml
```

### Remote / HTTP — connecting from another machine

If you run `localdb serve`, it mounts the same four MCP tools at `/mcp` alongside its
`/v1` REST API. This is how to point an MCP client at localdb running on a different
machine — e.g. a home server reachable over Tailscale, or a NAS on your LAN.

1. Start the daemon bound to an address reachable from the client machine (not just
   `127.0.0.1`). Binding to a specific non-loopback address — a Tailscale IP, a LAN
   IP — is a deliberate, supported trust decision; see
   [docs/http-api.md](http-api.md#trust-model) for the full trust model and how to
   configure it in `config.yaml`.

   ```yaml
   server:
     bind: 100.x.y.z   # your Tailscale/LAN address
     port: 7700
   ```

2. On the client machine, register the daemon's `/mcp` endpoint as an HTTP MCP
   server. For Claude Code:

   ```
   claude mcp add --transport http localdb http://100.x.y.z:7700/mcp
   ```

localdb automatically allow-lists whatever address the daemon actually bound to for
`rmcp`'s DNS-rebinding `Host`-header check — you don't need to configure this
separately. (Internally: a deliberately-chosen non-loopback bind is added to the
allowlist alongside `rmcp`'s own `localhost`/`127.0.0.1`/`::1` defaults; a wildcard
bind, `0.0.0.0`/`::`, disables the check entirely, since it already accepts
connections from any network. See [specs/05-surfaces.md](../specs/05-surfaces.md) §4.2.)

Store resolution over `/mcp` is realtime: a store added later via `POST /v1/stores`
appears on the very next MCP call (`search`, `get_document`, `get_chunks`,
`list_stores`) — no daemon restart needed.

---

## Daemon-proxied stdio

If a daemon is already running when you start `localdb mcp`, it detects this the same
way every other localdb command does and **proxies** every request to the daemon's
own `/mcp` route instead of opening the store a second time. This means:

- You no longer need to stop `localdb serve` before using `localdb mcp` — the two now
  coexist by design (this replaces earlier v1 guidance that told you to stop the
  daemon first).
- Proxied mode always exposes the daemon's current (realtime) full store set —
  **`--store` narrowing is not honored** when a daemon is running, since the daemon's
  own `/mcp` route has no notion of a per-stdio-session store filter to apply.
  `localdb mcp --store <name>` against a running daemon prints a non-fatal warning to
  stderr and serves the daemon's full store set regardless. This is a documented v1
  limitation, not a bug — see [specs/05-surfaces.md](../specs/05-surfaces.md) §4.2.

If no daemon is running, `localdb mcp` opens the store(s) embedded in-process exactly
as before — no behavior change for the common case.

---

## Tools

The server exposes four read-only tools. Write tools are reserved for a future
`--allow-write` release; `--allow-write` is accepted by the CLI today for
forward-compatibility but all mutating operations are rejected in v1.

### `search`

Hybrid search (BM25 + dense vector) across indexed stores. Returns a ranked list
of citations in the canonical localdb Citation JSON shape.

> **Note:** the dense component uses the configured embedder (default: `pplx-embed-context-v1-0.6b`
> local ONNX). The model is downloaded automatically on first use (~706 MB). See
> [../specs/04-search-pipeline.md](../specs/04-search-pipeline.md) for the pipeline details.

**Input schema** (as actually returned by `tools/list`):

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
      "type": ["array", "null"],
      "items": { "type": "string" },
      "description": "Optional list of store names to search. Defaults to all stores."
    },
    "limit": {
      "type": ["integer", "null"],
      "minimum": 1,
      "maximum": 100,
      "description": "Maximum number of results to return (default: 10, max: 100)"
    },
    "content_length": {
      "type": ["integer", "null"],
      "minimum": 1,
      "description": "Soft cap on snippet text chars per result in the text rendering (default: 400)"
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
      "type": ["string", "null"],
      "description": "Document URI (e.g. file:///path/to/doc or URL)"
    }
  }
}
```

> **v1 limitation:** `uri`-based lookup is not supported. Pass the document `id`
> from a `search` citation. Sending a `uri` without `id` returns `isError: true` with the
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

### `get_chunks`

Fetch a document's chunks in storage order — `(block_seq, seq_in_block)` — paginated
by `offset`/`limit`. Use this to page through a long document after finding it via
`search` or `get_document`.

**Input schema:**

```json
{
  "type": "object",
  "required": ["document_id"],
  "properties": {
    "document_id": {
      "type": "string",
      "description": "Document ID (content-addressed blake3 hash)"
    },
    "offset": {
      "type": ["integer", "null"],
      "minimum": 0,
      "description": "Number of chunks to skip before the first returned chunk (default: 0)"
    },
    "limit": {
      "type": ["integer", "null"],
      "minimum": 1,
      "maximum": 200,
      "description": "Maximum number of chunks to return (default: 50, max: 200)"
    }
  }
}
```

> Like `get_document`, `uri`-based lookup is not supported — use a `document_id`
> obtained from a prior `search` or `get_document` call. An unknown `document_id`
> returns `document_not_found`. An `offset` past the end of the chunk list returns an
> empty `chunks` array, not an error.

**Example call:**

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "tools/call",
  "params": {
    "name": "get_chunks",
    "arguments": { "document_id": "a9bb80b7...", "offset": 0, "limit": 50 }
  }
}
```

**Example result** (`text` carries pretty-printed JSON):

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": {
    "isError": false,
    "content": [
      {
        "type": "text",
        "text": "{\n  \"document_id\": \"a9bb80b7...\",\n  \"uri\": \"file:///home/user/notes/meeting.txt\",\n  \"title\": null,\n  \"store\": { \"id\": \"01KTVGQ62...\", \"name\": \"notes\" },\n  \"total_chunks\": 1,\n  \"offset\": 0,\n  \"limit\": 50,\n  \"returned\": 1,\n  \"chunks\": [\n    {\n      \"chunk_id\": \"eff4065c...\",\n      \"block_seq\": 0,\n      \"seq_in_block\": 0,\n      \"block_kind\": null,\n      \"span\": { \"start\": 0, \"end\": 138 },\n      \"heading_path\": [],\n      \"text\": \"Meeting 2026-06-02: decided to adopt reciprocal rank fusion...\"\n    }\n  ]\n}"
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

## Error model

MCP failures split into exactly two tiers, by whether the request could be *routed*
to a tool at all. See [specs/05-surfaces.md](../specs/05-surfaces.md) §4.3 for the
full rationale — this section shows what each tier actually looks like on the wire.

**Tool-level** (`result.isError: true`) — everything you're likely to hit in
practice: a missing or malformed argument, an unknown store name, a not-found lookup,
an out-of-range `limit`/`offset`. This includes cases you might expect to be
protocol-level, like a missing required argument:

```json
{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"failed to deserialize parameters: missing field `query`"}],"isError":true}}
```

Business-logic errors (unknown store, not-found document, etc.) carry a structured
`{"error": {"code", "message"}}` JSON body as their text content instead of a plain
string, e.g. `{"error": {"code": "store_not_found", "message": "no store named 'x'"}}`.

**Protocol-level** (a JSON-RPC error, no `result` field) — only one case: calling a
tool name that doesn't exist at all.

```json
{"jsonrpc":"2.0","id":5,"error":{"code":-32602,"message":"tool not found"}}
```

In proxied stdio mode (see [Daemon-proxied stdio](#daemon-proxied-stdio) above), both
tiers pass through from the daemon's `/mcp` route unchanged. A failure of the proxy
hop itself (daemon unreachable, connection dropped mid-request) is a distinct case
with no upstream answer to relay a tier from — the CLI reports this as
`daemon_unreachable` (exit code 5).

---

## Embedded mode

When no daemon is running, `localdb mcp` opens the store databases in-process
(embedded mode). This is the normal operating mode and requires no prior setup
beyond having run `localdb index`.

If a daemon *is* running, see [Daemon-proxied stdio](#daemon-proxied-stdio) above —
`localdb mcp` proxies to it automatically rather than conflicting with it.

---

## Troubleshooting

### `daemon is unreachable` (exit 5) / stale socket

If the daemon was killed with `SIGKILL` (or crashed), it may leave a stale
`daemon.sock` file in the data directory. Remove it:

```
rm <data_dir>/daemon.sock
```

After removing the socket, `localdb status` should report `daemon: not running
(embedded mode)` and the MCP server will start normally.

### A remote HTTP MCP client reports "needs authentication"

This isn't an auth prompt — it's almost always `rmcp`'s DNS-rebinding `Host`-header
check rejecting the request with `403 Forbidden: Host header is not allowed`, which
some MCP clients surface as a generic auth failure. As of this release, localdb
automatically allow-lists the daemon's own bind address (see
[Remote / HTTP](#remote-http-connecting-from-another-machine) above), so this
should no longer happen for a supported (non-wildcard) bind — if you still hit it,
confirm the daemon's `config.yaml` `server.bind` matches the address/port you're
actually connecting to, and that you've restarted `localdb serve` after changing it.
