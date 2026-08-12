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

**Known v1 limitation:** the HTTP `/mcp` route snapshots the daemon's stores once at
startup — a store added later via `POST /v1/stores` won't appear over MCP until the
daemon restarts.

---

## Daemon-proxied stdio

If a daemon is already running when you start `localdb mcp`, it detects this the same
way every other localdb command does and **proxies** every request to the daemon's
own `/mcp` route instead of opening the store a second time. This means:

- You no longer need to stop `localdb serve` before using `localdb mcp` — the two now
  coexist by design (this replaces earlier v1 guidance that told you to stop the
  daemon first).
- Absent `--store`, proxied mode exposes whatever store set the daemon had at its own
  startup, and every request relays verbatim.
- With `--store`, the scope is enforced per request — see
  [Store scoping](#store-scoping) below.

If no daemon is running, `localdb mcp` opens the store(s) embedded in-process exactly
as before — no behavior change for the common case.

---

## Store scoping

`localdb mcp --store <name>` (repeatable) limits the session to those stores. Use it
when an agent should only see part of your index — a project-bound store, say, rather
than everything you have ever indexed.

```
$ localdb mcp --store books --store research
```

Omit `--store` and every store is exposed. An unknown name exits `3` before the server
starts serving, in both modes — it is never silently dropped. A database with **no**
stores is not an error: the server starts and exposes zero stores, since an MCP server
that exits non-zero at startup reads to its client as broken rather than as empty.

The scope is enforced in both process modes, by different mechanisms:

| Mode | How |
|---|---|
| Embedded (no daemon) | Only the scoped stores are opened. Nothing else is reachable because nothing else exists in the process. |
| Daemon-proxied | The scope is applied to every relayed `tools/call`: `search`'s `stores` and `get_document`/`get_chunks`'s `store` arguments are filled in when absent and rejected when they name a store outside the scope, and `list_stores` is filtered so out-of-scope stores cannot even be enumerated. |

Proxied mode has to work through tool arguments because there is no transport-level
channel: rmcp's HTTP service factory is a synchronous `Fn()` with no access to the
request, so neither `/mcp?store=x` nor a custom header can select a scoped handler on
the daemon side. The tool arguments already exist to name stores, so the scope travels
per request instead of per connection.

An out-of-scope store name comes back as an ordinary tool-level error:

```json
{
  "error": {
    "code": "invalid_request",
    "message": "store 'hydra' is outside this session's --store scope; allowed: [books]"
  }
}
```

While the tool set is fixed at four read-only tools, a scoped session relays only those
four; any other tool name is rejected, so a future mutating tool cannot slip through
unscoped on the day it lands.

> **This is scoping, not a security boundary.** The daemon's `/mcp` route is loopback
> and **unauthenticated**: anything that can open a socket can bypass `localdb mcp`
> entirely and talk to the unscoped endpoint directly. `--store` stops an agent from
> *accidentally* reading another project's docs; it does not contain a hostile one.
> Real containment needs daemon-side authentication, which does not exist in v1. In
> embedded mode there is no such endpoint, so the scope is as strong as the process
> boundary.

---

## Tools

The server exposes four read-only tools. Write tools are reserved for a future
`--allow-write` release.

**`--allow-write` currently has no effect.** v1 registers no mutating tool on any
transport, so the tool set is byte-identical with and without the flag; passing it
prints a warning to stderr saying so. It is accepted today only so the CLI surface is
stable for callers. (Unlike a misapplied `--store`, which exits 2, this one only warns:
`--allow-write` fails *safe* — it can withhold a capability, never widen access.)

### `search`

Hybrid search (BM25 + dense vector) across indexed stores. Returns a ranked list
of citations in the canonical localdb Citation JSON shape.

> **Note:** the dense component uses the configured embedder (default: `pplx-embed-context-v1-0.6b`
> local ONNX). The model is downloaded automatically on first use (~706 MB). See
> [../specs/04-search-pipeline.md](../specs/04-search-pipeline.md) for the pipeline details.

**Input schema** (as actually returned by `tools/list`):

```json

{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "content_length": {
      "default": null,
      "description": "Soft cap on snippet text chars per result in the text rendering; snaps to the nearest paragraph/sentence/word boundary rather than cutting mid-word (default: 400). The JSON citation payload always carries the full snippet.",
      "format": "int64",
      "minimum": 1,
      "type": [
        "integer",
        "null"
      ]
    },
    "limit": {
      "default": null,
      "description": "Maximum number of results to return (default: 10, max: 100)",
      "format": "int64",
      "maximum": 100,
      "minimum": 1,
      "type": [
        "integer",
        "null"
      ]
    },
    "query": {
      "description": "Natural language search query",
      "type": "string"
    },
    "stores": {
      "default": null,
      "description": "Optional list of store names to search. Defaults to all stores.",
      "items": {
        "type": "string"
      },
      "type": [
        "array",
        "null"
      ]
    }
  },
  "required": [
    "query"
  ],
  "type": "object"
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

**Example result.** The single `text` content block carries the pretty-printed JSON,
then a `\n\n---\n` separator, then a human-readable rendering of the same citations
(`search` is the only tool that appends this rendering; the others return JSON alone):

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "isError": false,
    "content": [
      {
        "type": "text",
        "text": "{\n  \"citations\": [\n    {\n      \"block\": {\n        \"kind\": \"text\",\n        \"seq\": 0\n      },\n      \"chunk_id\": \"0bbaaa6b64dffd8b232410017b224c7b499bc3fe235382bfaa8ea63b1e435824\",\n      \"chunk_position\": {\n        \"seq_in_block\": 0\n      },\n      \"heading_path\": [],\n      \"location\": {\n        \"span\": {\n          \"end\": 165,\n          \"start\": 0\n        }\n      },\n      \"metadata\": {\n        \"contributor\": [],\n        \"coverage\": null,\n        \"creator\": [],\n        \"date\": null,\n        \"description\": null,\n        \"format\": \"text/plain\",\n        \"identifier\": null,\n        \"kind\": \"document\",\n        \"language\": null,\n        \"page_count\": null,\n        \"publisher\": null,\n        \"relation\": [],\n        \"rights\": null,\n        \"source\": null,\n        \"subject\": [],\n        \"title\": null,\n        \"type\": null,\n        \"word_count\": null\n      },\n      \"provenance\": {\n        \"content_hash\": \"226aa53267d613baa9aaf444cf661ef20a2e9d8e1e9d140819ee2f7044320e4b\",\n        \"fetched_at\": \"2026-06-11T14:17:30Z\"\n      },\n      \"resource_id\": \"5e16a53946004c13b941685cddaed55d9267965abe65462bbe75d8e6184f15e7\",\n      \"score\": {\n        \"bm25\": 3.0748,\n        \"dense\": 0.7099609375,\n        \"fused\": 0.03278688524590164\n      },\n      \"snippet\": \"Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark connectors are deferred to the next milestone.\",\n      \"store\": {\n        \"id\": \"01KTVGQ62TQN8X6XN9E5FDZN67\",\n        \"name\": \"notes\"\n      },\n      \"title\": null,\n      \"uri\": \"file:///home/user/notes/meeting.txt\"\n    }\n  ],\n  \"total_candidates\": 3\n}\n\n---\n1. file:///home/user/notes/meeting.txt\n   Score: 0.0328\n   Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark connectors are deferred to the next milestone."
      }
    ]
  }
}
```

(`metadata` here is mostly null/empty because a plain `.txt` file carries no Dublin Core
metadata — only `format` is set, from the sniffed MIME type. Every Dublin Core field is
always present in the JSON; absent values serialize as `null` (or `[]` for the repeated
fields), never omitted. How much gets populated depends entirely on the parser:

| Parser | Populates |
|---|---|
| EPUB | `title`, `creator`, `subject`, `description`, `publisher`, `contributor`, `date`, `format`, `identifier`, `language`, `rights` — read from the OPF package's own Dublin Core block |
| Markdown / HTML / Office | `title` (first H1) + `format` |
| PDF | `format` only — the info dictionary is not read |
| Plain text | `format` only |

EPUB is currently the only parser that populates the rich Dublin Core set. YAML front
matter is preserved as a `Frontmatter` content block but is **not** parsed into metadata —
see [#195](https://github.com/dokterbob/localdb/issues/195). The top-level `title` field is
a convenience copy of `metadata`'s title.)

(The structural fields above — `block`, `chunk_position`, `heading_path`,
`location.span`, `snippet`, `metadata`, `chunk_id`, `resource_id` and
`provenance.content_hash` — are captured from a real indexing run. `score`,
`store` and `provenance.fetched_at` are illustrative.)

The citation shape is identical to `localdb search --json`. There is no top-level
`document_id`, `block_seq`, `block_kind`, or `span` — those are superseded by
`resource_id`, the nested `block {seq, kind}`, `chunk_position {seq_in_block}`, and
`location {span, window_block_seqs}` respectively. See
[../specs/02-domain-model.md](../specs/02-domain-model.md) §6 for field definitions.

---

### `get_document`

Fetch the normalized text and metadata for a document by its ID.

**Input schema:**

```json

{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "id": {
      "default": "",
      "description": "Document ID (content-addressed blake3 hash)",
      "type": "string"
    },
    "store": {
      "default": null,
      "description": "Store id or name to restrict the lookup to (e.g. the store.id or store.name from a search result's citation). Defaults to scanning all available stores and returning the first match.",
      "type": [
        "string",
        "null"
      ]
    },
    "uri": {
      "default": null,
      "description": "Document URI (e.g. file:///path/to/doc or URL)",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "type": "object"
}
```

> **v1 limitation:** `uri`-based lookup is not supported. Pass the document `id`
> from a `search` citation. Sending a `uri` without `id` returns `isError: true` with the
> message: `"uri-based get_document is not supported in v1; use the document 'id'
> from a search result"`.

> **Store disambiguation (#144):** pass `store` — the `store.id` or `store.name`
> from a search citation — when the document `id` might exist in more than one
> store; it is resolved the same way as `search`'s `stores` argument, and an
> unknown `store` returns `store_not_found`. Omitting `store` scans every
> available store and returns the first match, which is ambiguous (effectively
> a coin flip) if the id exists in more than one.

**Example call:**

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "tools/call",
  "params": {
    "name": "get_document",
    "arguments": { "id": "5e16a53946004c13b941685cddaed55d9267965abe65462bbe75d8e6184f15e7" }
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
        "text": "{\n  \"chunk_count\": 1,\n  \"metadata\": {\n    \"contributor\": [],\n    \"coverage\": null,\n    \"creator\": [],\n    \"date\": null,\n    \"description\": null,\n    \"format\": \"text/plain\",\n    \"identifier\": null,\n    \"kind\": \"document\",\n    \"language\": null,\n    \"page_count\": null,\n    \"publisher\": null,\n    \"relation\": [],\n    \"rights\": null,\n    \"source\": null,\n    \"subject\": [],\n    \"title\": null,\n    \"type\": null,\n    \"word_count\": null\n  },\n  \"provenance\": {\n    \"content_hash\": \"226aa53267d613baa9aaf444cf661ef20a2e9d8e1e9d140819ee2f7044320e4b\",\n    \"fetched_at\": \"2026-06-11T14:17:30Z\"\n  },\n  \"resource_id\": \"5e16a53946004c13b941685cddaed55d9267965abe65462bbe75d8e6184f15e7\",\n  \"store\": {\n    \"id\": \"01KTVGQ62TQN8X6XN9E5FDZN67\",\n    \"name\": \"notes\"\n  },\n  \"text\": \"Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark connectors are deferred to the next milestone.\",\n  \"title\": null,\n  \"uri\": \"file:///home/user/notes/meeting.txt\"\n}"
      }
    ]
  }
}
```

---

### `get_chunks`

Fetch a document's chunks in storage order — `(block_seq, seq_in_block)` — paginated
by `offset`/`limit`, or by an anchor position (`anchor_chunk_id`/`anchor_block_seq`,
see [Anchor-relative pagination](#anchor-relative-pagination) below). Use this to
page through a long document after finding it via `search` or `get_document`.

**Input schema:**

```json

{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "anchor_block_seq": {
      "default": null,
      "description": "Resolve via lower-bound to the first chunk with block_seq >= anchor_block_seq (tie-broken by lowest seq_in_block), then return a window of `limit` chunks centered on it. Mutually exclusive with `offset` and `anchor_chunk_id`.",
      "format": "uint32",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "anchor_chunk_id": {
      "default": null,
      "description": "Resolve to the chunk with this exact chunk_id, then return a window of `limit` chunks centered on it. Mutually exclusive with `offset` and `anchor_block_seq`.",
      "type": [
        "string",
        "null"
      ]
    },
    "limit": {
      "default": null,
      "description": "Maximum number of chunks to return (default: 50, max: 200)",
      "format": "int64",
      "maximum": 200,
      "minimum": 1,
      "type": [
        "integer",
        "null"
      ]
    },
    "offset": {
      "default": null,
      "description": "Number of chunks to skip before the first returned chunk (default: 0)",
      "format": "int64",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "resource_id": {
      "description": "Resource ID (content-addressed blake3 hash)",
      "type": "string"
    },
    "store": {
      "default": null,
      "description": "Store id or name to restrict the lookup to (e.g. the store.id or store.name from a search result's citation). Defaults to scanning all available stores and returning the first match.",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "resource_id"
  ],
  "type": "object"
}
```

> Like `get_document`, `uri`-based lookup is not supported — use a `resource_id`
> obtained from a prior `search` or `get_document` call. An unknown `resource_id`
> returns `resource_not_found`. An `offset` past the end of the chunk list returns an
> empty `chunks` array, not an error.

> **Store disambiguation (#144):** pass `store` — the `store.id` or `store.name`
> from a search citation — when the resource id might exist in more than one
> store; resolved the same way as `get_document`'s `store` argument. Omitting
> it scans every available store and returns the first match.

**Example call:**

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "tools/call",
  "params": {
    "name": "get_chunks",
    "arguments": { "resource_id": "5e16a53946004c13b941685cddaed55d9267965abe65462bbe75d8e6184f15e7", "offset": 0, "limit": 50 }
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
        "text": "{\n  \"anchor_index\": null,\n  \"chunks\": [\n    {\n      \"block_kind\": \"text\",\n      \"block_seq\": 0,\n      \"chunk_id\": \"0bbaaa6b64dffd8b232410017b224c7b499bc3fe235382bfaa8ea63b1e435824\",\n      \"heading_path\": [],\n      \"seq_in_block\": 0,\n      \"span\": {\n        \"end\": 165,\n        \"start\": 0\n      },\n      \"text\": \"Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results. Aardvark connectors are deferred to the next milestone.\"\n    }\n  ],\n  \"limit\": 50,\n  \"offset\": 0,\n  \"resource_id\": \"5e16a53946004c13b941685cddaed55d9267965abe65462bbe75d8e6184f15e7\",\n  \"returned\": 1,\n  \"store\": {\n    \"id\": \"01KTVGQ62TQN8X6XN9E5FDZN67\",\n    \"name\": \"notes\"\n  },\n  \"title\": null,\n  \"total_chunks\": 1,\n  \"uri\": \"file:///home/user/notes/meeting.txt\"\n}"
      }
    ]
  }
}
```

`anchor_index` is `null` here because this call used plain `offset` pagination; see
below for what it carries on an anchor-based call.

---

### Anchor-relative pagination

As an alternative to `offset`, `get_chunks` accepts `anchor_chunk_id` (string) or
`anchor_block_seq` (integer ≥ 0). `offset`, `anchor_chunk_id`, and `anchor_block_seq`
are **pairwise mutually exclusive** — passing more than one of the three in the same
call is a tool-level `invalid_request` error, not a silent precedence rule.

Anchor resolution runs over the resource's full chunk list, sorted the same way as
the plain-`offset` path — `(block_seq, seq_in_block)`:

- `anchor_chunk_id` resolves to the chunk with that exact `chunk_id`. Unknown
  `anchor_chunk_id` → `chunk_not_found`.
- `anchor_block_seq` resolves via lower-bound: the first chunk with
  `block_seq >= anchor_block_seq`, tie-broken by the lowest `seq_in_block` at that
  `block_seq`. If `anchor_block_seq` is past every block in the resource, this is
  also `chunk_not_found`.

Once an anchor resolves to a position in the full chunk list, the response window is
`limit` chunks **centered** on that position — the anchor sits at, or as close as
possible to, the middle of the returned page — clamped at the start/end of the
resource's chunk list. The window never shrinks below `limit` chunks purely because
the anchor is near an edge (it shifts toward the interior instead); it only returns
fewer than `limit` chunks when the resource has fewer than `limit` chunks in total.
The response's `offset` field reports the effective offset the returned window
corresponds to (as if the caller had passed that `offset` directly), and
`anchor_index` reports the 0-based index of the anchor chunk within the returned
`chunks` array — `null` when the request used plain `offset` pagination instead.

**Example** (illustrative — placeholder IDs and elided `text`/`span` values, to show the
windowing arithmetic): a resource with 20 chunks (`block_seq` 0–19, one chunk per block),
requested with `anchor_chunk_id` set to the `block_seq = 10` chunk and `limit: 5`.
With an odd `limit`, centering puts 2 chunks before the anchor and 2 after, so the
returned window covers `block_seq` 8–12, `offset` is 8 (the position of the first
returned chunk in the full ordered list), and the anchor is the 3rd of the 5 returned
chunks (`anchor_index: 2`):

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "method": "tools/call",
  "params": {
    "name": "get_chunks",
    "arguments": { "resource_id": "<resource_id of a 20-chunk document>", "anchor_chunk_id": "<chunk_id at block 10>", "limit": 5 }
  }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "result": {
    "isError": false,
    "content": [
      {
        "type": "text",
        "text": "{\n  \"anchor_index\": 2,\n  \"chunks\": [\n    { \"block_kind\": \"text\", \"block_seq\": 8, \"chunk_id\": \"...\", \"heading_path\": [], \"seq_in_block\": 0, \"span\": { \"end\": 0, \"start\": 0 }, \"text\": \"...\" },\n    { \"block_kind\": \"text\", \"block_seq\": 9, \"chunk_id\": \"...\", \"heading_path\": [], \"seq_in_block\": 0, \"span\": { \"end\": 0, \"start\": 0 }, \"text\": \"...\" },\n    { \"block_kind\": \"text\", \"block_seq\": 10, \"chunk_id\": \"<anchor chunk_id>\", \"heading_path\": [], \"seq_in_block\": 0, \"span\": { \"end\": 0, \"start\": 0 }, \"text\": \"...\" },\n    { \"block_kind\": \"text\", \"block_seq\": 11, \"chunk_id\": \"...\", \"heading_path\": [], \"seq_in_block\": 0, \"span\": { \"end\": 0, \"start\": 0 }, \"text\": \"...\" },\n    { \"block_kind\": \"text\", \"block_seq\": 12, \"chunk_id\": \"...\", \"heading_path\": [], \"seq_in_block\": 0, \"span\": { \"end\": 0, \"start\": 0 }, \"text\": \"...\" }\n  ],\n  \"limit\": 5,\n  \"offset\": 8,\n  \"resource_id\": \"<resource_id>\",\n  \"returned\": 5,\n  \"store\": { \"id\": \"<store id>\", \"name\": \"<store>\" },\n  \"title\": null,\n  \"total_chunks\": 20,\n  \"uri\": \"file:///home/user/notes/research-log.md\"\n}"
      }
    ]
  }
}
```

If the same `anchor_chunk_id` (`block_seq = 10`) were requested with `limit: 30`
against this 20-chunk resource, the window would clamp to the whole list:
`offset: 0`, `returned: 20`, `anchor_index: 10`.

See [specs/05-surfaces.md](../specs/05-surfaces.md) §4.1 for the full spec
(issue #146).

---

### `list_stores`

List all available stores with their names, visibility, and document/chunk counts.

**Input schema:** `{"properties": {}, "type": "object"}` (no arguments)

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

### Diagnosing a rejected connection

`localdb serve` and `localdb mcp` log rejected/failed connections at `warn` level:
a 4xx/5xx response from any daemon route (`/v1/*` or the nested `/mcp` mount — including
rmcp's own Host-header check, see below) is logged with method, path, status, and the
request's `Host` header; a failed proxy connect from `localdb mcp` to the daemon (stale
`LOCALDB_DAEMON_URL`, or the daemon going away between the initial probe and the actual
connect) is logged with the daemon URL and the underlying transport error.

Both surface on stderr **by default**, no `RUST_LOG` needed — `localdb`'s default log
filter is `warn,pdf_oxide=off` (set in `localdb/src/main.rs`), which passes `warn`-level
events through. Set `RUST_LOG=debug` (or `RUST_LOG=localdb=debug`) for more detail.

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
