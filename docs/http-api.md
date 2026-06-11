# HTTP API (`localdb serve`)

> **EXPERIMENTAL — do not rely on this surface for production use.**
>
> The daemon currently serves an **in-memory store**. Data you have indexed with the CLI is not
> visible to the daemon, and search will return empty results. Additionally, CLI commands on the
> same data directory **fail while the daemon is running** because both processes compete for the
> runtime-state database lock. The HTTP API exists today to preview the API surface and exercise
> the endpoint shapes; production behaviour (shared LanceDB store, CLI→daemon routing) is tracked
> in [specs/06-roadmap.md](../specs/06-roadmap.md).
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
`<data_dir>/daemon.sock` so that CLI and MCP processes can detect it.

### Bind address and port

The bind address and port are controlled by the `server` block in `config.yaml`:

```yaml
version: 1
server:
  bind: 127.0.0.1   # default; non-loopback addresses require auth (see Trust model below)
  port: 7700        # default; 0 = OS-assigned
```

Setting `port: 0` asks the OS for an ephemeral port. The assigned port is shown in the announce
line.

### Trust model

The daemon binds `127.0.0.1` by default with **no authentication**. The documented trust boundary
is: anything on this machine that can reach localhost is as trusted as the files themselves. Binding
to a non-loopback address without auth configured is a **refused startup**, not a warning — this is
forward-compatible with the multi-user/home-server mode described in
[specs/06-roadmap.md](../specs/06-roadmap.md) §1, which will arrive together with real auth. See
[specs/05-surfaces.md](../specs/05-surfaces.md) §3 for the binding and trust decision.

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
            "backend": "lancedb",
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
    "backend": "lancedb",
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

> Note: because the daemon uses an in-memory store (see the EXPERIMENTAL callout above), sources
> added with `localdb source add` are not reflected here.

---

### `GET /v1/config`

Returns the parsed configuration as localdb sees it, together with the effective store list (which
merges YAML-declared stores and runtime-created stores).

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
            "ownership": "runtime",
            "visibility": "private",
            "backend": "lancedb"
        }
    ]
}
```

`effective_stores` is the merged view: YAML-declared stores (ownership `"yaml"`) and runtime stores
(ownership `"runtime"`) appear side-by-side. Config schema details are in
[specs/03-config.md](../specs/03-config.md).

---

### `POST /v1/search`

Hybrid search across stores. Returns a ranked citation list. Because the daemon currently uses an
in-memory store, this endpoint returns empty results even when CLI-indexed data exists on disk.

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
| `store_locked` | 409 | Write lock held elsewhere |
| `daemon_running` | 409 | A second daemon was started against the same data dir |
| `daemon_unreachable` | 502 | Daemon socket exists but is not responding |
| `config_readonly` | 409 | Attempted write to a YAML-owned object |
| `invalid_config` | 422 | Config failed validation |
| `invalid_request` | 400 | Bad request body or arguments |
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

### CLI commands fail while the daemon is running

Every CLI command opens the runtime-state database before it can probe the daemon socket. Because
the daemon holds that database open, the CLI exits with an internal error:

```
error: internal error (correlation_id=runtime_state_open): cannot open runtime-state DB at
'<data_dir>/runtime-state.redb': Database already open. Cannot acquire lock.
exit: 1
```

Workaround: stop the daemon before running CLI commands against the same data directory.

### Stale `daemon.sock` after an ungraceful shutdown

If the daemon process is killed (e.g. with `kill <pid>` or a crash), the Unix socket file at
`<data_dir>/daemon.sock` is **not cleaned up**. The CLI will then report the daemon as running and
`localdb search` will exit with:

```
error: daemon is unreachable
exit: 5
```

Fix: remove the stale socket file manually, then CLI commands will fall back to embedded mode.

```
rm <data_dir>/daemon.sock
```

After removal `localdb status` will show `daemon: not running (embedded mode)`.
