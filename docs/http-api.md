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

### Trust model

The daemon binds `127.0.0.1` by default with **no authentication**. The documented trust boundary
is: anything that can reach the bind address is as trusted as the files themselves. Any bind address
is accepted — binding to a specific non-loopback address (e.g. a LAN or VPN IP) is treated as a
deliberate trust decision and starts silently. Binding to `0.0.0.0` (all interfaces) logs a warning
at startup, since that makes the unauthenticated daemon reachable from any network the machine is
on. See [specs/05-surfaces.md](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md)
§3 for the binding and trust decision.

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

| Field                                              | Type      | Description                                                                                                                                                                 |
| -------------------------------------------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `daemon`                                           | bool      | Always `true` when the daemon is responding                                                                                                                                 |
| `store_count`                                      | int       | Number of stores known to this daemon instance                                                                                                                              |
| `source_count`                                     | int       | Total sources across all stores                                                                                                                                             |
| `job_count`                                        | int       | Number of jobs ever created in this daemon session                                                                                                                          |
| `stores[].document_count` / `stores[].chunk_count` | int\|null | Per-store `RetrievalStore::stats()` figures; `null` if that store's stats call itself failed (a corrupt or mid-migration store must not blank out the report on the others) |
| `database.path`                                    | string    | Path to the shared `localdb.db` file — one physical file backs every store, so this is reported once, not per-store                                                         |
| `database.exists`                                  | bool      | Whether the file exists yet (`false` before the first `store add`/`index`)                                                                                                  |
| `database.size_bytes` / `database.wal_size_bytes`  | int\|null | Bytes in the main file / `-wal` sidecar; `null` if a stat fails                                                                                                             |
| `database.total_size_bytes`                        | int       | `size_bytes + wal_size_bytes` (missing components treated as 0) — what the disk actually has allocated right now                                                            |
| `database.bytes_per_chunk`                         | int\|null | `total_size_bytes` divided by the sum of every store's `chunk_count`; `null` with no chunks                                                                                 |
| `database.largest_tables`                          | array     | Up to 5 `{name, bytes}` rows, the largest on-disk tables via SQLite's `dbstat`, descending; best-effort — empty if `dbstat` querying fails                                  |

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

| Parameter | Type     | Required | Description                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| --------- | -------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `store`   | string[] | no       | Repeatable — scope the lookup to specific stores by name. Omitted: looks the id up across every store (`invalid_request`, 400, if more than one store holds a document with that id). Exactly one: scopes the lookup unambiguously. More than one: resolves unscoped, then checks the found document's store against the given set. Same 0/1/many semantics as CLI `document get -s` (specs/05-surfaces.md §2.2), available in the daemon from day one |

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

### `POST /v1/search`

Hybrid search across stores. Returns a ranked citation list over the same data the CLI indexes — the
daemon and the CLI share `<data_dir>/localdb.db`.

**Request body:**

| Field                              | Type     | Required | Description                                                                          |
| ---------------------------------- | -------- | -------- | ------------------------------------------------------------------------------------ |
| `query`                            | string   | yes      | Natural language search query                                                        |
| `store_filter`                     | string[] | no       | Store names to search; omit or pass `[]` to search all stores                        |
| `limit`                            | int      | no       | Maximum results to return (default: 10; silently clamped to 100, `SEARCH_MAX_LIMIT`) |
| `cursor`                           | string   | no       | Pagination cursor from a previous response                                           |
| `path`                             | string   | no       | Restrict to resources whose URI starts with this prefix. Matched literally.          |
| `mime`                             | string   | no       | Restrict to resources with this exact MIME type. Matched literally.                  |
| `added_after`/`added_before`       | string   | no       | Bound on when the resource was first indexed                                         |
| `updated_after`/`updated_before`   | string   | no       | Bound on when the store last wrote the resource's stored state                       |
| `modified_after`/`modified_before` | string   | no       | Bound on the source's own claimed last-modified time                                 |
| `document_after`/`document_before` | string   | no       | Bound on the document's own claimed date (Dublin Core `dc:date`)                     |

Each date field accepts a full RFC 3339 datetime, a partial date (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`),
or a relative duration (`7d`, `30m`, `2w`) — a duration always resolves to **now minus the
duration**, for either bound (`modified_after: "7d"` means "modified within the last 7 days";
`modified_before: "7d"` means "modified more than 7 days ago"). A resource with no value on a
filtered axis is excluded regardless of the bound (the NULL rule); `document_after`/
`document_before` currently exclude every PDF and feed entry, since that date is populated today
only for HTML, Markdown front matter, and Office documents (issue #251). A malformed filter value is
`invalid_request`, HTTP 400. Multiple filters, of any kind, always AND together.

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
