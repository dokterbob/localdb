# HTTP API (`localdb serve`)

> **EXPERIMENTAL — do not rely on this surface for production use.**
>
> The daemon opens the same unified database (`<data_dir>/localdb.db`) as the CLI, so CLI-indexed data IS visible via `/v1/search`, `/v1/documents/{id}`, and `/v1/status`. The one open limitation in v0.1.0 is that ingestion via `POST /v1/jobs` is a no-op — to actually index, run `localdb index` from the CLI (which works concurrently with the daemon via SQLite WAL).
>
> For design rationale see [specs/05-surfaces.md](../specs/05-surfaces.md) §3.

---

## Starting the daemon

```
localdb serve
```

On startup the daemon prints a single announce line to stdout and then continues running:

```
daemon listening on http://127.0.0.1:7700
```

It binds the HTTP listener and also creates a Unix discovery socket at
`<data_dir>/daemon.sock` so that CLI and MCP processes can detect it, plus a
`<data_dir>/daemon.url` file recording the daemon's actual client-reachable base URL
(e.g. `http://192.168.1.5:7700` for a LAN bind, or `http://127.0.0.1:7700` when bound to
`0.0.0.0`/`::`, since the wildcard address itself isn't connectable). CLI/MCP discovery reads
this file, so it works for any configured bind address or port — not just the default
`127.0.0.1:7700`.

### Bind address and port

The bind address and port are controlled by the `server` block in `config.yaml`:

```yaml
version: 1
server:
  bind: 127.0.0.1   # default; any bind address is accepted (see Trust model below)
  port: 7700        # default; 0 = OS-assigned
```

Setting `port: 0` asks the OS for an ephemeral port. The assigned port is shown in the announce
line.

### Trust model and authentication

The daemon binds `127.0.0.1` by default. Whether requests need a bearer token is controlled by
`server.auth` in `config.yaml`:

| `server.auth` | Loopback bind | Non-loopback bind (LAN, Tailscale, `0.0.0.0`, …) |
|---|---|---|
| `auto` (default) | **Open** — no bearer token required | **Enforced** — every protected route requires `Authorization: Bearer ldb_...` |
| `required` | Enforced | Enforced |
| `off` | Open | **Hard error at startup** (`invalid_config`) — the daemon refuses to expose an unauthenticated surface to a network |

Binding to a specific non-loopback address is a deliberate trust decision and is accepted
(auth enforced automatically under the default `auto`). Binding to `0.0.0.0`/`::` (all
interfaces) additionally logs a startup warning, since that's reachable from every network the
machine is on. See [specs/05-surfaces.md](../specs/05-surfaces.md) §3 for the full binding and
auth-mode decision matrix.

**When auth is Open:** every request runs as an implicit admin principal (`Principal::local_trust()`)
— the same trust boundary as the CLI and embedded MCP: anything that can reach the bind address
is as trusted as the files themselves.

**When auth is Enforced:** a request with a missing or invalid bearer token gets `401` with
`WWW-Authenticate: Bearer` (plus, since the discovery routes below exist, a
`resource_metadata="<base>/.well-known/oauth-protected-resource"` parameter — see
[OAuth discovery](#oauth-discovery--dynamic-client-registration) below). A request from an
authenticated principal who lacks permission for the resource gets `403`. Members (as opposed
to admins) only see/search `shared`-visibility stores they hold an explicit grant for
(`localdb store grant`) — see [specs/05-surfaces.md](../specs/05-surfaces.md) §3.1 for the D7
authorization model.

**One-time setup code.** The first time `localdb serve` starts with auth enforced and zero
users exist yet, it prints a one-time setup code to stderr:

```
No users exist yet and authentication is enforced.
One-time setup code (use it to create the first admin account; shown only once):

    ldb_...
```

Paste that code into the browser consent page `GET /authorize` renders (or run
`localdb login --setup-code <code>`) to create the first admin account. The code is single-use
and is rejected once any user exists.

**Route table:**

| Routes | Auth |
|---|---|
| `GET /.well-known/oauth-protected-resource`, `GET /.well-known/oauth-authorization-server`, `GET\|POST /authorize`, `POST /token`, `POST /revoke`, `POST /register`, `POST /v1/invites/redeem`, `GET /v1/invites/requests/{id}` | **Public** — no bearer token, even when auth is enforced. These routes *are* the auth flow (OAuth discovery, DCR, and the code+PKCE/invite-redemption flows themselves). |
| `GET /v1/stores`, `GET /v1/stores/{name}`, `GET /v1/stores/{name}/sources`, `POST /v1/search`, `GET /v1/documents/{id}`, `GET /v1/auth/me`, `/mcp` | Bearer token required once enforced. Results are scoped by the principal's store access (admins: everything; members: granted `shared` stores only). |
| `POST/PATCH/DELETE /v1/stores`, `POST /v1/stores/{name}/sources`, `DELETE /v1/sources/{id}`, `POST/GET /v1/jobs`, `GET /v1/jobs/{id}`, `GET /v1/config` | Bearer token **and** `role = admin`. |
| `GET/POST /v1/users`, `PATCH/DELETE /v1/users/{id}`, `GET /v1/users/{id}/keys`, `DELETE /v1/keys/{id}`, `GET/POST /v1/stores/{name}/grants`, `DELETE /v1/stores/{name}/grants/{user}` | Bearer token **and** `role = admin` — except `POST /v1/users/{id}/keys` also allows a non-admin principal to mint a key for themselves. |
| `GET/POST /v1/invites`, `DELETE /v1/invites/{id}`, `GET /v1/invites/requests`, `POST /v1/invites/requests/{id}/approve\|deny` | Bearer token **and** `role = admin`. |

### OAuth discovery + Dynamic Client Registration

Once auth is enforced, three RFCs work together so a stock MCP client can onboard against
`/mcp` (or any bearer-protected route) with **zero static configuration** — no pre-shared
token, no manually-typed client ID:

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
   (`myapp://callback`) are rejected. `token_endpoint_auth_method`, if sent, must be `"none"`:
   this endpoint only ever registers **public** clients, so no `client_secret` is ever minted
   or returned. Unlike the built-in `localdb-cli` client (which gets an RFC 8252 §7.3
   loopback-*any-port* exception, since the CLI binds a fresh ephemeral port per login), a
   registered client's redirect_uri is matched **exactly** against what it registered — no
   loopback leniency.
5. It runs the ordinary code+PKCE flow (`GET/POST /authorize` → `POST /token`) using the
   `client_id` from step 4.

**Base URL resolution.** `<base>` in every URL above is `server.public_url` when configured
(trimmed of a trailing slash), or otherwise derived from the request's own `Host` header. Set
`server.public_url` whenever the daemon sits behind a TLS-terminating reverse proxy for remote
use — it becomes the canonical issuer/resource identifier and the daemon itself never needs to
know it's behind TLS:

```yaml
server:
  public_url: https://localdb.example.com   # only set behind a TLS-terminating reverse proxy
```

Without `public_url`, the `Host` header is attacker-influencable (these are, by design, some of
the few unauthenticated routes) — a malformed or hostile header (embedded path, scheme,
userinfo, or control characters) is rejected with `400 invalid_request` rather than ever echoed
back, and the discovery routes need *some* valid header to respond at all in that case.

**Plain-HTTP LAN risk.** If you bind the daemon to a LAN/Tailscale address *without* a
TLS-terminating reverse proxy (i.e. without `public_url`, talking plain `http://`), bearer
tokens travel in cleartext on that network segment — anyone who can observe the traffic (a
compromised device on the same LAN, a malicious access point) can capture and replay a token
until it's revoked or expires. This is the same risk as any bearer-token API served over plain
HTTP; the mitigations are the usual ones — trust the network segment (Tailscale's own
WireGuard tunnel already encrypts the transport, which is why the Tailscale case above is
lower-risk in practice), or put a TLS-terminating reverse proxy in front (and set
`server.public_url` to match) for any bind that leaves your own machine.

---

## MCP over HTTP

Alongside `/v1`, the daemon also mounts `/mcp` — the same four read-only MCP tools
(`search`, `get_document`, `get_chunks`, `list_stores`) served over the
[MCP Streamable HTTP transport](https://modelcontextprotocol.io/), for connecting a
remote MCP client (e.g. Claude Code on another machine, over Tailscale/LAN). It
inherits this daemon's bind-address trust decision automatically — see
[docs/mcp.md](mcp.md#remote-http-connecting-from-another-machine) for setup and
[specs/05-surfaces.md](../specs/05-surfaces.md) §4.2 for the transport/error-model
details.

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
{"daemon":true,"store_count":1,"source_count":0,"job_count":0}
```

| Field | Type | Description |
|---|---|---|
| `daemon` | bool | Always `true` when the daemon is responding |
| `store_count` | int | Number of stores known to this daemon instance |
| `source_count` | int | Total sources across all stores |
| `job_count` | int | Number of jobs ever created in this daemon session |

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
            "visibility": "private",
            "backend": "libsql",
            "ownership": "runtime"
        }
    ],
    "next_cursor": null,
    "total": 1
}
```

---

### `GET /v1/stores/{name}`

Fetch a single store by name.

```
curl -s http://127.0.0.1:7700/v1/stores/notes
```

```json
{
    "name": "notes",
    "visibility": "private",
    "backend": "libsql",
    "ownership": "runtime"
}
```

Returns `404` with error code `store_not_found` if the store does not exist (see
[Error responses](#error-responses)).

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
[specs/03-config.md](../specs/03-config.md).

---

### `POST /v1/search`

Hybrid search across stores. Returns a ranked citation list over the same data the CLI indexes —
the daemon and the CLI share `<data_dir>/localdb.db`.

**Request body:**

| Field | Type | Required | Description |
|---|---|---|---|
| `query` | string | yes | Natural language search query |
| `stores` | string[] | no | Store names to search; omit to search all stores |
| `limit` | int | no | Maximum results to return (default: 10, max: 100) |
| `cursor` | string | no | Pagination cursor from a previous response |

```
curl -s -X POST http://127.0.0.1:7700/v1/search \
  -H 'Content-Type: application/json' \
  -d '{"query":"hybrid search","limit":1}'
```

```json
{
    "citations": [],
    "total_candidates": 0,
    "next_cursor": null
}
```

Each citation in `citations` follows the canonical Citation shape defined in
[specs/02-domain-model.md](../specs/02-domain-model.md) §6. For a fully-populated example see the
`localdb search --json` output in the CLI reference.

---

### `POST /v1/jobs`

Submit an index job for a store. The daemon processes the job asynchronously; poll
`GET /v1/jobs/{id}` for progress.

**Request body:**

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | string | yes | Job type; currently only `"index"` is supported |
| `store_name` | string | yes | Name of the store to index |

```
curl -s -X POST http://127.0.0.1:7700/v1/jobs \
  -H 'Content-Type: application/json' \
  -d '{"type":"index","store_name":"notes"}'
```

```json
{"id":"01KTVM5XMA59N4WGHNZ80QX9B7","store_id":"notes","scope":{"type":"store"},"state":"pending","stats":{"docs_seen":0,"docs_indexed":0,"docs_deleted":0,"chunks_written":0,"unsupported_format_count":0,"error_count":0},"error":null,"created_at":"2026-06-11T15:17:59Z","started_at":null,"completed_at":null}
```

> If you pass `"store"` instead of `"store_name"` the server returns a 422-style deserialisation
> error: `Failed to deserialize the JSON body into the target type: missing field 'store_name' at
> line 1 column 32`.

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
        "docs_seen": 0,
        "docs_indexed": 0,
        "docs_deleted": 0,
        "chunks_written": 0,
        "unsupported_format_count": 0,
        "error_count": 0
    },
    "error": null,
    "created_at": "2026-06-11T15:17:59Z",
    "started_at": "2026-06-11T15:17:59Z",
    "completed_at": "2026-06-11T15:17:59Z"
}
```

**Job fields:**

| Field | Type | Description |
|---|---|---|
| `id` | string | ULID job identifier |
| `store_id` | string | Store name the job runs against |
| `scope` | object | `{"type":"store"}` for a full-store index |
| `state` | string | `"pending"`, `"running"`, or `"done"` |
| `stats` | object | Running counters (see below) |
| `error` | string\|null | Error message if the job failed |
| `created_at` | string | ISO 8601 timestamp |
| `started_at` | string\|null | ISO 8601 timestamp; null while pending |
| `completed_at` | string\|null | ISO 8601 timestamp; null while running |

**Stats fields:**

| Field | Description |
|---|---|
| `docs_seen` | Files/URLs examined |
| `docs_indexed` | New or changed documents ingested |
| `docs_deleted` | Documents removed because the source file is gone |
| `chunks_written` | Chunks written to the vector store |
| `unsupported_format_count` | Files skipped due to unrecognised format |
| `error_count` | Per-document errors |

SSE progress streaming is on the roadmap (see [specs/06-roadmap.md](../specs/06-roadmap.md) §5);
the job resource shape is designed so SSE adds a new representation without changing the model.

---

## Pagination

List endpoints (`/v1/stores`, `/v1/stores/{name}/sources`) use cursor-based pagination.

| Query parameter | Default | Description |
|---|---|---|
| `cursor` | — | Opaque cursor from a previous response's `next_cursor` |
| `limit` | server default | Maximum items per page |

A `next_cursor` of `null` means the last page has been reached.

---

## Error responses

All errors use the same JSON envelope:

```json
{"code":"store_not_found","message":"store not found: nope"}
```

| Field | Type | Description |
|---|---|---|
| `code` | string | Machine-readable error code (stable API) |
| `message` | string | Human-readable detail |

HTTP status codes follow the shared error taxonomy in [specs/05-surfaces.md](../specs/05-surfaces.md) §5:

| Code | HTTP status | Meaning |
|---|---|---|
| `store_not_found` / `source_not_found` / `document_not_found` / `job_not_found` | 404 | Unknown entity |
| `runtime_state_locked` | 409 | Unified database locked by another process (SQLite `busy_timeout` exceeded) |
| `daemon_running` | 409 | A second daemon was started against the same data dir |
| `daemon_unreachable` | 502 | Daemon socket exists but is not responding |
| `invalid_config` | 422 | Config failed validation |
| `invalid_request` | 400 | Bad request body or arguments |
| `unauthorized` | 401 | Missing or invalid bearer token (once auth is enforced) — carries `WWW-Authenticate: Bearer`, upgraded with `resource_metadata=...` when a base URL can be resolved (see [OAuth discovery](#oauth-discovery--dynamic-client-registration)) |
| `forbidden` | 403 | Authenticated, but insufficient permission for the resource (e.g. a member reading an ungranted store, or any non-admin management route) |
| `unsupported_format` | 422 | Extractor cannot handle the file |
| `provider_unavailable` | 502 | External embedding endpoint down |
| `model_missing` | 503 | Local model not yet downloaded |
| `index_in_progress` | 409 | Conflicting job already running for this scope |
| `internal` | 500 | Bug; response includes a `correlation_id` for log correlation |

---

## Troubleshooting

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
