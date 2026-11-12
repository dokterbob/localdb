# HTTP API (`localdb serve`)

> **EXPERIMENTAL — do not rely on this surface for production use.**
>
> The daemon opens the same unified database (`<data_dir>/localdb.db`) as the CLI, so CLI-indexed
> data IS visible via `/v1/search`, `/v1/documents/{id}`, `/v1/stores/{name}/documents`, and
> `/v1/status`. Ingestion via `POST /v1/jobs` runs the real pipeline through an async job queue
> ([#187](https://github.com/dokterbob/localdb/issues/187)) — `localdb index` submits a job and
> attaches to its live progress (`GET /v1/jobs/{id}/events`, SSE) whenever a daemon is running, with
> identical output to embedded mode; you no longer need to stop the daemon first. It remains
> experimental as a surface: write concurrency across processes is SQLite WAL + `busy_timeout=5000`,
> not a dedicated lock.
>
> For design rationale see
> [specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md) §3.

---

## Starting the daemon

```
localdb serve
```

On startup the daemon prints a single announce line to stdout and then continues running:

```
daemon listening on http://127.0.0.1:7700
```

It binds the HTTP listener and also creates a Unix discovery socket at `<data_dir>/daemon.sock` so
that CLI and MCP processes can detect it, plus a `<data_dir>/daemon.url` file recording the daemon's
actual client-reachable base URL (e.g. `http://192.168.1.5:7700` for a LAN bind, or
`http://127.0.0.1:7700` when bound to `0.0.0.0`/`::`, since the wildcard address itself isn't
connectable). CLI/MCP discovery reads this file, so it works for any configured bind address or port
— not just the default `127.0.0.1:7700`.

### Bind address and port

The bind address and port are controlled by the `server` block in `config.yaml`:

```yaml
version: 1
server:
  bind: 127.0.0.1 # default; any bind address is accepted (see Trust model below)
  port: 7700 # default; 0 = OS-assigned
```

Setting `port: 0` asks the OS for an ephemeral port. The assigned port is shown in the announce
line.

### Trust model and authentication

The daemon binds `127.0.0.1` by default. Whether requests need a bearer token is controlled by
`server.auth` in `config.yaml`:

| `server.auth`    | Loopback bind                       | Non-loopback bind (LAN, Tailscale, `0.0.0.0`, …)                                                                    |
| ---------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `auto` (default) | **Open** — no bearer token required | **Enforced** — every protected route requires `Authorization: Bearer ldb_...`                                       |
| `required`       | Enforced                            | Enforced                                                                                                            |
| `off`            | Open                                | **Hard error at startup** (`invalid_config`) — the daemon refuses to expose an unauthenticated surface to a network |

Binding to a specific non-loopback address is a deliberate trust decision and is accepted (auth
enforced automatically under the default `auto`). Binding to `0.0.0.0`/`::` (all interfaces)
additionally logs a startup warning, since that's reachable from every network the machine is on.
See [specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md) §3
for the full binding and auth-mode decision matrix.

**When auth is Open:** every request runs as an implicit admin principal
(`Principal::local_trust()`) — the same trust boundary as the CLI and embedded MCP: anything that
can reach the bind address is as trusted as the files themselves.

**When auth is Enforced:** a request with a missing or invalid bearer token gets `401` with
`WWW-Authenticate: Bearer` (plus, since the discovery routes below exist, a
`resource_metadata="<base>/.well-known/oauth-protected-resource"` parameter — see
[OAuth discovery](#oauth-discovery--dynamic-client-registration) below). A request from an
authenticated principal who lacks permission for the resource gets `403`. Members (as opposed to
admins) only see/search `shared`-visibility stores they hold an explicit grant for
(`localdb store grant`) — see
[specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md) §3.1 for
the D7 authorization model.

**Setup and recovery.** When authentication is enforced and no admin exists, or first-admin setup is
incomplete, `localdb serve` prints a setup-code block to stdout right after the announce line:

```
daemon listening on http://127.0.0.1:7700

One-time setup code: ldb_...
Run: localdb login --url http://127.0.0.1:7700 --setup-code ldb_...
This code works once and changes on restart.
```

`serve --json`'s `listening` object carries the same code as a `setup_code` field, present only
while setup is outstanding:

```json
{ "status": "listening", "url": "http://127.0.0.1:7700", "setup_code": "ldb_..." }
```

Paste it into the browser consent page (or use `localdb login --setup-code <code>`). Interrupted
authorization can be retried with the same code and reuses the pending admin. The first
authenticated protected request completes setup and disables the code. Restarting incomplete setup
prints a replacement code; an established admin never reopens setup merely because credentials were
lost. Existing `--direct-db` recovery remains available to operators.

A `POST /v1/invites` response's `consent_url` points at `GET /join?invite=<token>`, a standalone
page for a browser invitee with no OAuth client and no CLI — it ends in a show-once API key (`open`
mode) or a waiting page (`closed` mode). Closed-invite onboarding, from either `/join` or the OAuth
consent page (`GET /authorize?invite=<token>`), waits for approval, collects the credential, and
either displays it (`/join`) or resumes the original OAuth flow (`/authorize`). It requires
JavaScript; no CLI operation is needed by the invited user. OAuth and invite responses use
`Cache-Control: no-store` and `Referrer-Policy: no-referrer`.

**Route table:**

| Routes                                                                                                                                                                                                                                                   | Auth                                                                                                                                                                     |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `GET /.well-known/oauth-protected-resource`, `GET /.well-known/oauth-authorization-server`, `GET\|POST /authorize`, `GET\|POST /join`, `POST /token`, `POST /revoke`, `POST /register`, `POST /v1/invites/redeem`, `POST /v1/invites/requests/{id}/poll` | **Public** — no bearer token, even when auth is enforced. These routes _are_ the auth flow (OAuth discovery, DCR, and the code+PKCE/invite-redemption flows themselves). |
| `GET /v1/stores`, `GET /v1/stores/{name}`, `GET /v1/stores/{name}/sources`, `GET /v1/stores/{name}/documents`, `GET /v1/status`, `POST /v1/search`, `GET /v1/documents/{id}`, `GET /v1/auth/me`, `/mcp`                                                  | Bearer token required once enforced. Results are scoped by the principal's store access (admins: everything; members: granted `shared` stores only).                     |
| `POST/PATCH/DELETE /v1/stores`, `POST /v1/stores/{name}/sources`, `DELETE /v1/sources/{id}`, `POST/GET /v1/jobs`, `GET /v1/jobs/{id}`, `GET /v1/config`                                                                                                  | Bearer token **and** `role = admin`.                                                                                                                                     |
| `GET/POST /v1/users`, `PATCH/DELETE /v1/users/{id}`, `GET /v1/users/{id}/keys`, `DELETE /v1/keys/{id}`, `GET/POST /v1/stores/{name}/grants`, `DELETE /v1/stores/{name}/grants/{user}`                                                                    | Bearer token **and** `role = admin` — except `POST /v1/users/{id}/keys` also allows a non-admin principal to mint a key for themselves.                                  |
| `GET/POST /v1/invites`, `DELETE /v1/invites/{id}`, `GET /v1/invites/requests`, `POST /v1/invites/requests/{id}/approve\|deny`                                                                                                                            | Bearer token **and** `role = admin`.                                                                                                                                     |

Job listing, cancellation, status, and progress streams (`/v1/jobs` and its subroutes) require an
admin. Members can read only granted shared stores through all retrieval routes, including document
listing. Their `/v1/status` response omits shared database diagnostics and counts only readable
stores and their jobs.

### OAuth discovery + Dynamic Client Registration

Once auth is enforced, three RFCs work together so a stock MCP client can onboard against `/mcp` (or
any bearer-protected route) with **zero static configuration** — no pre-shared token, no
manually-typed client ID:

1. The client calls a protected route (e.g. `POST /mcp`) with no credential and gets `401` with
   `WWW-Authenticate: Bearer resource_metadata="<base>/.well-known/oauth-protected-resource"`.
2. It follows that URL to **RFC 9728** protected-resource metadata:

   ```json
   {
     "resource": "http://127.0.0.1:7700",
     "authorization_servers": ["http://127.0.0.1:7700"],
     "bearer_methods_supported": ["header"]
   }
   ```

3. It follows `authorization_servers[0]` to **RFC 8414** authorization-server metadata at
   `GET /.well-known/oauth-authorization-server`:

   ```json
   {
     "issuer": "http://127.0.0.1:7700",
     "authorization_endpoint": "http://127.0.0.1:7700/authorize",
     "token_endpoint": "http://127.0.0.1:7700/token",
     "revocation_endpoint": "http://127.0.0.1:7700/revoke",
     "registration_endpoint": "http://127.0.0.1:7700/register",
     "response_types_supported": ["code"],
     "grant_types_supported": ["authorization_code", "refresh_token"],
     "code_challenge_methods_supported": ["S256"],
     "token_endpoint_auth_methods_supported": ["none"],
     "revocation_endpoint_auth_methods_supported": ["none"]
   }
   ```

4. It registers itself against `registration_endpoint` (**RFC 7591**, `POST /register`):

   ```
   curl -s -X POST http://127.0.0.1:7700/register \
     -H 'Content-Type: application/json' \
     -d '{"redirect_uris": ["http://127.0.0.1:54321/callback"], "client_name": "My MCP Client"}'
   ```

   ```json
   {
     "client_id": "01K...",
     "client_name": "My MCP Client",
     "redirect_uris": ["http://127.0.0.1:54321/callback"],
     "grant_types": ["authorization_code", "refresh_token"],
     "response_types": ["code"],
     "token_endpoint_auth_method": "none"
   }
   ```

   Every `redirect_uris` entry must be either an `https://` URL or a loopback
   `http://127.0.0.1[:port]/...` / `http://localhost[:port]/...` URL — custom URI schemes
   (`myapp://callback`) are rejected. `token_endpoint_auth_method`, if sent, must be `"none"`: this
   endpoint only ever registers **public** clients, so no `client_secret` is ever minted or
   returned. Unlike the built-in `localdb-cli` client (which gets an RFC 8252 §7.3
   loopback-_any-port_ exception, since the CLI binds a fresh ephemeral port per login), a
   registered client's redirect_uri is matched **exactly** against what it registered — no loopback
   leniency.

5. It runs the ordinary code+PKCE flow (`GET/POST /authorize` → `POST /token`) using the `client_id`
   from step 4.

**Base URL resolution.** `<base>` in every URL above is `server.public_url` when configured (trimmed
of a trailing slash), or otherwise derived from the request's own `Host` header. Set
`server.public_url` whenever the daemon sits behind a TLS-terminating reverse proxy for remote use —
it becomes the canonical issuer/resource identifier and the daemon itself never needs to know it's
behind TLS:

```yaml
server:
  public_url: https://localdb.example.com # only set behind a TLS-terminating reverse proxy
```

Without `public_url`, the `Host` header is attacker-influencable (these are, by design, some of the
few unauthenticated routes) — a malformed or hostile header (embedded path, scheme, userinfo, or
control characters) is rejected with `400 invalid_request` rather than ever echoed back, and the
discovery routes need _some_ valid header to respond at all in that case.

**Plain-HTTP LAN risk.** If you bind the daemon to a LAN/Tailscale address _without_ a
TLS-terminating reverse proxy (i.e. without `public_url`, talking plain `http://`), bearer tokens
travel in cleartext on that network segment — anyone who can observe the traffic (a compromised
device on the same LAN, a malicious access point) can capture and replay a token until it's revoked
or expires. This is the same risk as any bearer-token API served over plain HTTP; the mitigations
are the usual ones — trust the network segment (Tailscale's own WireGuard tunnel already encrypts
the transport, which is why the Tailscale case above is lower-risk in practice), or put a
TLS-terminating reverse proxy in front (and set `server.public_url` to match) for any bind that
leaves your own machine.

---

## MCP over HTTP

Alongside `/v1`, the daemon also mounts `/mcp` — the same five read-only MCP tools (`search`,
`get_document`, `get_chunks`, `list_stores`, `list_documents`) served over the
[MCP Streamable HTTP transport](https://modelcontextprotocol.io/), for connecting a remote MCP
client (e.g. Claude Code on another machine, over Tailscale/LAN). It inherits this daemon's
bind-address trust decision automatically — see
[docs/mcp.md](mcp.md#remote-http-connecting-from-another-machine) for setup and
[specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md) §4.2 for
the transport/error-model details.

---

## Endpoint reference

All endpoints are under the `/v1` prefix. Request and response bodies are JSON; set
`Content-Type: application/json` on requests that carry a body.

### `GET /v1/status`

Returns a brief daemon health summary.

```
curl -s http://127.0.0.1:7700/v1/status
```

```json
{
  "daemon": true,
  "store_count": 1,
  "source_count": 0,
  "job_count": 0,
  "features": ["search_filters"],
  "stores": [
    {
      "name": "notes",
      "visibility": "private",
      "backend": "libsql",
      "document_count": 3,
      "chunk_count": 30
    }
  ],
  "database": {
    "path": "/path/to/data/localdb.db",
    "exists": true,
    "size_bytes": 90112,
    "wal_size_bytes": 0,
    "total_size_bytes": 90112,
    "bytes_per_chunk": 3003,
    "largest_tables": [{ "name": "chunks", "bytes": 65536 }]
  }
}
```

| Field                                              | Type      | Description                                                                                                                                                                                                                                                           |
| -------------------------------------------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `daemon`                                           | bool      | Always `true` when the daemon is responding                                                                                                                                                                                                                           |
| `store_count`                                      | int       | Number of stores known to this daemon instance                                                                                                                                                                                                                        |
| `source_count`                                     | int       | Total sources across all stores                                                                                                                                                                                                                                       |
| `job_count`                                        | int       | Number of jobs ever created in this daemon session                                                                                                                                                                                                                    |
| `stores[].document_count` / `stores[].chunk_count` | int\|null | Per-store `RetrievalStore::stats()` figures; `null` if that store's stats call itself failed (a corrupt or mid-migration store must not blank out the report on the others)                                                                                           |
| `database.path`                                    | string    | Path to the shared `localdb.db` file — one physical file backs every store, so this is reported once, not per-store                                                                                                                                                   |
| `database.exists`                                  | bool      | Whether the file exists yet (`false` before the first `store add`/`index`)                                                                                                                                                                                            |
| `database.size_bytes` / `database.wal_size_bytes`  | int\|null | Bytes in the main file / `-wal` sidecar; `null` if a stat fails                                                                                                                                                                                                       |
| `features`                                         | string[]  | Capabilities this daemon supports, so a newer client can tell whether an older running daemon will honour a request before sending it. Currently `["search_filters"]`. Treat an absent or unknown name as unsupported — daemons predating this field omit it entirely |
| `database.total_size_bytes`                        | int       | `size_bytes + wal_size_bytes` (missing components treated as 0) — what the disk actually has allocated right now                                                                                                                                                      |
| `database.bytes_per_chunk`                         | int\|null | `total_size_bytes` divided by the sum of every store's `chunk_count`; `null` with no chunks                                                                                                                                                                           |
| `database.largest_tables`                          | array     | Up to 5 `{name, bytes}` rows, the largest on-disk tables via SQLite's `dbstat`, descending; best-effort — empty if `dbstat` querying fails                                                                                                                            |

This is the same shape the embedded CLI's `localdb status --json` reports (see
[specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md) §2.4) —
daemon-routed and embedded `status` render identically.

---

### `GET /v1/stores`

List all stores. Response is paginated (see [Pagination](#pagination)).

```
curl -s http://127.0.0.1:7700/v1/stores
```

```json
{
  "items": [
    {
      "name": "notes",
      "id": "01KTVGQ62TQN8X6XN9E5FDZN67",
      "visibility": "private",
      "backend": "libsql"
    }
  ],
  "next_cursor": null,
  "total": 1
}
```

---

### `POST /v1/stores`

Create a runtime-owned store. The DB is the single source of truth for stores — there is no YAML
store declaration (see [`GET /v1/config`](#get-v1config)).

**Request body:**

| Field        | Type   | Required | Description                                         |
| ------------ | ------ | -------- | --------------------------------------------------- |
| `name`       | string | yes      | Store name; must be non-empty and not already exist |
| `visibility` | string | no       | `"private"` (default) or `"shared"`                 |

```
curl -s -X POST http://127.0.0.1:7700/v1/stores \
  -H 'Content-Type: application/json' \
  -d '{"name":"notes"}'
```

```json
{
  "name": "notes",
  "id": "01KTVGQ62TQN8X6XN9E5FDZN67",
  "visibility": "private",
  "backend": "libsql"
}
```

Returns `201` on success. `invalid_request`, 400, for an empty `name`, a `name` that already exists,
or an unrecognized `visibility` value (see [Error responses](#error-responses)).

---

### `GET /v1/stores/{name}`

Fetch a single store by name.

```
curl -s http://127.0.0.1:7700/v1/stores/notes
```

```json
{
  "name": "notes",
  "id": "01KTVGQ62TQN8X6XN9E5FDZN67",
  "visibility": "private",
  "backend": "libsql"
}
```

Returns `404` with error code `store_not_found` if the store does not exist (see
[Error responses](#error-responses)).

---

### `PATCH /v1/stores/{name}`

Update a runtime-owned store. All fields are optional — only provided fields are updated. Currently
the only mutable field is `visibility`.

**Request body:**

| Field        | Type   | Required | Description                               |
| ------------ | ------ | -------- | ----------------------------------------- |
| `visibility` | string | no       | New visibility: `"private"` or `"shared"` |

```
curl -s -X PATCH http://127.0.0.1:7700/v1/stores/notes \
  -H 'Content-Type: application/json' \
  -d '{"visibility":"shared"}'
```

```json
{
  "name": "notes",
  "id": "01KTVGQ62TQN8X6XN9E5FDZN67",
  "visibility": "shared",
  "backend": "libsql"
}
```

Returns `404` with error code `store_not_found` if the store does not exist. `invalid_request`, 400,
for an unrecognized `visibility` value.

---

### `DELETE /v1/stores/{name}`

Delete a store, cascading to all its sources, documents, and chunks.

```
curl -s -X DELETE http://127.0.0.1:7700/v1/stores/notes
```

Returns `204 No Content` (empty body) on success. Returns `404` with error code `store_not_found` if
the store does not exist.

---

### `GET /v1/stores/{name}/sources`

List sources attached to a store. Response is paginated.

```
curl -s http://127.0.0.1:7700/v1/stores/notes/sources
```

```json
{
  "items": [],
  "next_cursor": null,
  "total": 0
}
```

---

### `POST /v1/stores/{name}/sources`

Add a source to a store.

**Request body:**

| Field     | Type   | Required | Description                                                                                                                                                                          |
| --------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `kind`    | string | yes      | `"path"`, `"url"`, or `"feed"`                                                                                                                                                       |
| `spec`    | object | yes      | Kind-specific spec — e.g. `{"root": "..."}` for `path` (see [specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md) §2 for `url`/`feed`) |
| `preset`  | string | no       | Chunking preset (default: `"prose"`)                                                                                                                                                 |
| `refresh` | string | no       | Refresh interval (e.g. `"24h"`); persisted for `url`/`feed` sources; rejected with `invalid_request` (400) for any other kind                                                        |

```
curl -s -X POST http://127.0.0.1:7700/v1/stores/notes/sources \
  -H 'Content-Type: application/json' \
  -d '{"kind":"path","spec":{"root":"/home/user/docs"}}'
```

```json
{
  "id": "01KTVH6AY4DC84HWW7M2PP4F0X",
  "store_id": "01KTVGQ62TQN8X6XN9E5FDZN67",
  "kind": "path",
  "spec": { "root": "/home/user/docs", "include": [], "exclude": [] },
  "preset": "prose",
  "refresh": null
}
```

Returns `201` on success. `404` with error code `store_not_found` if the store does not exist.
`invalid_request`, 400, for an unrecognized `kind` or a `spec` that fails kind-specific validation
(e.g. a `path` source's `spec` missing `root`, or a `feed` source's `max_entries: 0`).

---

### `DELETE /v1/sources/{id}`

Remove a source by id. Store-agnostic — the id alone identifies the source, so this route is not
nested under `/v1/stores/{name}`.

```
curl -s -X DELETE http://127.0.0.1:7700/v1/sources/01KTVH6AY4DC84HWW7M2PP4F0X
```

Returns `204 No Content` (empty body) on success. Returns `404` with error code `source_not_found`
if the source does not exist.

---

### `GET /v1/stores/{name}/documents`

List documents registered in a store. Response is paginated (see [Pagination](#pagination)).

**Query parameters:**

| Parameter | Type   | Required | Description                                                                                                        |
| --------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------ |
| `source`  | string | no       | Restrict to documents from this source id. An unknown id is a pure filter — it returns an empty page, not an error |
| `cursor`  | string | no       | Pagination cursor from a previous response                                                                         |
| `limit`   | int    | no       | Maximum items per page (must be ≥ 1; `0` is rejected as `invalid_request`)                                         |

```text
curl -s http://127.0.0.1:7700/v1/stores/notes/documents
```

```json
{
  "items": [
    {
      "store_id": "01KTVGQ62TQN8X6XN9E5FDZN67",
      "id": "a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8",
      "source_id": "01KTVH6AY4DC84HWW7M2PP4F0X",
      "ingestor_kind": "file",
      "uri": "file:///home/user/notes/meeting.txt",
      "title": null,
      "mime": "text/plain",
      "content_hash": "e3732cc41f646a4bc94bc3611b8b6fd9d7f31f1c192748d586f55b8e7e171fd2",
      "fetched_at": "2026-08-17T20:25:09Z",
      "origin_store": "01KTVGQ62TQN8X6XN9E5FDZN67",
      "policy_version": "a739e16768e0b8872b7220d37c37b9c9729d8eee52aa47575401035593411a69",
      "metadata": { "kind": "document", "format": "text/plain", "...": "..." }
    }
  ],
  "next_cursor": null,
  "total": 1
}
```

Returns `404` with error code `store_not_found` if the store does not exist (see
[Error responses](#error-responses)).

---

### `GET /v1/documents/{id}`

Fetch a single document's identity, metadata, and reconstructed full text by id.

**Query parameters:**

| Parameter | Type     | Required | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| --------- | -------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `store`   | string[] | no       | Repeatable — scope the lookup to specific stores by name. Omitted: looks the id up across every store the caller can read (`invalid_request`, 400, if more than one such store holds a document with that id); a miss is a plain 404 even if the id exists in a store the caller cannot read — the lookup never scans unreadable stores to decide between 404 and 403, since that scan would itself leak the document's (and the private store's) existence. Exactly one or more than one named store: resolves as above, and a name the caller cannot read is 403, not 404. Same 0/1/many semantics as CLI `document get -s` (specs/05-surfaces.md §2.2), available in the daemon from day one |

```text
curl -s http://127.0.0.1:7700/v1/documents/a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8
```

```json
{
  "id": "a86bf252232bcec2a7da314d11e4c6005918f7930c7b9e1b081ef528034a34e8",
  "uri": "file:///home/user/notes/meeting.txt",
  "title": null,
  "store_id": "01KTVGQ62TQN8X6XN9E5FDZN67",
  "source_id": "01KTVH6AY4DC84HWW7M2PP4F0X",
  "content_hash": "e3732cc41f646a4bc94bc3611b8b6fd9d7f31f1c192748d586f55b8e7e171fd2",
  "fetched_at": "2026-08-17T20:25:09Z",
  "normalized_text": "Meeting 2026-06-02: decided to adopt reciprocal rank fusion for combining dense and sparse retrieval results.",
  "metadata": { "kind": "document", "format": "text/plain", "...": "..." }
}
```

`normalized_text` is the document's reconstructed full text — always present in the response; there
is no query parameter that omits it (the CLI's `document get --text` is purely a rendering choice on
top of the same always-fetched text, specs/05-surfaces.md §2). `metadata` is the full `Metadata`
enum, same shape as a search citation's `metadata` field
([specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
§7).

Returns `404` with error code `resource_not_found` if no document with that id exists in scope, or
`store_not_found` if a named `?store=` does not exist.

```text
curl -s "http://127.0.0.1:7700/v1/documents/<id>?store=notes&store=books"
```

---

### `GET /v1/config`

Returns the parsed configuration as localdb sees it, together with the effective store list (all
runtime-created stores from the DB).

```
curl -s http://127.0.0.1:7700/v1/config
```

```json
{
  "yaml_config": {
    "defaults": {
      "indexing": {
        "chunking": {
          "preset_overrides": {}
        },
        "embedding": {
          "model": "pplx-embed-context-v1-0.6b",
          "provider": "local-onnx"
        }
      }
    },
    "paths": {
      "data": "/path/to/data",
      "logs": "/path/to/logs",
      "models": "/path/to/models"
    },
    "providers": [],
    "server": {
      "bind": "127.0.0.1",
      "port": 7700
    },
    "stores": [],
    "version": 1
  },
  "effective_stores": [
    {
      "name": "notes",
      "visibility": "private",
      "backend": "libsql"
    }
  ]
}
```

`effective_stores` lists all stores registered via `localdb store add` (or `POST /v1/stores`). The
DB is the single source of truth — there is no YAML store declaration. Config schema details are in
[specs/03-config.md](https://github.com/dokterbob/localdb/blob/main/specs/03-config.md).

---

### `GET /v1/auth/me`

Returns the caller's resolved identity. Requires a bearer token once auth is enforced — the same
tier as the plain retrieval routes (`GET /v1/stores`, `POST /v1/search`, …). In open mode (no auth
enforced, see [Trust model and authentication](#trust-model-and-authentication)) this returns the
implicit local-trust admin identity, `user_id: "local"`.

```
curl -s http://127.0.0.1:7700/v1/auth/me \
  -H 'Authorization: Bearer ldb_...'
```

```json
{
  "user_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "name": "alice",
  "role": "member",
  "store_access": { "granted": { "stores": ["docs"] } }
}
```

An admin, or the implicit local-trust principal in open mode, gets `"store_access": "all"` instead
of a `granted` object. `role` is `"admin"` or `"member"`.

Returns `401 unauthorized` for a missing or invalid bearer token once auth is enforced (the
`require_auth` middleware rejects the request before this handler runs).

---

### `GET /v1/users`

Admin-only. Lists every user account.

```
curl -s http://127.0.0.1:7700/v1/users \
  -H 'Authorization: Bearer ldb_...'
```

```json
[
  {
    "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "name": "alice",
    "role": "member",
    "created_at": "2026-06-10T12:00:00Z"
  }
]
```

Unlike the store/source/document list routes, this is a bare JSON array — there is no pagination
envelope. Never carries credential material; see [`GET /v1/users/{id}/keys`](#get-v1usersidkeys) for
API keys.

Returns `403 forbidden` for a non-admin bearer, `401 unauthorized` for a missing or invalid one.

---

> None of the request-body structs documented in this reference reject unknown fields (no
> `#[serde(deny_unknown_fields)]` on any of them) — an unrecognized field in a JSON request body is
> silently ignored everywhere below, not an error.

### `POST /v1/users`

Admin-only. Creates a user account. There are no passwords (identity is proven solely by bearer
secrets) — mint an API key for the new user afterwards via
[`POST /v1/users/{id}/keys`](#post-v1usersidkeys).

**Request body:**

| Field  | Type   | Required | Description                             |
| ------ | ------ | -------- | --------------------------------------- |
| `name` | string | yes      | User name; must be non-empty and unique |
| `role` | string | yes      | `"admin"` or `"member"`                 |

```
curl -s -X POST http://127.0.0.1:7700/v1/users \
  -H 'Authorization: Bearer ldb_...' -H 'Content-Type: application/json' \
  -d '{"name":"alice","role":"member"}'
```

Returns `201`:

```json
{
  "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "name": "alice",
  "role": "member",
  "created_at": "2026-06-10T12:00:00Z"
}
```

`invalid_request`, 400, for an empty `name`, a `name` that already exists, or an unrecognized `role`
value. `403 forbidden` for a non-admin bearer.

A missing or wrong-typed field in the body — e.g. no `role` at all — does **not** get the
`invalid_request` envelope above: this route (and every other body-carrying route in this section,
except `POST /v1/invites/redeem` and `POST /v1/invites/requests/{id}/poll`) extracts its body with
axum's plain `Json<T>`, not the `ApiJson` wrapper `POST /v1/jobs` uses. A missing/mistyped field is
axum's own plain-text `422 Unprocessable Entity`, invalid JSON syntax is plain-text `400`, and a
missing or wrong `Content-Type` is plain-text `415` — none of the three carry a `code`/`message`
body.

---

### `PATCH /v1/users/{id}`

Admin-only. Sets the user's role.

**Request body:**

| Field  | Type   | Required | Description             |
| ------ | ------ | -------- | ----------------------- |
| `role` | string | yes      | `"admin"` or `"member"` |

```
curl -s -X PATCH http://127.0.0.1:7700/v1/users/01ARZ3NDEKTSV4RRFFQ69G5FAV \
  -H 'Authorization: Bearer ldb_...' -H 'Content-Type: application/json' \
  -d '{"role":"admin"}'
```

```json
{
  "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "name": "alice",
  "role": "admin",
  "created_at": "2026-06-10T12:00:00Z"
}
```

`invalid_request`, 400, for an unrecognized `role` value, an unknown `id`, or an attempt to demote
the sole remaining admin to `member` — this last-admin lockout guard applies generically, so it also
catches an admin demoting the _other_ admin down to zero, not only a self-demote. `403 forbidden`
for a non-admin bearer. Same malformed-body caveat as [`POST /v1/users`](#post-v1users).

---

### `DELETE /v1/users/{id}`

Admin-only. Deletes a user. Cascades to that user's API keys and store grants at the schema level —
deleting a user also revokes every key it owns and every grant it held.

```
curl -s -X DELETE http://127.0.0.1:7700/v1/users/01ARZ3NDEKTSV4RRFFQ69G5FAV \
  -H 'Authorization: Bearer ldb_...'
```

Returns `204 No Content` (empty body) on success. `invalid_request`, 400, for an unknown `id` or an
attempt to delete the sole remaining admin (same last-admin guard as
[`PATCH /v1/users/{id}`](#patch-v1usersid)). `403 forbidden` for a non-admin bearer.

---

### `GET /v1/users/{id}/keys`

Admin-only. Lists the named user's API keys — metadata only, never a secret: a key's plaintext is
shown exactly once, in the response of [`POST /v1/users/{id}/keys`](#post-v1usersidkeys) itself, and
is not retrievable again after that.

```
curl -s http://127.0.0.1:7700/v1/users/01ARZ3NDEKTSV4RRFFQ69G5FAV/keys \
  -H 'Authorization: Bearer ldb_...'
```

```json
[
  {
    "id": "01ARZ3NDEKTSV4RRFFQ69G5KEY",
    "created_at": "2026-06-10T12:00:00Z",
    "last_used_at": null,
    "expires_at": null,
    "revoked_at": null
  }
]
```

Only `kind = api_key` tokens are listed here — OAuth access/refresh session tokens are an
implementation detail of the login flow, not something a caller manages through this route.
`invalid_request`, 400, for an unknown `id` — not `404`; this route, like every other route in this
section that looks up a user or key by id, reports a bad id as `invalid_request` rather than the
`*_not_found` shape stores/sources/jobs use. `403 forbidden` for a non-admin bearer.

---

### `POST /v1/users/{id}/keys`

Mints a new API key for the named user. Admin-only for another user, with one carve-out: a caller
may always mint a key **for themselves** (`id` equal to the caller's own `user_id`) — the one
self-service exception among the admin-only management routes in this section.

```
curl -s -X POST http://127.0.0.1:7700/v1/users/01ARZ3NDEKTSV4RRFFQ69G5FAV/keys \
  -H 'Authorization: Bearer ldb_...'
```

Returns `201`:

```json
{
  "id": "01ARZ3NDEKTSV4RRFFQ69G5KEY",
  "secret": "ldb_QUFBQTExMTFCQkJCMjIyMkNDQ0MzMzMzRERERDQ0NAA"
}
```

`secret` is the plaintext bearer secret (`ldb_`-prefixed) — **shown exactly once, in this
response**. It is never persisted in retrievable form (only its blake3 hash is stored) and cannot be
recovered later; losing it means revoking the key ([`DELETE /v1/keys/{id}`](#delete-v1keysid)) and
minting a new one. This route takes no request body — only the `{id}` path segment.
`invalid_request`, 400, for an unknown `id` (same 400-not-404 shape as above). `403 forbidden` if
the caller is neither an admin nor minting a key for themselves.

The minted key inherits the caller's own session ancestry, whatever credential authenticated the
caller: an access token's own `family_id`, or an API key's `minted_by_family` if that key was itself
minted under a session (or under another key already tainted by one). That ancestry becomes the new
key's `minted_by_family`, so reuse-detected revocation of the originating session revokes every key
descended from it, transitively — including one an admin mints for another user from a stolen
session, or from a key already tainted by one.

---

### `DELETE /v1/keys/{id}`

Admin-only. Revokes any token by its own `id` (not its secret) — lets an admin revoke a lost or
compromised API key without needing the plaintext secret itself.

```
curl -s -X DELETE http://127.0.0.1:7700/v1/keys/01ARZ3NDEKTSV4RRFFQ69G5KEY \
  -H 'Authorization: Bearer ldb_...'
```

Returns `204 No Content` (empty body) on success. `invalid_request`, 400, for an unknown `id` (same
400-not-404 shape as above). `403 forbidden` for a non-admin bearer.

---

### `GET /v1/stores/{name}/grants`

Admin-only. Lists every D7 store grant on the named store, with each grantee's user name resolved
alongside its id.

```
curl -s http://127.0.0.1:7700/v1/stores/docs/grants \
  -H 'Authorization: Bearer ldb_...'
```

```json
[
  {
    "store_name": "docs",
    "user_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "user_name": "alice",
    "granted_by": "01ARZ3NDEKTSV4RRFFQ69G5ADM",
    "created_at": "2026-06-10T12:00:00Z"
  }
]
```

`granted_by` is the `user_id` of the admin who created the grant. `store_not_found`, 404, if the
named store does not exist. `403 forbidden` for a non-admin bearer.

---

### `POST /v1/stores/{name}/grants`

Admin-only. Grants a member read access to a `shared` store (D7).

**Request body:**

| Field  | Type   | Required | Description                         |
| ------ | ------ | -------- | ----------------------------------- |
| `user` | string | yes      | The grantee, by user name **or** id |

```
curl -s -X POST http://127.0.0.1:7700/v1/stores/docs/grants \
  -H 'Authorization: Bearer ldb_...' -H 'Content-Type: application/json' \
  -d '{"user":"alice"}'
```

Returns `201`:

```json
{
  "store_name": "docs",
  "user_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "user_name": "alice",
  "granted_by": "01ARZ3NDEKTSV4RRFFQ69G5ADM",
  "created_at": "2026-06-10T12:00:00Z"
}
```

`user` is resolved by name first, falling back to id only if no user has that exact name: a client
that always passes stored user IDs still works, unless some other user happens to be named
identically to that ID string (vanishingly unlikely given ULIDs' alphabet, but the name match wins
if it ever happens).

Granting the same user the same store again updates the existing row (`granted_by`/`created_at`
refreshed) rather than erroring or duplicating it. `store_not_found`, 404, for an unknown store
name. `forbidden`, 403, if the named store is `private` — only `shared` stores are grantable (this
is a different 403 than the one a non-admin bearer gets). `invalid_request`, 400, if `user` names
neither an existing user name nor id. Same malformed-body caveat as
[`POST /v1/users`](#post-v1users).

---

### `DELETE /v1/stores/{name}/grants/{user}`

Admin-only. Revokes `user`'s (name or id) grant on `store_name` — `user` is resolved the same way as
[`POST .../grants`](#post-v1storesnamegrants)'s `user` field: by name first, falling back to id.

```
curl -s -X DELETE http://127.0.0.1:7700/v1/stores/docs/grants/alice \
  -H 'Authorization: Bearer ldb_...'
```

Returns `204 No Content` (empty body) on success. `invalid_request`, 400, if `user` names neither an
existing user name nor id, **or** if that user holds no grant on `store_name` — unlike
[`GET`](#get-v1storesnamegrants) and [`POST .../grants`](#post-v1storesnamegrants) above, this route
does not check that the named store exists first: an unknown store name and "no such grant" both
collapse to the same `invalid_request`/400, never `store_not_found`/404. `403 forbidden` for a
non-admin bearer.

---

### `GET /v1/invites`

Admin-only. Lists every invite — no secrets (the plaintext token is shown exactly once, at creation,
in [`POST /v1/invites`](#post-v1invites)'s response).

```
curl -s http://127.0.0.1:7700/v1/invites \
  -H 'Authorization: Bearer ldb_...'
```

```json
[
  {
    "id": "01ARZ3NDEKTSV4RRFFQ69G5NVT",
    "mode": "open",
    "store_grants": ["docs"],
    "max_uses": 1,
    "uses": 0,
    "expires_at": null,
    "revoked_at": null,
    "created_by": "01ARZ3NDEKTSV4RRFFQ69G5ADM",
    "created_at": "2026-06-10T12:00:00Z"
  }
]
```

`403 forbidden` for a non-admin bearer.

---

### `POST /v1/invites`

Admin-only. Creates an invite (D9). See
[Trust model and authentication](#trust-model-and-authentication) for how an invite's `consent_url`
is used from a browser.

**Request body:**

| Field        | Type     | Required | Description                                                                                                                      |
| ------------ | -------- | -------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `mode`       | string   | yes      | `"open"` (redemption creates the user immediately) or `"closed"` (redemption files a pending request that an admin must approve) |
| `stores`     | string[] | no       | Store names granted to the resulting user on redemption; default `[]`. Any named store that is `private` is rejected             |
| `max_uses`   | integer  | no       | `u32`; range 1-4294967295, default `1`; `0` is rejected                                                                          |
| `expires_at` | string   | no       | RFC 3339 timestamp, or `null`/omitted for no expiry; normalized to UTC on acceptance                                             |

```
curl -s -X POST http://127.0.0.1:7700/v1/invites \
  -H 'Authorization: Bearer ldb_...' -H 'Content-Type: application/json' \
  -d '{"mode":"open","stores":["docs"],"max_uses":1}'
```

Returns `201`:

```json
{
  "id": "01ARZ3NDEKTSV4RRFFQ69G5NVT",
  "mode": "open",
  "store_grants": ["docs"],
  "max_uses": 1,
  "expires_at": null,
  "created_at": "2026-06-10T12:00:00Z",
  "token": "ldb_S0VZMWFhYWFCQkJCY2NjY0RERERlZWVlRkZGRmdnZwA",
  "consent_url": "http://127.0.0.1:7700/join?invite=ldb_S0VZMWFhYWFCQkJCY2NjY0RERERlZWVlRkZGRmdnZwA"
}
```

`token` is the plaintext invite secret — **shown exactly once, in this response** (blake3-hashed at
rest; not recoverable afterward). `consent_url` is a ready-made link to the standalone
`GET /join?invite=<token>` join page for a browser invitee with no CLI and no OAuth client, built
from `server.public_url` if configured, else the request's own `Host` header.

`invalid_request`, 400, for an unrecognized `mode` or `max_uses: 0`. `invalid_request`, 400, for an
`expires_at` that isn't a valid RFC 3339 timestamp, with the message
`expires_at must be an RFC 3339 timestamp`; an accepted value is normalized to UTC before being
stored, regardless of the offset it was submitted with. A `max_uses` that is negative, fractional,
or exceeds `4294967295` never reaches that `0` check at all — it fails to deserialize into this
route's plain `Json<CreateInviteRequest>` body, so it gets the same plain-text
`422 Unprocessable Entity` as any other wrong-typed field (see the malformed-body caveat below), not
the `invalid_request` envelope. `store_not_found`, 404, for an unknown store name in `stores`.
`forbidden`, 403, if any named store in `stores` is `private` — only `shared` stores can be granted
via invite (a different 403 than the one a non-admin bearer gets). `invalid_request`, 400, if
`server.public_url` is not configured and the request's `Host` header is missing or malformed —
there is then no base URL to build `consent_url` from, so the invite is never created. Same
malformed-body caveat as [`POST /v1/users`](#post-v1users).

---

### `DELETE /v1/invites/{id}`

Admin-only. Revokes an invite — a revoked invite can no longer be redeemed, but this does not affect
users already created by earlier redemptions, nor any access request already filed against it: a
pending request can still be [approved](#post-v1invitesrequestsidapprove) after its invite is
revoked (approval never re-checks the invite's state) — to refuse it, the request must be
[denied](#post-v1invitesrequestsiddeny) explicitly.

```
curl -s -X DELETE http://127.0.0.1:7700/v1/invites/01ARZ3NDEKTSV4RRFFQ69G5NVT \
  -H 'Authorization: Bearer ldb_...'
```

Returns `204 No Content` (empty body) on success. `invalid_request`, 400, for an unknown `id` or one
already revoked. `403 forbidden` for a non-admin bearer.

---

### `POST /v1/invites/redeem`

**Public — no bearer token, even when auth is enforced.** This route, together with
[`POST /v1/invites/requests/{id}/poll`](#post-v1invitesrequestsidpoll) below, _is_ the
invite-redemption flow — see [Trust model and authentication](#trust-model-and-authentication).

**Request body:**

| Field            | Type   | Required | Description                   |
| ---------------- | ------ | -------- | ----------------------------- |
| `token`          | string | yes      | The invite's plaintext secret |
| `requested_name` | string | yes      | The requested new user's name |

```
curl -s -X POST http://127.0.0.1:7700/v1/invites/redeem \
  -H 'Content-Type: application/json' \
  -d '{"token":"ldb_S0VZMWFhYWFCQkJCY2NjY0RERERlZWVlRkZGRmdnZwA","requested_name":"alice"}'
```

`open`-mode invite → `201`, the user, its granted stores, and a show-once API key, all created
immediately:

```json
{
  "user": { "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "name": "alice", "role": "member" },
  "granted_stores": ["docs"],
  "api_key": "ldb_QUFBQTExMTFCQkJCMjIyMkNDQ0MzMzMzRERERDQ0NAA"
}
```

`closed`-mode invite → `202`, a pending access request is filed instead:

```json
{
  "request_id": "01ARZ3NDEKTSV4RRFFQ69G5REQ",
  "request_secret": "ldb_UkRNMmZmZmZHR0dHaGhoaElJSUlqampqS0tLS2xsbAA",
  "poll": "/v1/invites/requests/01ARZ3NDEKTSV4RRFFQ69G5REQ/poll"
}
```

`api_key` and `request_secret` are both **shown exactly once**, in this response, and are never
retrievable again afterward — a lost `request_secret` means the request can never be polled (there
is no way to recover or re-request it short of redeeming the invite again, which consumes another
use if `max_uses` allows it).

Both `token` and `requested_name` are required. A body that isn't a JSON object at all — malformed
JSON, a wrong `Content-Type`, or (since neither field has a default) one of them missing entirely —
is `invalid_request`, 400, with the fixed message `request body must be a JSON object`; the body is
never echoed back. A well-formed body with an empty `token` or `requested_name` is also
`invalid_request`, 400, but with the message `token and requested_name are both required`.

An unknown, revoked, expired, or exhausted (`max_uses` already reached) invite is `unauthorized`,
401, in every case, but the message differs: `invalid or unknown invite token`,
`invite has been revoked`, `invite has expired`, and `invite has no remaining uses` respectively —
same status and code, different message.

A `requested_name` that collides with an existing user name is rejected at redemption with the same
`invalid_request`/400 duplicate-name shape [`POST /v1/users`](#post-v1users) gets — **but only for
an `open`-mode invite**. A `closed`-mode invite does not check the name at all: redemption accepts
it, consumes a use, and returns `202` as normal; the collision only surfaces later, when an admin
tries to [approve the resulting request](#post-v1invitesrequestsidapprove), which then fails with
`invalid_request`/400 (`user '<name>' already exists`) and leaves the request `pending`. Approval
keeps failing for as long as the name stays taken. [Deny](#post-v1invitesrequestsiddeny) the request
to reject it; if the conflicting user is deleted, a later approval can succeed.

---

### `GET /v1/invites/requests`

Admin-only. Lists every access request across every invite — pending, approved, and denied alike; no
pagination (this is a small admin-facing surface).

```
curl -s http://127.0.0.1:7700/v1/invites/requests \
  -H 'Authorization: Bearer ldb_...'
```

```json
[
  {
    "id": "01ARZ3NDEKTSV4RRFFQ69G5REQ",
    "invite_id": "01ARZ3NDEKTSV4RRFFQ69G5NVT",
    "requested_name": "bob",
    "state": "pending",
    "resulting_user_id": null,
    "created_at": "2026-06-10T12:00:00Z",
    "decided_at": null
  }
]
```

`state` is `"pending"`, `"approved"`, or `"denied"`. The requester's own secret is never included
here — it was shown once, in [`POST /v1/invites/redeem`](#post-v1invitesredeem)'s `closed`-mode
response. `403 forbidden` for a non-admin bearer.

---

### `POST /v1/invites/requests/{id}/approve`

Admin-only. Approves a pending access request: creates the user and applies the invite's store
grants. No credential is minted here — a fresh API key is minted only when the requester's own next
[poll](#post-v1invitesrequestsidpoll) collects it.

```
curl -s -X POST http://127.0.0.1:7700/v1/invites/requests/01ARZ3NDEKTSV4RRFFQ69G5REQ/approve \
  -H 'Authorization: Bearer ldb_...'
```

```json
{ "id": "01ARZ3NDEKTSV4RRFFQ69G5B0B", "name": "bob", "role": "member" }
```

User creation, the invite's store grants, and the approval decision all commit atomically — a
failure anywhere leaves the request pending rather than partially applied. The most common failure
is a `requested_name` that now collides with an existing user name — this can only happen for a
`closed`-mode invite, since an `open`-mode one already checked uniqueness at redemption (see
[`POST /v1/invites/redeem`](#post-v1invitesredeem)) — which returns `invalid_request`, 400
(`user '<name>' already exists`), and leaves the request `pending`. Approval keeps failing for as
long as the name stays taken: [deny](#post-v1invitesrequestsiddeny) the request to reject it, or
delete the conflicting user and approve again.

Approval does not re-check the invite's own state: a pending request can still be approved after its
invite has been [revoked](#delete-v1invitesid) or has expired. If access should be refused once an
invite is revoked or expired, every request still pending against it must be denied explicitly.

`invalid_request`, 400, for an unknown `id` (not `404`) or a request that is no longer `pending`
(already approved or denied). `403 forbidden` for a non-admin bearer.

---

### `POST /v1/invites/requests/{id}/deny`

Admin-only. Denies a pending access request; the requester's next
[poll](#post-v1invitesrequestsidpoll) observes `"denied"`.

```
curl -s -X POST http://127.0.0.1:7700/v1/invites/requests/01ARZ3NDEKTSV4RRFFQ69G5REQ/deny \
  -H 'Authorization: Bearer ldb_...'
```

Returns `204 No Content` (empty body) on success. `invalid_request`, 400, for an unknown `id` (not
`404`) or a request that is no longer `pending`. `403 forbidden` for a non-admin bearer.

---

### `POST /v1/invites/requests/{id}/poll`

**Public — no bearer token, even when auth is enforced.** Polls a `closed`-mode access request's
status; see [`POST /v1/invites/redeem`](#post-v1invitesredeem) above.

The secret travels in the JSON body, not a query parameter or the URL path — a URL is routinely
recorded in reverse-proxy and access logs, and whoever can read those would otherwise be able to
poll first and collect the one-time API key ahead of the real requester (the same reasoning as RFC
8628's device-authorization grant, which sends its device code the same way).

**Request body:**

| Field    | Type   | Required | Description                                   |
| -------- | ------ | -------- | --------------------------------------------- |
| `secret` | string | yes      | The `request_secret` from the redeem response |

```
curl -s -X POST http://127.0.0.1:7700/v1/invites/requests/01ARZ3NDEKTSV4RRFFQ69G5REQ/poll \
  -H 'Content-Type: application/json' \
  -d '{"secret":"ldb_UkRNMmZmZmZHR0dHaGhoaElJSUlqampqS0tLS2xsbAA"}'
```

Malformed JSON, a wrong field type, or a missing/wrong `Content-Type` is `invalid_request`, 400,
with the fixed message `request body must be a JSON object` (the `SecretJson` extractor collapses
every such rejection to this, never echoing the body back) — distinct from a well-formed body with a
missing, `null`, or empty `secret`, which is `unauthorized`, 401, as described below.

Otherwise returns `200`; the body's `state` tells the caller what to do next. One exception: if the
user created by the approval is deleted before the key has been collected, the poll returns
`internal`, 500, and no key is issued.

```json
{ "state": "pending" }
```

```json
{ "state": "denied" }
```

```json
{ "state": "approved", "api_key": "ldb_QUFBQTExMTFCQkJCMjIyMkNDQ0MzMzMzRERERDQ0NAA" }
```

```json
{ "state": "collected" }
```

`"approved"` with an `api_key` is returned **exactly once** — on the first poll to observe the
request's `approved` transition, marking it collected in the same transaction that mints the key.
Every later poll on the same request answers `"collected"` instead, with no `api_key` field; a poll
that loses a race against another concurrent poll also gets `"collected"`, never a second key. An
unknown `id` and a wrong `secret` presented against a real one are **deliberately
indistinguishable**: `unauthorized`, 401, with an identical message either way — no existence
oracle. An empty or missing `secret` in the body gets that same `unauthorized`/401 response, not
`invalid_request`.

---

### `POST /v1/search`

Hybrid search across stores. Returns a ranked citation list over the same data the CLI indexes — the
daemon and the CLI share `<data_dir>/localdb.db`.

**Request body:**

| Field                              | Type     | Required | Description                                                                                                                   |
| ---------------------------------- | -------- | -------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `query`                            | string   | yes      | Natural language search query                                                                                                 |
| `store_filter`                     | string[] | no       | Store names to search; omit or pass `[]` to search all stores                                                                 |
| `limit`                            | int      | no       | Maximum results to return (default: 10; silently clamped to 100, `SEARCH_MAX_LIMIT`)                                          |
| `cursor`                           | string   | no       | Pagination cursor from a previous response                                                                                    |
| `path`                             | string   | no       | Restrict to resources whose URI starts with this prefix. Matched with SQL `LIKE`, so a literal `%` or `_` acts as a wildcard. |
| `mime`                             | string   | no       | Restrict to resources with this exact MIME type, compared as an exact string.                                                 |
| `added_after`/`added_before`       | string   | no       | Bound on when the resource was first indexed                                                                                  |
| `updated_after`/`updated_before`   | string   | no       | Bound on when the store last wrote the resource's stored state                                                                |
| `modified_after`/`modified_before` | string   | no       | Bound on the source's own claimed last-modified time                                                                          |
| `document_after`/`document_before` | string   | no       | Bound on the document's own claimed date (Dublin Core `dc:date`)                                                              |

Each date field accepts a full RFC 3339 datetime, a partial date (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`),
or a relative duration (`7d`, `30m`, `2w`) — a duration always resolves to **now minus the
duration**, for either bound (`modified_after: "7d"` means "modified within the last 7 days";
`modified_before: "7d"` means "modified more than 7 days ago"). A resource with no value on a
filtered axis is excluded regardless of the bound (the NULL rule), and a resource carries a document
date only when its source supplied one — HTML, Markdown front matter, Office, PDF (`/CreationDate`
or XMP), and feed entries all can, plain text cannot. A malformed filter value is `invalid_request`,
HTTP 400. Multiple filters, of any kind, always AND together.

```
curl -s -X POST http://127.0.0.1:7700/v1/search \
  -H 'Content-Type: application/json' \
  -d '{"query":"hybrid search","limit":1,"modified_after":"30d"}'
```

```json
{
  "citations": [],
  "total_candidates": 0,
  "next_cursor": null
}
```

Each citation in `citations` follows the canonical Citation shape defined in
[specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
§6. For a fully-populated example see the `localdb search --json` output in the CLI reference.

A `limit` above 100 is not an error — it is silently clamped to 100
(`localdb_core::SEARCH_MAX_LIMIT`), matching the MCP `search` tool's own cap.

---

### `GET /v1/jobs`

List every job on the daemon's queue, in any state, across every store.

```
curl -s http://127.0.0.1:7700/v1/jobs
```

```json
[
  {
    "id": "01KTVM5XMA59N4WGHNZ80QX9B7",
    "store_id": "notes",
    "scope": { "type": "store" },
    "state": "running",
    "stats": {
      "docs_seen": 0,
      "docs_indexed": 0,
      "docs_skipped": 0,
      "docs_deleted": 0,
      "docs_prunable": 0,
      "chunks_written": 0,
      "unsupported_format_count": 0,
      "error_count": 0,
      "sources_count": 0
    },
    "error": null,
    "error_code": null,
    "created_at": "2026-06-11T15:17:59Z",
    "started_at": "2026-06-11T15:17:59Z",
    "completed_at": null
  }
]
```

Returns the raw `IndexJob[]` array directly — unlike `/v1/stores` and `/v1/stores/{name}/sources`,
there is no pagination envelope: jobs are ephemeral operational records with bounded retention (the
registry caps how many terminal jobs it keeps, evicting the oldest first), so the response never
grows unbounded. Order is registry iteration order, not guaranteed stable.

---

### `POST /v1/jobs`

Submit an index job for a store. This runs the real ingestion pipeline (`server::job_exec::run_job`)
through an async job queue with a configurable worker pool (`server.job_workers`, default 1, issues
#187/#208) — the daemon processes the job asynchronously, in the background; poll
`GET /v1/jobs/{id}` or stream `GET /v1/jobs/{id}/events` for progress.

**Request body:**

| Field             | Type   | Required | Description                                                                                                                                                                               |
| ----------------- | ------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `store_name`      | string | yes      | Name of the store to index                                                                                                                                                                |
| `source_id`       | string | no       | Index only this source; omit to index the whole store                                                                                                                                     |
| `deletion_policy` | string | no       | `"retain"` (default) — never removes documents; `"delete"` — prunes documents no longer present at their source (mirrors CLI `index --delete`). Any other value is `invalid_request`, 400 |

```
curl -s -X POST http://127.0.0.1:7700/v1/jobs \
  -H 'Content-Type: application/json' \
  -d '{"store_name":"notes"}'
```

```json
{
  "id": "01KTVM5XMA59N4WGHNZ80QX9B7",
  "store_id": "notes",
  "scope": { "type": "store" },
  "state": "pending",
  "stats": {
    "docs_seen": 0,
    "docs_indexed": 0,
    "docs_skipped": 0,
    "docs_deleted": 0,
    "docs_prunable": 0,
    "chunks_written": 0,
    "unsupported_format_count": 0,
    "error_count": 0,
    "sources_count": 0
  },
  "error": null,
  "error_code": null,
  "created_at": "2026-06-11T15:17:59Z",
  "started_at": null,
  "completed_at": null
}
```

> If you pass `"store"` instead of `"store_name"` the server returns a 422-style deserialisation
> error: `Failed to deserialize the JSON body into the target type: missing field 'store_name'`
> (followed by a line/column offset). Unknown keys are ignored rather than rejected —
> `CreateJobRequest` does not set `deny_unknown_fields`.

A second `POST /v1/jobs` for a store that already has a job queued or running is rejected with
`index_in_progress`, 409 (see [Error responses](#error-responses)), regardless of worker-pool size —
the in-flight guard is per-store, reserved atomically before the job is created, so two concurrent
submissions for the same store can never both proceed. Jobs for different stores run concurrently,
up to `server.job_workers` workers (default 1); same-store jobs are always serialized by the
per-store guard.

---

### `GET /v1/jobs/{id}`

Poll the status of a previously submitted job.

```
curl -s http://127.0.0.1:7700/v1/jobs/01KTVM5XMA59N4WGHNZ80QX9B7
```

```json
{
  "id": "01KTVM5XMA59N4WGHNZ80QX9B7",
  "store_id": "notes",
  "scope": {
    "type": "store"
  },
  "state": "done",
  "stats": {
    "docs_seen": 3,
    "docs_indexed": 3,
    "docs_skipped": 0,
    "docs_deleted": 0,
    "docs_prunable": 0,
    "chunks_written": 12,
    "unsupported_format_count": 0,
    "error_count": 0,
    "sources_count": 1
  },
  "error": null,
  "error_code": null,
  "created_at": "2026-06-11T15:17:59Z",
  "started_at": "2026-06-11T15:17:59Z",
  "completed_at": "2026-06-11T15:17:59Z"
}
```

**Job fields:**

| Field          | Type         | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| -------------- | ------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`           | string       | ULID job identifier                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `store_id`     | string       | Store name the job runs against                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `scope`        | object       | `{"type":"store"}` for a full-store index, `{"type":"source","source_id":"..."}` for one source. `{"type":"document","resource_id":"..."}` also exists in the type but is currently unreachable — `POST /v1/jobs` has no `resource_id` field to construct it                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `state`        | string       | `"pending"`, `"running"`, `"done"`, or `"failed"`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `stats`        | object       | Running counters (see below)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `error`        | string\|null | Error message if the job failed                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `error_code`   | string\|null | Stable error code (see [Error responses](#error-responses)) if the job failed with a typed error — e.g. `"invalid_config"` for an embedder-construction failure. `null` for a synthetic queue-level failure (the queue itself full/closed, or the job's task panicking) that never had one, and always `null` on `"done"`. Issue #187 review, finding 3: lets a daemon-attached CLI client reconstruct the original error and exit with the same code an equivalent embedded failure would, instead of every job failure collapsing to a generic internal error. `#[serde(default)]` on the Rust side, so a daemon predating this field omits the key entirely rather than sending `null` — treat a missing key the same as `null` |
| `created_at`   | string       | ISO 8601 timestamp                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `started_at`   | string\|null | ISO 8601 timestamp; null while pending                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `completed_at` | string\|null | ISO 8601 timestamp; null while running                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |

**Stats fields:**

| Field                      | Description                                                                                                                                                                                              |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `docs_seen`                | Files/URLs examined                                                                                                                                                                                      |
| `docs_indexed`             | New or changed documents ingested                                                                                                                                                                        |
| `docs_skipped`             | Documents skipped (unchanged content hash)                                                                                                                                                               |
| `docs_deleted`             | Documents removed because the source is gone (only ever non-zero with `deletion_policy: "delete"`)                                                                                                       |
| `docs_prunable`            | Documents that would have been deleted had `deletion_policy: "delete"` been requested — always 0 on a run that actually deleted (they were removed and counted in `docs_deleted` instead)                |
| `chunks_written`           | Chunks written to the vector store                                                                                                                                                                       |
| `unsupported_format_count` | Files skipped due to unrecognised format                                                                                                                                                                 |
| `error_count`              | Per-document errors                                                                                                                                                                                      |
| `sources_count`            | Number of sources the job's scope resolved to, before any were processed — distinguishes "nothing to index" (0) from "sources existed but nothing needed indexing" (>0, other counters possibly still 0) |

---

### `DELETE /v1/jobs/{id}`

Request cancellation of a queued or running job (issue #218).

```
curl -s -X DELETE http://127.0.0.1:7700/v1/jobs/01KTVM5XMA59N4WGHNZ80QX9B7
```

```json
{
  "id": "01KTVM5XMA59N4WGHNZ80QX9B7",
  "store_id": "notes",
  "scope": { "type": "store" },
  "state": "running",
  "stats": {
    "docs_seen": 0,
    "docs_indexed": 0,
    "docs_skipped": 0,
    "docs_deleted": 0,
    "docs_prunable": 0,
    "chunks_written": 0,
    "unsupported_format_count": 0,
    "error_count": 0,
    "sources_count": 0
  },
  "error": null,
  "error_code": null,
  "created_at": "2026-06-11T15:17:59Z",
  "started_at": "2026-06-11T15:17:59Z",
  "completed_at": null
}
```

Returns `202` and the job's snapshot at the moment cancellation was requested — **not** a guarantee
it has already stopped. Poll `GET /v1/jobs/{id}` or watch `GET /v1/jobs/{id}/events` for the
eventual terminal state. Cancellation does not add a new `state` value — it reuses the existing
`"failed"` terminal state with `error_code: "job_cancelled"`:

```json
{
  "id": "01KTVM5XMA59N4WGHNZ80QX9B7",
  "store_id": "notes",
  "scope": { "type": "store" },
  "state": "failed",
  "stats": { "...": "..." },
  "error": "job was cancelled",
  "error_code": "job_cancelled",
  "created_at": "2026-06-11T15:17:59Z",
  "started_at": "2026-06-11T15:17:59Z",
  "completed_at": "2026-06-11T15:17:59Z"
}
```

`404` with error code `job_not_found` for an unknown job id. `409` with error code
`job_already_terminal` for a job that already reached `"done"` or `"failed"` — a cancel landing
after normal completion (or after a real failure) must never overwrite the recorded outcome:

```json
{ "code": "job_already_terminal", "message": "job already reached a terminal state; cannot cancel" }
```

---

### `GET /v1/jobs/{id}/events`

Stream a job's live progress as
[Server-Sent Events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events) (issue
#83).

```
curl -N -H 'Accept: text/event-stream' http://127.0.0.1:7700/v1/jobs/01KTVM5XMA59N4WGHNZ80QX9B7/events
```

Each in-flight update is an `event: progress` frame, `data:` a JSON-serialized `core::ProgressEvent`
(internally tagged on `type`):

```
event: progress
data: {"type":"source_started","source_id":"01K...","location":"/path/to/docs"}

event: progress
data: {"type":"discovered","total":3}

event: progress
data: {"type":"document_started","uri":"file:///path/to/docs/a.md","index":0,"total":3}

event: progress
data: {"type":"document_finished","uri":"file:///path/to/docs/a.md","outcome":{"outcome":"indexed","chunks":4}}
```

The stream always ends with exactly one `event: job` frame carrying the terminal `IndexJob` (the
same shape `GET /v1/jobs/{id}` returns, `state` either `"done"` or `"failed"`), after which the
connection closes:

```
event: job
data: {"id":"01KTVM5XMA59N4WGHNZ80QX9B7","store_id":"notes","scope":{"type":"store"},"state":"done","stats":{...},"error":null,"error_code":null,"created_at":"...","started_at":"...","completed_at":"..."}
```

A client that connects after the job has already reached a terminal state — or after its live
progress channel has already been torn down — receives _only_ that terminal `job` event,
immediately; it never sees the `progress` events it missed. Progress delivery is lossy/best-effort
by design (a lagging subscriber skips ahead rather than stalling the stream or buffering
unboundedly), but the terminal `job` event is always guaranteed exactly once. Unknown `job_id` →
`job_not_found`, 404, as an ordinary JSON error response (not an SSE frame — the 404 happens before
the stream opens).

---

## Pagination

List endpoints (`/v1/stores`, `/v1/stores/{name}/sources`, `/v1/stores/{name}/documents`) use
cursor-based pagination. `GET /v1/jobs` is a list endpoint but is _not_ paginated — see its own
section above for why.

| Query parameter | Default        | Description                                                    |
| --------------- | -------------- | -------------------------------------------------------------- |
| `cursor`        | —              | Opaque cursor from a previous response's `next_cursor`         |
| `limit`         | server default | Maximum items per page; must be ≥ 1 (`0` is `invalid_request`) |

A `next_cursor` of `null` means the last page has been reached.

---

## Error responses

All errors use the same JSON envelope:

```json
{ "code": "store_not_found", "message": "nope" }
```

| Field     | Type   | Description                                  |
| --------- | ------ | -------------------------------------------- |
| `code`    | string | Machine-readable error code (stable API)     |
| `message` | string | Error detail — see below for its exact shape |

For `store_not_found`, `source_not_found`, `resource_not_found`, `job_not_found`, `invalid_config`,
`invalid_request`, `provider_unavailable`, `model_missing`, and `rate_limited`, `message` is the
_bare_ field the error was built from (the id, or the validation/provider/rate-limit detail) — it
does **not** carry the human-readable prefix a CLI-rendered version of the same error would (e.g.
`"store not found: "`). This lets a daemon-attached client reconstruct the original typed error from
`code` + `message` (`core::Error::from_code`) and render its own prefix without doubling it; a
client that just wants display text should combine `code` and `message` itself (e.g.
`"store not found: nope"`). Every other code's `message` carries the full human-readable string
as-is.

HTTP status codes follow the shared error taxonomy in
[specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md) §5:

| Code                                                                            | HTTP status | Meaning                                                                                                                                                        |
| ------------------------------------------------------------------------------- | ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `store_not_found` / `source_not_found` / `resource_not_found` / `job_not_found` | 404         | Unknown entity                                                                                                                                                 |
| `runtime_state_locked`                                                          | 409         | Unified database locked by another process (SQLite `busy_timeout` exceeded)                                                                                    |
| `daemon_running`                                                                | 409         | A second daemon was started against the same data dir                                                                                                          |
| `daemon_unreachable`                                                            | 502         | Daemon socket exists but is not responding                                                                                                                     |
| `invalid_config`                                                                | 422         | Config failed validation                                                                                                                                       |
| `invalid_request`                                                               | 400         | Bad request body or arguments                                                                                                                                  |
| `unsupported_format`                                                            | 422         | Extractor cannot handle the file                                                                                                                               |
| `provider_unavailable`                                                          | 502         | External embedding endpoint down                                                                                                                               |
| `model_missing`                                                                 | 503         | Local model not yet downloaded                                                                                                                                 |
| `rate_limited`                                                                  | 502         | Retries against an upstream host exhausted; grouped with "upstream not currently servable" rather than 429, since it's an upstream limit, not the daemon's own |
| `index_in_progress`                                                             | 409         | Conflicting job already running for this scope                                                                                                                 |
| `job_already_terminal`                                                          | 409         | `DELETE /v1/jobs/{id}` requested for a job that already reached `done`/`failed` — cancellation must never overwrite a recorded outcome                         |
| `job_cancelled`                                                                 | n/a         | Never a live response's `code` — appears only as a cancelled job's `error_code` (`GET /v1/jobs/{id}`), reconstructed via `core::Error::from_code`              |
| `internal`                                                                      | 500         | Bug; response includes a `correlation_id` for log correlation                                                                                                  |

---

## Troubleshooting

### Diagnosing a rejected (4xx/5xx) request

`localdb serve` logs every response with status >= 400 at `warn` level, with the request's method,
path, status, and `Host` header — including responses from the nested `/mcp` mount (e.g. rmcp's own
DNS-rebinding Host-header check), not just `/v1` routes. This surfaces on stderr by default:
`localdb`'s default log filter (`warn,pdf_oxide=off`, set in `localdb/src/main.rs`) already passes
`warn`-level events through, so no `RUST_LOG` is needed to see a rejected request logged. Set
`RUST_LOG=debug` for more detail.

### `daemon_running` (exit 4) when starting `localdb serve`

Only one daemon may run against a given data directory at a time. If `localdb serve` exits
immediately with:

```
error: daemon is already running
exit: 4
```

there is already a daemon process running. Stop it before starting a new one.

### Stale `daemon.sock` / `daemon.url` after an ungraceful shutdown

If the daemon process is killed (e.g. with `kill <pid>` or a crash), the Unix socket file at
`<data_dir>/daemon.sock` and the discovery URL file at `<data_dir>/daemon.url` are **not cleaned
up**. The CLI will then report the daemon as running and `localdb search` will exit with:

```
error: daemon is unreachable
exit: 5
```

Fix: remove the stale files manually, then CLI commands will fall back to embedded mode.

```
rm <data_dir>/daemon.sock <data_dir>/daemon.url
```

After removal `localdb status` will show `daemon: not running (embedded mode)`.

Authentication errors: `unauthorized` maps to HTTP 401, `forbidden` to HTTP 403; both use CLI exit
code 6.
