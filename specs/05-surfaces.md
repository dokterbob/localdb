# Spec 05 — Surfaces: CLI, HTTP API, MCP

> Status: accepted draft, revised 2026-07-07. All three surfaces sit on the same `core`
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
| `status` | Stores, resource/chunk counts, policy staleness, daemon state — extended to show the caller's identity (user, role) and cached token expiry once auth lands | reads directly | queries daemon |
| `store add/list/remove` | Manage runtime-owned stores | direct write | routed to daemon |
| `source add/list/remove` | Manage sources on a store | direct write | routed to daemon |
| `add <path|url>...` | Alias for `source add` — add one or more sources to a store | direct write | routed to daemon |
| `index [--store S] [--source ID] [--strict]` | One-shot scan & index; creates IndexJob | runs job synchronously, progress to stderr | submits job, polls, streams progress |
| `search <query>... [--limit N] [--content-length N]` | Hybrid search with citations; `--content-length` is a **soft cap** on human-readable snippet chars (default 1000; JSON output always full text) — see §4 for the snapping behavior shared with MCP | embedded read | via API |
| `login` / `logout` (planned) | Authenticate against a daemon and cache the resulting token, or revoke and clear it | n/a — there is no auth to log into without a daemon (§3.1) | `login` exchanges credentials via `/token` (or an invite/API-key flow) and writes `credentials.json` (§6, [03-config.md](03-config.md) §6); `logout` calls `/revoke` and clears the cached entry |
| `user add\|list\|remove\|set-role` (planned) | Manage user accounts (admin only) | **break-glass**: writes the unified DB's `users` table directly — recovery path when the daemon is unreachable or auth is locked out | routed to daemon, requires an admin bearer token; `user add` also accepts a `--direct-db` flag to bypass a running daemon for lockout recovery |
| `key create\|list\|revoke` (planned) | Manage API keys (`auth_tokens` rows with `kind='api_key'`) for the caller or, as admin, another user | break-glass direct write | routed to daemon; managing another user's key requires admin |
| `store grant\|revoke` (planned) | Grant/revoke a member's read access to a `shared` store (D7); rejected on `private` stores | direct write | routed to daemon, admin only |
| `invite create\|list\|revoke\|requests\|approve\|deny` (planned) | Manage invites and pending access requests (admin only; the full redeem/approve state machine lands in T6) | direct write | routed to daemon, admin only |

Output: human-readable by default (citations as `uri:heading_path` + snippet), `--json` emits the
canonical structures for scripting. The CLI is **command-oriented**; interactive browse is a
roadmap item with the web UI.

**Break-glass commands:** `user`, `key`, `store grant/revoke`, and `invite` all have a
daemonless mode that writes the unified database directly, bypassing HTTP auth entirely. This
is deliberate: it is the recovery path when the daemon can't be reached, or when every admin
credential has been lost or revoked. It carries the same trust assumption as any other
daemonless CLI command — whoever can open the database file is already trusted (§3.1) — so no
additional confirmation is required. `user add --direct-db` extends this to force break-glass
behavior even while a daemon is running, for the lockout-recovery case specifically. None of
the commands in this paragraph are implemented yet (T1 ships only the underlying schema and
`core::auth` types); see the design note above each row.

## 3. HTTP API

**Decision:** **REST + JSON, the canonical surface for external integrators.** Served only by the
daemon. **Rejected:** gRPC (worse curl-ability and browser story for a local tool; can be added
later if a consumer demands it).

- **Bind & trust:** `127.0.0.1` by default. Auth enforcement is controlled by `server.auth:
  auto | required | off` (default `auto`) ([03-config.md](03-config.md) §1) — see §3.1 for what
  "enforced" means. `auto` enforces auth **iff** the daemon is bound to a non-loopback address;
  loopback (`127.0.0.1` / `::1`) under `auto` stays auth-free, same trust boundary as today:
  anything that can reach the bind address is trusted, same as the files themselves. `required`
  always enforces auth regardless of bind address, including loopback. Binding to a specific
  non-loopback address (e.g. a LAN or VPN IP) is still a deliberate trust decision by the user,
  but it is now only accepted if auth ends up enforced (`auto` or `required`) — `off` combined
  with a non-loopback bind is a hard startup error (`invalid_config`): the daemon refuses to
  start rather than exposing an unauthenticated surface to a network. Binding to all interfaces
  (`0.0.0.0`, `::`, or any other address form the OS resolves to the unspecified address) logs a
  warning at startup — checked against the address the OS actually bound, not the raw config
  string, so aliases the string form can't see are still caught — since it makes the daemon
  reachable from any network the machine is on and is the one case a user could plausibly not
  realize how exposed this makes them; this warning applies regardless of auth mode. The daemon
  also records its client-reachable base URL (loopback substituted for a wildcard bind) in a
  discovery file so CLI/MCP clients can find it regardless of bind address or port
  ([01-architecture.md](01-architecture.md) §3). When auth is enforced and zero users exist yet
  (`AuthStore::count_users() == 0`), `serve` prints a one-time setup code to stdout/stderr so the
  operator can bootstrap the first admin user (§3.1).
- **Resources** (`/v1`): `GET/POST /stores`, `GET/PATCH/DELETE /stores/{id}`,
  `GET/POST /stores/{id}/sources`, `POST /search` (body: query, store filter, metadata filters,
  limit; citations carry full `Metadata`), `GET /resources/{id}` (response includes
  `metadata: Metadata`), `POST /jobs` (index requests), `GET /jobs/{id}`, `GET /status`,
  `GET /config` (resolved config).
- **Long-running work:** indexing is a **job resource**: `POST /jobs` → `202` + job; clients poll
  `GET /jobs/{id}`. SSE progress streaming is roadmap ([06-roadmap.md](06-roadmap.md) §5) — the
  job resource is designed so SSE adds a representation, not a new model.
- **Pagination:** cursor-based (`?cursor=`, `?limit=`) on list endpoints from day one.

### 3.1 Authentication

**Status:** foundation only as of this ticket (T1) — `core::auth` (types, `AuthStore` trait,
`AuthService`) and the `store-libsql` persistence layer and schema exist
([02-domain-model.md](02-domain-model.md) §2, §9), but nothing below is enforced yet. No
middleware runs, no route requires a token, and `server.auth` has no runtime effect. Enforcement
lands incrementally across T2–T7; this section describes the target shape so later tickets have
a design to implement against.

**Decision (target shape):** bearer tokens (`Authorization: Bearer ldb_...`) on every route under
`/v1/*` and `/mcp`, once auth is enforced for the daemon (§3). A request with a missing or
invalid token gets `401` with a `WWW-Authenticate: Bearer` header. A request from an
authenticated principal who lacks permission for the resource (e.g. a member reading a store
they have no grant for, or any non-admin route) gets `403`. See §5 for the `unauthorized`
(401) / `forbidden` (403) error codes.

**Route table (target, D7 for the read model):**

| Routes | Auth |
|---|---|
| `GET /.well-known/oauth-protected-resource`, `GET /.well-known/oauth-authorization-server`, `GET\|POST /authorize`, `POST /token`, `POST /revoke`, `POST /register`, `POST /v1/invites/redeem`, `GET /v1/invites/requests/{id}` | Public — no bearer token required (these routes *are* the auth flow, or are the deliberately open invite-redemption/status-check surface). |
| Everything else under `/v1/*` and `/mcp` | Bearer token required once auth is enforced (§3). Results are filtered by the principal's store access — admins see all stores; members see only `shared` stores they hold a grant for (D7, [02-domain-model.md](02-domain-model.md) §2). |
| User management, grant management, invite management (once they exist as routes) | Bearer token **and** `role = admin` (`Principal::require_admin`). |

**One-time setup code:** when `localdb serve` starts with auth enforced and no users exist yet
(`AuthStore::count_users() == 0`), the daemon prints a one-time setup code to stdout/stderr so
the operator can bootstrap the first admin user without any prior credential.

**Token model:** opaque 32-byte secrets minted from `OsRng`, `ldb_`-prefixed, shown once at
issuance and stored at rest only as a blake3 hash — never a password anywhere. Access tokens are
short-lived (1h); refresh tokens (30d) rotate on every use with reuse detection — presenting an
already-rotated refresh token revokes its entire token family. API keys share the same
`auth_tokens` table (`kind = 'api_key'`), have no default expiry, and track `last_used_at`. OAuth2
authorization-code + PKCE (S256) is the flow behind `/authorize` and `/token`, once implemented.

## 4. MCP

**Decision:** v1 MCP is **read-only**: tools `search` (args: query, optional store names, limit, optional content_length →
Citation list as structured content; each citation carries full `Metadata`),
`get_document` (id or uri → block texts + `metadata: Metadata`),
`get_chunks` (document_id, optional offset/limit → the document's chunks in order, paginated),
`list_stores` (names, visibility, counts). **Mutating tools** (`add_source`, `reindex`, …) are a
follow-up behind an explicit opt-in flag (`localdb mcp --allow-write`), never on by default.

Once auth is enforced (§3.1), tool results on any authenticated transport are filtered by the
principal's store access — admins see every store, members only the `shared` stores they hold a
grant for (D7). The embedded stdio MCP (§4.2, no daemon running) stays unauthenticated: it is
already trusted as local-files-equivalent, the same trust boundary as every other daemonless
command.

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
- **HTTP** (`/mcp`, mounted on the daemon alongside its own `/v1` routes): built from the
  daemon's store set at its own startup (see `mcp::http::build_streamable_http_service`'s doc
  comment). HTTP MCP sessions always run with `allow_write = false`.

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
  `search`'s `query`, `get_chunks`'s `document_id`) fails `rmcp`'s `Parameters<T>`
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
| `store_not_found` / `source_not_found` / `resource_not_found` / `job_not_found` | Unknown entity | 404 |
| `runtime_state_locked` | Unified database locked by another process (busy timeout exceeded) | 409 |
| `daemon_running` / `daemon_unreachable` | Process-model conflicts | 409 / 502 |
| `invalid_config` | Config failed validation (path-precise message) | 422 |
| `invalid_request` | Bad arguments/body | 400 |
| `unauthorized` | Missing or invalid bearer token (§3.1) | 401 |
| `forbidden` | Authenticated, but insufficient permission for the resource (§3.1, D7) | 403 |
| `unsupported_format` | Extraction can't handle the file type (informational in job stats) | 422 |
| `extraction_failed` | Recognized, supported format whose contents could not be extracted (corrupt/truncated). Counted in `error_count` in job stats; produces a WARN per file. | 422 |
| `provider_unavailable` | External embedding endpoint down/misconfigured | 502 |
| `model_missing` | Local model not yet downloaded; message includes the fix | 503 |
| `index_in_progress` | Conflicting job already running for the scope | 409 |
| `internal` | Bug; includes correlation id, logged with backtrace | 500 |

CLI exit codes: `0` ok, `1` internal, `2` invalid usage/config, `3` not found, `4` conflict/locked,
`5` unavailable (daemon/provider/model), `6` permission denied (auth required/insufficient).

### `localdb index --strict`

By default `index` is **best-effort**: unsupported files are silently counted; extraction failures
produce a per-file WARN but the run continues and exits `0`. Pass `--strict` to exit `2` when any
resource failed (`error_count > 0`). The run always completes — `--strict` never aborts mid-run;
it only affects the final exit code and JSON `"status"` field.
