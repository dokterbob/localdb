# Spec 05 — Surfaces: CLI, HTTP API, MCP

> Status: accepted draft, revised 2026-06-30. All three surfaces sit on the same `core`
> ([01-architecture.md](01-architecture.md) §1) and return the same Citation shape
> ([02-domain-model.md](02-domain-model.md) §6) and error taxonomy (§5).

## 1. Process-model behavior shared by CLI and MCP

Every command/tool first probes the daemon socket ([01-architecture.md](01-architecture.md) §3):
daemon present → thin client over its HTTP API; absent → embedded mode (open store in-process).
The client's base URL for the HTTP API comes from the daemon's recorded discovery URL (§3), not a
hardcoded default, so this works for any configured bind address or port. The behavior difference
per command is noted below; users should rarely need to care.

## 2. CLI

Single binary, subcommand tree. Global flags: `--config`, `--json`, `--store <name>` (repeatable).

| Command | Purpose | Daemonless (embedded) | Daemon-attached |
|---|---|---|---|
| `init` | Create config + data dir, first-run model download prompt | full | n/a (refuses if daemon running with different data dir) |
| `serve` | Run the daemon (HTTP API, watching, refresh, socket) | becomes the daemon | error `daemon_running` |
| `mcp` | Run MCP server on stdio | embedded core | thin client |
| `status` | Stores, resource/chunk counts, policy staleness, daemon state | reads directly | queries daemon |
| `store add/list/remove` | Manage runtime-owned stores | direct write | routed to daemon |
| `source add/list/remove` | Manage sources on a store | direct write | routed to daemon |
| `add <path|url>...` | Alias for `source add` — add one or more sources to a store | direct write | routed to daemon |
| `index [--store S] [--source ID] [--strict]` | One-shot scan & index; creates IndexJob | runs job synchronously, progress to stderr | submits job, polls, streams progress |
| `search <query>... [--limit N] [--content-length N]` | Hybrid search with citations; `--content-length` is a **soft cap** on human-readable snippet chars (default 1000; JSON output always full text) — see §4 for the snapping behavior shared with MCP | embedded read | via API |
| `db status` | Inspect schema state: current version, head version, pending/unsupported steps. Never refuses, even on a store newer than the binary | reads directly | error `daemon_running` |
| `db migrate` | Apply pending migrations with per-step progress; legacy v1–v3 rebuild and any other destructive step require confirmation; prints a `localdb index` hint when a weight-class-3 migration ran | direct write | error `daemon_running` |
| `db downgrade [--to N]` | Reverse migrations down to version `N` (default: one step) using stored down-SQL; requires confirmation; refuses cleanly on a step with `down_unsupported_reason` | direct write | error `daemon_running` |

Output: human-readable by default (citations as `uri:heading_path` + snippet), `--json` emits the
canonical structures for scripting. The CLI is **command-oriented**; interactive browse is a
roadmap item with the web UI.

### 2.1 Schema migrations

All schema-version mismatches on open — on every surface, CLI, HTTP daemon, and MCP alike — map
to `invalid_config` / exit 2 with an actionable hint (§5); no surface auto-migrates on open.
`db migrate` and `db downgrade` are **CLI-only**: the HTTP daemon and MCP never apply migrations,
they only ever surface the refusal-with-hint. Both commands require the daemon to be stopped —
run against a live daemon they fail the same way every other daemon-aware write command does,
error `daemon_running`, exit 4. Destructive paths (the legacy v1–v3 rebuild inside `db migrate`,
and `db downgrade`) require explicit confirmation before touching the store. See
[02-domain-model.md](02-domain-model.md) §9 for the `schema_migrations` table and the
migration-weight-class design.

### 2.2 Feed sources

Both `localdb add <url>...` and `localdb source add <url>...` accept `--kind <path|url|feed>` to
override auto-classification. `--kind feed` requires an `http(s)://` argument — anything else is
`invalid_request`, exit 2. Two more flags apply only when the effective kind is `feed`:
`--max-entries <N>` (`0` is rejected) and `--no-fetch-full-content` (selects single-document mode,
[02-domain-model.md](02-domain-model.md) §2). Passing either flag when the effective kind is not
`feed` is `invalid_request`, exit 2 — not silently ignored. `--help` for `add`/`source add` notes
that feed ingestion in the default (discovery) mode fetches every entry's linked page and
recommends `--max-entries` to bound that.

`source list` (human) renders feed rows as `{id} [feed] {url} (max_entries=…, full_content=on|off)`
— `…` is the configured integer or `none`. `--json` adds parsed `max_entries` (`null` or integer)
and `fetch_full_content` (bool), reconstructed from `config_json` (never the raw column), and now
also surfaces `refresh` for both `url` and `feed` sources.

## 3. HTTP API

**Decision:** **REST + JSON, the canonical surface for external integrators.** Served only by the
daemon. **Rejected:** gRPC (worse curl-ability and browser story for a local tool; can be added
later if a consumer demands it).

- **Bind & trust:** `127.0.0.1` by default, **no auth in local mode** — documented trust
  assumption: anything that can reach the bind address is trusted, same boundary as the files
  themselves. Any bind address is accepted; the daemon does not refuse to start based on it.
  Binding to a specific non-loopback address (e.g. a LAN or VPN IP) is treated as a deliberate
  trust decision by the user and starts silently. Binding to all interfaces (`0.0.0.0`, `::`, or
  any other address form the OS resolves to the unspecified address) logs a warning at startup —
  checked against the address the OS actually bound, not the raw config string, so aliases the
  string form can't see are still caught — since it makes the unauthenticated daemon reachable
  from any network the machine is on and is the one case a user could plausibly not realize how
  exposed this makes them. The daemon also records its client-reachable base URL (loopback
  substituted for a wildcard bind) in a discovery file so CLI/MCP clients can find it regardless
  of bind address or port ([01-architecture.md](01-architecture.md) §3).
- **Resources** (`/v1`): `GET/POST /stores`, `GET/PATCH/DELETE /stores/{id}`,
  `GET/POST /stores/{id}/sources`, `POST /search` (body: query, store filter, metadata filters,
  limit; citations carry full `Metadata`), `GET /resources/{id}` (response includes
  `metadata: Metadata`), `POST /jobs` (index requests), `GET /jobs/{id}`, `GET /status`,
  `GET /config` (resolved config).
- **Feed sources:** `POST /stores/{id}/sources` accepts `{kind: "feed", spec: {url, max_entries,
  fetch_full_content}, preset, refresh}` — `spec` mirrors `SourceSpec::Feed`
  ([02-domain-model.md](02-domain-model.md) §2). Validation failures (`max_entries: 0`, a
  non-`http(s)` `url`, etc.) are `invalid_request`, 400. `GET .../sources` reconstructs a clean
  `spec` object per source from `config_json` (never the raw column) and now surfaces `refresh`
  for both `url` and `feed` sources. Feed's `refresh` is persisted and validated the same as
  `url`'s but is currently inert — no scheduled refresh runs yet for either kind.
- **Long-running work:** indexing is a **job resource**: `POST /jobs` → `202` + job; clients poll
  `GET /jobs/{id}`. SSE progress streaming is roadmap ([06-roadmap.md](06-roadmap.md) §5) — the
  job resource is designed so SSE adds a representation, not a new model.
- **Pagination:** cursor-based (`?cursor=`, `?limit=`) on list endpoints from day one.

## 4. MCP

**Decision:** v1 MCP is **read-only**: tools `search` (args: query, optional store names, limit, optional content_length →
Citation list as structured content; each citation carries full `Metadata`),
`get_document` (id or uri → block texts + `metadata: Metadata`),
`get_chunks` (resource_id, optional offset/limit, or optional anchor_chunk_id/anchor_block_seq
(§4.1) → the resource's chunks in order, paginated),
`list_stores` (names, visibility, counts). **Mutating tools** (`add_source`, `reindex`, …) are a
follow-up behind an explicit opt-in flag (`localdb mcp --allow-write`), never on by default.

**Rationale:** the dominant agent use case is retrieval; a read-only surface has a trivially
auditable blast radius, and write semantics through agents deserve their own design pass.
**Rejected:** full CRUD via MCP in v1.

Citations cross MCP as structured tool results (the JSON shape from
[02-domain-model.md](02-domain-model.md) §6), with a short text rendering alongside for
non-structured clients (text rendering includes `creator · date` where present).
Resources/prompts: none in v1; resources are reachable via `get_document` / `get_chunks`.

### 4.1 `get_chunks`

Returns a resource's chunks in storage order — `(block_seq, seq_in_block)` — with pagination.
Args: `resource_id` (required), `offset` (integer ≥ 0, default 0), `limit` (integer 1..=200,
default 50). Like `get_document`, `uri`-based lookup is not supported in v1 — callers must use a
resource ID obtained from a prior `search` or `get_document` call. Unknown `resource_id` →
`resource_not_found`. An `offset` past the end of the chunk list returns an empty `chunks` array,
not an error — this is not a usage mistake worth surfacing as one.

**Anchor-relative pagination (#146):** as an alternative to `offset`, `get_chunks` accepts
`anchor_chunk_id` (string) or `anchor_block_seq` (integer ≥ 0). `offset`, `anchor_chunk_id`,
and `anchor_block_seq` are mutually exclusive — passing more than one of the three is a
tool-level `invalid_request` error, not a silent precedence rule.

Anchor resolution runs over the resource's full chunk list, sorted the same way as the
plain-`offset` path — `(block_seq, seq_in_block)`:

- `anchor_chunk_id` resolves to the chunk with that exact `chunk_id`. Unknown
  `anchor_chunk_id` → `chunk_not_found`.
- `anchor_block_seq` resolves via lower-bound: the first chunk with `block_seq >=
  anchor_block_seq`, tie-broken by the lowest `seq_in_block` at that `block_seq`. If
  `anchor_block_seq` is past every block in the resource (no chunk satisfies the lower-bound),
  this is also `chunk_not_found`.

Once an anchor resolves to a position in the full chunk list, the response window is `limit`
chunks **centered** on that position — the anchor sits at, or as close as possible to, the
middle of the returned page — clamped at the start/end of the resource's chunk list. The
window never shrinks below `limit` chunks purely because the anchor is near an edge (it
shifts toward the interior instead); it only returns fewer than `limit` chunks when the
resource has fewer than `limit` chunks in total. The response's `offset` field reports the
effective offset the returned window corresponds to (as if the caller had passed that
`offset` directly), and a new `anchor_index` field reports the 0-based index of the anchor
chunk within the returned `chunks` array — `null` when the request used plain `offset`
pagination instead of an anchor.

Response shape (plain `offset` pagination):

```json
{
  "resource_id": "...",
  "uri": "...",
  "title": "...",
  "store": { "id": "...", "name": "..." },
  "total_chunks": 0,
  "offset": 0,
  "limit": 0,
  "returned": 0,
  "anchor_index": null,
  "chunks": [
    {
      "chunk_id": "...",
      "block_seq": 0,
      "seq_in_block": 0,
      "block_kind": "...",
      "span": { "start": 0, "end": 0 },
      "heading_path": ["..."],
      "text": "..."
    }
  ]
}
```

**Anchor example:** a resource with 20 chunks (`block_seq` 0–19, one chunk per block),
requested with `anchor_chunk_id` set to the `block_seq = 10` chunk and `limit: 5`. With an
odd `limit`, centering puts 2 chunks before the anchor and 2 after, so the returned window
covers `block_seq` 8–12, `offset` is 8 (the position of the first returned chunk in the full
ordered list), and the anchor is the 3rd of the 5 returned chunks (`anchor_index: 2`):

Request:

```json
{ "resource_id": "...", "anchor_chunk_id": "...", "limit": 5 }
```

Response:

```json
{
  "resource_id": "...",
  "uri": "...",
  "title": "...",
  "store": { "id": "...", "name": "..." },
  "total_chunks": 20,
  "offset": 8,
  "limit": 5,
  "returned": 5,
  "anchor_index": 2,
  "chunks": [
    { "chunk_id": "...", "block_seq": 8, "seq_in_block": 0, "block_kind": "...", "span": { "start": 0, "end": 0 }, "heading_path": ["..."], "text": "..." },
    { "chunk_id": "...", "block_seq": 9, "seq_in_block": 0, "block_kind": "...", "span": { "start": 0, "end": 0 }, "heading_path": ["..."], "text": "..." },
    { "chunk_id": "...", "block_seq": 10, "seq_in_block": 0, "block_kind": "...", "span": { "start": 0, "end": 0 }, "heading_path": ["..."], "text": "..." },
    { "chunk_id": "...", "block_seq": 11, "seq_in_block": 0, "block_kind": "...", "span": { "start": 0, "end": 0 }, "heading_path": ["..."], "text": "..." },
    { "chunk_id": "...", "block_seq": 12, "seq_in_block": 0, "block_kind": "...", "span": { "start": 0, "end": 0 }, "heading_path": ["..."], "text": "..." }
  ]
}
```

If the same `anchor_chunk_id` (`block_seq = 10`) were requested with `limit: 30` against
this 20-chunk resource, the window would clamp to the whole list: `offset: 0`,
`returned: 20`, `anchor_index: 10`.

`content_length` (default 400) is a **soft cap**, not a hard truncation point: the JSON
citation payload always carries the full, untruncated snippet — only the human-readable
text rendering is shortened. The text rendering snaps its cut point to the nearest natural
boundary at or below the cap, checked in priority order: paragraph break (`\n\n`) → sentence
terminator (`.`/`!`/`?`, optionally followed by a closing quote/bracket, then whitespace or
end-of-text) → word boundary (last whitespace at or before the cap) → hard UTF-8
char-boundary cut as a last resort. A bounded overshoot (up to ~20% over the cap) is allowed
so a paragraph/sentence boundary just past the cap is preferred over a mid-word hard cut;
word/char fallback never overshoots. An ellipsis (`…`) is appended whenever the snippet was
actually shortened. This logic lives in `core` (`localdb_core::snippet::truncate_snippet`)
and also backs the CLI's `--content-length` (§2) — the CLI additionally collapses whitespace
before truncating, which removes `\n\n` paragraph breaks, so only sentence/word snapping
applies on that path. `context_sentences` (an alternative sentence-count-based unit) is out
of scope for this design.

### 4.2 Transports and process model

MCP is served over two transports, built on the official `rmcp` SDK:

- **Stdio** (`localdb mcp`): if no daemon is running, the CLI opens the store(s) embedded
  in-process and serves them directly. If a daemon is already running (detected the same way
  every other daemon-aware CLI command detects it, §1), `localdb mcp` instead **proxies**
  every request verbatim to that daemon's own `/mcp` HTTP route below, rather than opening the
  store a second time. The stdio caller cannot tell which mode is in effect except by behavior:
  proxied mode exposes whatever store set the daemon had at its own startup, unfiltered.
  **Known v1 gap:** `--store` is not honored in proxied mode — the daemon's `/mcp` route has no
  concept of a per-stdio-session store filter, and building client-side re-filtering for this
  narrow case was rejected as not worth the complexity in v1. `localdb mcp --store <name>`
  against a running daemon prints a non-fatal warning to stderr and serves the daemon's full
  store set regardless of the flag; this is a documented limitation, not a bug.
- **HTTP** (`/mcp`, mounted on the daemon alongside its own `/v1` routes): a startup-time
  snapshot of stores, not rebuilt per session — a store added later via `/v1/stores` is
  invisible over MCP until the daemon restarts (see `mcp::http::build_streamable_http_service`'s
  doc comment). HTTP MCP sessions always run with `allow_write = false`.

Tool registration (the four read-only tools) and business logic are identical on both
transports and in both stdio modes — only the code path serving the request differs.

### 4.3 Error model

MCP failures split into exactly two tiers, by whether the request could be *routed* to a tool
at all:

- **Protocol-level** (a JSON-RPC error): the tool name itself is unregistered. `rmcp`'s
  macro-generated dispatch returns `ErrorCode::INVALID_PARAMS` ("tool not found") for any name
  not in the tool router. This is the one case a caller cannot recover from within the tool
  result.
- **Tool-level** (`CallToolResult { isError: true, .. }`): everything else — including cases
  one might expect to be protocol-level. A missing or wrong-typed *required* argument (e.g.
  `search`'s `query`, `get_chunks`'s `resource_id`) fails `rmcp`'s `Parameters<T>`
  deserialization, which itself produces a protocol-level `ErrorData::invalid_params` — but
  `rmcp` 1.8.0's tool router downgrades that specific case to a tool-level result via
  `into_tool_argument_error`, so the caller's MCP client can render it like any other tool
  result. This is a real behavior difference from what an initial reading of the `rmcp` API
  might suggest; it was verified empirically (`mcp/tests/mcp_protocol.rs`), not assumed. Our own
  semantic validation (empty strings, out-of-range `limit`/`offset`, unknown store names,
  not-found lookups) is always tool-level, carrying a `{"error": {"code", "message"}}` JSON
  body as its text content.

Proxied stdio mode forwards whichever tier the daemon's own `/mcp` route returns unchanged —
the proxy never re-tiers an error it received an answer for. A failure of the proxy hop itself
(the daemon unreachable, the connection dropped mid-request) is a distinct case: there is no
upstream answer to relay a tier from, so it surfaces as a fresh protocol-level error instead.

## 5. Shared error taxonomy

One enum in `core`; every surface maps it mechanically (HTTP status / CLI exit code + stderr /
MCP tool error). Codes are stable API:

| Code | Meaning | HTTP |
|---|---|---|
| `store_not_found` / `source_not_found` / `resource_not_found` / `job_not_found` / `chunk_not_found` | Unknown entity | 404 |
| `runtime_state_locked` | Unified database locked by another process (busy timeout exceeded) | 409 |
| `daemon_running` / `daemon_unreachable` | Process-model conflicts | 409 / 502 |
| `invalid_config` | Config failed validation (path-precise message) | 422 |
| `invalid_request` | Bad arguments/body | 400 |
| `unsupported_format` | Extraction can't handle the file type (informational in job stats) | 422 |
| `extraction_failed` | Recognized, supported format whose contents could not be extracted (corrupt/truncated). Counted in `error_count` in job stats; produces a WARN per file. | 422 |
| `provider_unavailable` | External embedding endpoint down/misconfigured | 502 |
| `model_missing` | Local model not yet downloaded; message includes the fix | 503 |
| `index_in_progress` | Conflicting job already running for the scope | 409 |
| `internal` | Bug; includes correlation id, logged with backtrace | 500 |

CLI exit codes: `0` ok, `1` internal, `2` invalid usage/config, `3` not found, `4` conflict/locked,
`5` unavailable (daemon/provider/model).

### `localdb index --strict`

By default `index` is **best-effort**: unsupported files are silently counted; extraction failures
produce a per-file WARN but the run continues and exits `0`. Pass `--strict` to exit `2` when any
resource failed (`error_count > 0`). The run always completes — `--strict` never aborts mid-run;
it only affects the final exit code and JSON `"status"` field.
