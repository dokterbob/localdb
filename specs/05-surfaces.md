# Spec 05 — Surfaces: CLI, HTTP API, MCP

> Status: accepted draft, revised 2026-07-08 (T6: invite create/redeem/approve). All three
> surfaces sit on the same `core` ([01-architecture.md](01-architecture.md) §1) and return the
> same Citation shape ([02-domain-model.md](02-domain-model.md) §6) and error taxonomy (§5).

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
| `login [--invite <token>] [--name <name>]` / `logout` | Authenticate against a daemon and cache the resulting token, or revoke and clear it. `login` without `--invite` drives the OAuth2 authorization-code + PKCE browser flow (T4); `login --invite <token>` (T6) redeems the invite directly against `POST /v1/invites/redeem` instead — no browser round trip. `--name` sets the requested user name (default: the OS login name, `$USER`/`%USERNAME%`) | n/a — there is no auth to log into without a daemon (§3.1) | `login` (no `--invite`) exchanges credentials via `/token` and writes `credentials.json` (§6, [03-config.md](03-config.md) §6); `login --invite`: `open`-mode invites persist the redeemed API key immediately; `closed`-mode invites print a "waiting for admin approval" message and poll `GET /v1/invites/requests/{id}` (1s interval) until an admin approves (credential persisted) or denies (clear error, non-zero exit); `logout` calls `/revoke` and clears the cached entry |
| `user add\|list\|remove\|set-role` | Manage user accounts (admin only) | direct write/read against the unified DB's `users` table — the recovery path when the daemon is unreachable or auth is locked out | routed to daemon, requires an admin bearer token; `user add` also accepts a `--direct-db` flag to bypass a running daemon for lockout recovery |
| `key create\|list\|revoke` | Manage API keys (`auth_tokens` rows with `kind='api_key'`) for the caller or, as admin, another user | direct write/read | routed to daemon; managing another user's key requires admin (minting your *own* key is always allowed); `key create` also accepts `--direct-db` |
| `store grant\|revoke` | Grant/revoke a member's read access to a `shared` store (D7); rejected on `private` stores | direct write | routed to daemon, admin only |
| `invite create\|list\|revoke\|requests\|approve\|deny` | Manage invites and pending access requests (admin only, T6) — §3.1's invite route table | direct write | routed to daemon, admin only |
| `db status` | Inspect schema state: current version, head version, pending/unsupported steps. Never refuses, even on a store newer than the binary | reads directly | error `daemon_running` |
| `db migrate` | Apply pending migrations with per-step progress; legacy v1–v3 rebuild and any other destructive step require confirmation; prints a `localdb index` hint when a weight-class-3 migration ran | direct write | error `daemon_running` |
| `db downgrade [--to N]` | Reverse migrations down to version `N` (default: one step) using stored down-SQL; requires confirmation; refuses cleanly on a step with `down_unsupported_reason` | direct write | error `daemon_running` |

Output: human-readable by default (citations as `uri:heading_path` + snippet), `--json` emits the
canonical structures for scripting. The CLI is **command-oriented**; interactive browse is a
roadmap item with the web UI.

**Daemon-routed-first, direct-DB fallback (T5):** `user`, `key`, and `store grant/revoke` follow
the same pattern as `store add/list/remove` (§2 above): when a daemon is reachable, the request
goes over HTTP with the caller's bearer — admin where the route requires it
([§3.1](#31-authentication)'s route table — a non-admin bearer gets `forbidden`/exit 6 from the
daemon, not a CLI-side refusal); otherwise it falls back to a direct, trusted database
read/write, carrying the same trust assumption as every other daemonless CLI command — whoever
can open the database file is already trusted. This makes "add a user while the server is up"
just work over HTTP, the common case, rather than requiring the daemon to be stopped first.

**Break-glass escape hatch:** `user add --direct-db` (and `key create --direct-db`) is the one
deliberate exception — it forces the direct-DB write even while a daemon is running, for the
lockout-recovery case where the daemon's own auth is broken or every admin credential has been
lost. It warns (non-JSON mode) that a daemon is running rather than refusing: SQLite's
write-ahead log plus busy-timeout already make concurrent access with a live daemon safe (the
same `runtime_state_locked`/exit 4 outcome as any other contended direct-DB write in the worst
case), so refusing outright would only get in the way of the one scenario this flag exists for.
`invite` (T6) follows the same daemon-routed-first, direct-DB-fallback pattern as `user`/`key`
(no `--direct-db` escape hatch, though — invites aren't a lockout-recovery primitive, so there is
no reason to force past a running daemon). `invite create` prints the show-once plaintext token
plus a ready-made consent URL (`{base}/authorize?invite=<token>`, built from the request's own
`Host` header in the daemon-routed case, or a placeholder pointing at "start a daemon" in the
direct-DB case, since there is no base URL to build one from without a running daemon).

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

**Status:** T1 shipped the foundation (`core::auth` types, `AuthStore` trait, `AuthService`, the
`store-libsql` persistence layer and schema — [02-domain-model.md](02-domain-model.md) §2, §9).
T2–T4 wired bearer-token enforcement, the `require_auth` middleware, and the OAuth2
authorization-code + PKCE flow. **T5 lifts the interim "every member gets 403 everywhere" gate**
(a fail-safe staging measure T3 shipped before store grants existed) and activates the target
shape described below: per-resource D7 scoping instead of a wholesale role gate, plus the
user/key/grant management routes themselves. **T6 lands invite management**: the
create/list/revoke/redeem/approve/deny/poll state machine (`core::auth::AuthService`) and its
HTTP routes (last two rows below).

**Decision:** bearer tokens (`Authorization: Bearer ldb_...`) on every route under `/v1/*` and
`/mcp`, once auth is enforced for the daemon (§3). A request with a missing or invalid token gets
`401` with a `WWW-Authenticate: Bearer` header. A request from an authenticated principal who
lacks permission for the resource (e.g. a member reading a store they have no grant for, or any
non-admin route) gets `403`. See §5 for the `unauthorized` (401) / `forbidden` (403) error codes.

**Route table:**

| Routes | Auth |
|---|---|
| `GET /.well-known/oauth-protected-resource`, `GET /.well-known/oauth-authorization-server`, `GET\|POST /authorize`, `POST /token`, `POST /revoke`, `POST /register`, `POST /v1/invites/redeem`, `GET /v1/invites/requests/{id}` | Public — no bearer token required (these routes *are* the auth flow, or are the deliberately open invite-redemption/status-check surface). |
| `GET /v1/stores`, `GET /v1/stores/{name}`, `GET /v1/stores/{name}/sources`, `POST /v1/search`, `GET /v1/documents/{id}`, `GET /v1/auth/me`, `/mcp` | Bearer token required once auth is enforced (§3). Results/visibility are scoped by the principal's store access (D7, [02-domain-model.md](02-domain-model.md) §2): admins see every store; members see only `shared` stores they hold a grant for. A named-but-unreadable store (a direct `GET`, or an explicit `store_filter`/MCP `stores` entry naming one) is **403, not 404** — consistent with the filtered list views, and chosen over 404 so a caller can't use the distinction to fish for private store names; see `handlers::stores::get_store`'s doc comment. |
| `POST/PATCH/DELETE /v1/stores`, `POST /v1/stores/{name}/sources`, `DELETE /v1/sources/{id}`, `POST /v1/jobs`, `GET /v1/jobs/{id}`, `GET /v1/config` | Bearer token **and** `role = admin`. Members are readers only in this phase — every mutation, plus `GET /v1/config` (server configuration, not store-scoped content), is admin-only regardless of any store grant. |
| `GET/POST /v1/users`, `PATCH/DELETE /v1/users/{id}`, `GET /v1/users/{id}/keys`, `DELETE /v1/keys/{id}`, `GET/POST /v1/stores/{name}/grants`, `DELETE /v1/stores/{name}/grants/{user}` | Bearer token **and** `role = admin` (`Principal::require_admin`), with one carve-out: `POST /v1/users/{id}/keys` also allows a non-admin principal to mint a key **for themselves** (`id` equal to the caller's own `user_id`) — every other combination (another user's keys, any user/grant list or mutation) is admin-only. |
| `GET/POST /v1/invites`, `DELETE /v1/invites/{id}`, `GET /v1/invites/requests`, `POST /v1/invites/requests/{id}/approve`, `POST /v1/invites/requests/{id}/deny` (T6) | Bearer token **and** `role = admin`. |

**Guard rails (D7 lockout prevention):** deleting or demoting (`role = admin` → `member`) the
*last remaining admin* is rejected — `AuthService::delete_user`/`set_user_role` check this
generically (would the action leave zero admins?), not just for self-targeted calls, so it also
catches e.g. an admin demoting the only other admin down to zero. Mapped to `invalid_request`
(400 / CLI exit 2) — the same code already used for other "well-formed request, not allowed given
current state" cases (e.g. a duplicate user name) — rather than inventing a new taxonomy entry
for it (§5's codes are stable API). Deleting a user cascades to their tokens and store grants via
`ON DELETE CASCADE` at the schema level ([02-domain-model.md](02-domain-model.md) §9).

**One-time setup code:** when `localdb serve` starts with auth enforced and no users exist yet
(`AuthStore::count_users() == 0`), the daemon prints a one-time setup code to stdout/stderr so
the operator can bootstrap the first admin user without any prior credential.

**Token model:** opaque 32-byte secrets minted from `OsRng`, `ldb_`-prefixed, shown once at
issuance and stored at rest only as a blake3 hash — never a password anywhere. Access tokens are
short-lived (1h); refresh tokens (30d) rotate on every use with reuse detection — presenting an
already-rotated refresh token revokes its entire token family. API keys share the same
`auth_tokens` table (`kind = 'api_key'`), have no default expiry, and track `last_used_at`. OAuth2
authorization-code + PKCE (S256) is the flow behind `/authorize` and `/token`, once implemented.

**Dynamic Client Registration (T7, RFC 7591):** `POST /register` is public/unauthenticated, so it
enforces per-request size caps on registration metadata — at most 5 `redirect_uris`, each at most
2048 characters, and an optional `client_name` at most 256 characters (`core::auth::client`'s
`MAX_REGISTRATION_*` constants) — rejected as `400 invalid_client_metadata`/`invalid_redirect_uri`
(RFC 7591 §3.2.2). These bound a single request's payload, not registration frequency; a global
registration-count cap or rate limit is a separate, not-yet-implemented concern.

### 3.1.1 Invites (T6, D9)

An invite ([02-domain-model.md](02-domain-model.md) §2) carries a `mode` (`open` | `closed`),
optional `store_grants` (rejected at CREATE time if any named store is `private`), `max_uses`
(default 1), and an optional absolute-RFC-3339 `expires_at`. The state machine lives entirely in
`core::auth::AuthService` (no domain logic in `server`/`cli`, per
[01-architecture.md](01-architecture.md) §1):
`create_invite`/`redeem_invite`/`approve_request`/`deny_request`/`poll_request`.

**`POST /v1/invites`** (admin) — body `{"mode": "open"|"closed", "stores": ["name", ...],
"max_uses": 1, "expires_at": "<RFC3339>"|null}` → `201`:

```json
{
  "id": "...", "mode": "open", "store_grants": ["docs"], "max_uses": 1,
  "expires_at": null, "created_at": "...",
  "token": "ldb_...", "consent_url": "http://<host>/authorize?invite=ldb_..."
}
```

`token` is the show-once plaintext invite secret (blake3-hashed at rest, D1); `consent_url` is
built from the request's own `Host` header (correct for any bind address/port with no extra
`AppState` plumbing) and is a ready-made link to the T4 consent page's invite-redemption variant
(§below). `GET /v1/invites` (admin) lists every invite with no secrets; `DELETE /v1/invites/{id}`
(admin) revokes one.

**`POST /v1/invites/redeem`** (public — no bearer, D9 device-authorization-grant pattern): body
`{"token": "<invite secret>", "name": "<requested user name>"}`.

- `open` mode → `201`: `{"user": {"id","name","role","created_at"}, "granted_stores": [...],
  "api_key": "ldb_..."}` — the user, its grants, and a show-once API key, all created immediately.
- `closed` mode → `202`: `{"request_id": "...", "request_secret": "ldb_...", "poll":
  "/v1/invites/requests/{id}?secret=..."}` — a pending `AccessRequest` is filed; `request_secret`
  is shown once here and doubles as the poll credential below.
- Unknown/revoked/expired/exhausted invite → `401 unauthorized` (mirrors `redeem_auth_code`'s
  "don't leak which check failed" convention for this public route); a requested name colliding
  with an existing user → the existing `400 invalid_request` duplicate-name shape.

**`GET /v1/invites/requests/{id}?secret=<request_secret>`** (public, query-param secret — chosen
over a header for `curl`-ability and to match the `poll` hint above): `200` with
`{"state": "pending"}` / `{"state": "denied"}` / `{"state": "approved", "api_key": "ldb_..."}` /
`{"state": "collected"}`. An unknown `id` and a wrong `secret` against a real one are
**deliberately indistinguishable** (`401 unauthorized`, identical body) — no existence oracle.
A **freshly minted** API key is handed back **exactly once**, on the poll that first observes the
`approved` transition (`AccessRequest.collected_at`, an atomic single-use guard mirroring
`consume_auth_code`) — every later poll answers `collected` instead of re-issuing a credential.
`request_secret` itself never becomes a credential: it is poll-only, scoped to proving knowledge
of this one request, and is deliberately never accepted by `AuthService::authenticate` — minting a
fresh key at collection time (rather than promoting the request secret, which travels as a URL
query parameter on every poll and would otherwise become a long-lived credential live from
approval time even if never collected) is what closes that hole. See
`AuthService::poll_request`'s doc comment for the full reasoning.

**Admin decision routes:** `GET /v1/invites/requests` (admin) lists every access request across
every invite (pending, approved, and denied alike; no pagination — this is a small admin-facing
surface). `POST /v1/invites/requests/{id}/approve` (admin) creates the user + grants (no
credential is minted here — that happens at the requester's next poll) and returns the new user.
`POST /v1/invites/requests/{id}/deny` (admin) marks it
denied; the requester's next poll observes `denied`.

**Concurrency (documented choice):** `redeem_invite` atomically *reserves* a use
(`AuthStore::try_consume_invite_use`, an `UPDATE ... WHERE uses < max_uses` conditional update)
before creating the user / filing the access request, and *releases* the reservation
(`AuthStore::release_invite_use`) if that mint then fails. This closes both hazards: concurrent
redemptions — even under distinct requested names — can never together push `uses` past
`max_uses` (the atomic reservation is the gate, not the `users.name` UNIQUE constraint), and a
redemption that reserves a use but then fails (most commonly a duplicate `requested_name` racing
`create_user`'s UNIQUE constraint) never permanently burns that use — a caller can retry with a
different name against the same `max_uses = 1` invite. See `AuthService::redeem_invite`'s doc
comment for the full reasoning.

**Consent page (T4 seam):** `GET /authorize?invite=<token>` renders an invite-redemption variant
of the consent form (a "your name" field instead of the setup-code/API-key credential field).
Submitting it (`server::auth::oauth::handle_invite_authorize`) redeems the invite: `open` mode
continues the OAuth2 flow as the newly created user (issuing an authorization code exactly like
the credential-based path), so `localdb login --invite <token>`'s browser fallback, if used, still
ends in ordinary browser-session tokens; `closed` mode renders a static "request submitted, ask
your admin" page and issues no code (no admin has approved yet, so there is no user to issue one
for). The CLI's own `login --invite` (§2) does **not** go through this page — it redeems and, for
`closed` mode, polls directly against the JSON routes above, since only that path can drive a
poll loop; the consent-page branch exists for the pure-browser case.

## 4. MCP

**Decision:** v1 MCP is **read-only**: tools `search` (args: query, optional store names, limit, optional content_length →
Citation list as structured content; each citation carries full `Metadata`),
`get_document` (id or uri → block texts + `metadata: Metadata`),
`get_chunks` (resource_id, optional offset/limit, or optional anchor_chunk_id/anchor_block_seq
(§4.1) → the resource's chunks in order, paginated),
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
