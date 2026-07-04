# Spec 05 — Surfaces: CLI, HTTP API, MCP

> Status: accepted draft, revised 2026-06-30. All three surfaces sit on the same `core`
> ([01-architecture.md](01-architecture.md) §1) and return the same Citation shape
> ([02-domain-model.md](02-domain-model.md) §6) and error taxonomy (§5).

## 1. Process-model behavior shared by CLI and MCP

Every command/tool first probes the daemon socket ([01-architecture.md](01-architecture.md) §3):
daemon present → thin client over its HTTP API; absent → embedded mode (open store in-process).
The behavior difference per command is noted below; users should rarely need to care.

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

Output: human-readable by default (citations as `uri:heading_path` + snippet), `--json` emits the
canonical structures for scripting. The CLI is **command-oriented**; interactive browse is a
roadmap item with the web UI.

## 3. HTTP API

**Decision:** **REST + JSON, the canonical surface for external integrators.** Served only by the
daemon. **Rejected:** gRPC (worse curl-ability and browser story for a local tool; can be added
later if a consumer demands it).

- **Bind & trust:** `127.0.0.1` by default, **no auth in local mode** — documented trust
  assumption: anything on this machine that can reach localhost is trusted, same boundary as the
  files themselves. Binding to a non-loopback address without auth configured is a **refused
  startup**, not a warning (forward-compatible with the shared/home-server mode in
  [06-roadmap.md](06-roadmap.md) §1, which arrives together with real auth).
- **Resources** (`/v1`): `GET/POST /stores`, `GET/PATCH/DELETE /stores/{id}`,
  `GET/POST /stores/{id}/sources`, `POST /search` (body: query, store filter, metadata filters,
  limit; citations carry full `Metadata`), `GET /resources/{id}` (response includes
  `metadata: Metadata`), `POST /jobs` (index requests), `GET /jobs/{id}`, `GET /status`,
  `GET /config` (resolved config).
- **Long-running work:** indexing is a **job resource**: `POST /jobs` → `202` + job; clients poll
  `GET /jobs/{id}`. SSE progress streaming is roadmap ([06-roadmap.md](06-roadmap.md) §5) — the
  job resource is designed so SSE adds a representation, not a new model.
- **Pagination:** cursor-based (`?cursor=`, `?limit=`) on list endpoints from day one.

## 4. MCP

**Decision:** v1 MCP is **read-only**: tools `search` (args: query, optional store names, limit, optional content_length →
Citation list as structured content; each citation carries full `Metadata`),
`get_document` (id or uri → block texts + `metadata: Metadata`),
`get_chunks` (document_id, optional offset/limit → the document's chunks in order, paginated),
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

Returns a document's chunks in storage order — `(block_seq, seq_in_block)` — with pagination.
Args: `document_id` (required), `offset` (integer ≥ 0, default 0), `limit` (integer 1..=200,
default 50). Like `get_document`, `uri`-based lookup is not supported in v1 — callers must use a
document ID obtained from a prior `search` or `get_document` call. Unknown `document_id` →
`document_not_found`. An `offset` past the end of the chunk list returns an empty `chunks` array,
not an error — this is not a usage mistake worth surfacing as one.

Response shape:

```json
{
  "document_id": "...",
  "uri": "...",
  "title": "...",
  "store": { "id": "...", "name": "..." },
  "total_chunks": 0,
  "offset": 0,
  "limit": 0,
  "returned": 0,
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

## 5. Shared error taxonomy

One enum in `core`; every surface maps it mechanically (HTTP status / CLI exit code + stderr /
MCP tool error). Codes are stable API:

| Code | Meaning | HTTP |
|---|---|---|
| `store_not_found` / `source_not_found` / `resource_not_found` / `job_not_found` | Unknown entity | 404 |
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
