# Configuration Reference

localdb is configured through a single YAML file. This document covers every field,
platform defaults, config lookup rules, and validation behaviour. For the ownership model
and design rationale, see [specs/03-config.md](../specs/03-config.md).

---

## Locating the config file

localdb resolves its config file in this order (first match wins):

1. `--config <PATH>` flag on any command
2. `LOCALDB_CONFIG` environment variable
3. Platform default (see table below)

```
localdb --config ~/myproject/localdb.yaml status
LOCALDB_CONFIG=~/myproject/localdb.yaml localdb status
```

**Platform default config paths:**

| Platform | Default path |
|----------|-------------|
| macOS    | `~/Library/Application Support/com.localdb.localdb.localdb/config.yaml` |
| Linux    | `$XDG_CONFIG_HOME/localdb/config.yaml` (falls back to `~/.config/localdb/config.yaml`) |

`localdb init` writes the config file to the path you supply with `--config`, or to the
platform default. The config location and the data directory are **independent** — the config
file does not have to live inside the data directory. See `paths.data` below.

> **Note (macOS bundle ID):** on macOS the platform-directories library derives a bundle
> identifier `com.localdb.localdb.localdb` that is used for the config dir, data dir, and
> model cache (the spec table in [specs/03-config.md](../specs/03-config.md) shows the
> intended shorter `localdb/` paths — a known divergence). Override `paths.*` in the config
> for cleaner locations.

---

## Minimal config

`localdb init` generates this file:

```yaml
version: 1
# localdb configuration
# Add stores and sources below.
```

`version: 1` is the only required field. Everything else has defaults.

---

## Full field reference

### `version` (required)

```yaml
version: 1
```

Must be the integer `1`. Any other value (including a missing key) is a validation error.
Breaking schema changes will increment this value; the loader will migrate in-memory and log
a deprecation note without rewriting your file.

---

### `server`

Controls the HTTP daemon started by `localdb serve`.

```yaml
server:
  bind: 127.0.0.1   # interface to listen on (default: 127.0.0.1)
  port: 7700        # port (default: 7700)
```

| Field | Default | Notes |
|-------|---------|-------|
| `bind` | `127.0.0.1` | Set to `0.0.0.0` to listen on all interfaces |
| `port` | `7700` | Set to `0` to let the OS assign an ephemeral port |

> **Experimental:** the HTTP daemon is an early preview. It uses an in-memory store and
> does **not** see data indexed via the CLI. See [Daemon limitations](#daemon-limitations).

---

### `paths`

All path overrides are optional. Platform defaults apply to any key you omit.

```yaml
paths:
  data: ~/localdb/data      # index data, runtime-state DB, unix socket
  models: ~/localdb/models  # embedding model cache
  logs: ~/localdb/logs      # structured log output
```

**Platform defaults:**

| Item | macOS | Linux |
|------|-------|-------|
| Data (indexes, DB, socket) | `~/Library/Application Support/com.localdb.localdb.localdb/data/` | `$XDG_DATA_HOME/localdb/` |
| Model cache | `~/Library/Caches/com.localdb.localdb.localdb/models/` | `$XDG_CACHE_HOME/localdb/models/` |
| Logs | `~/Library/Logs/localdb/` | `$XDG_STATE_HOME/localdb/logs/` |

Tilde expansion (`~`) is supported.

---

### `defaults`

Global indexing policy inherited by every store that does not override it.

```yaml
defaults:
  indexing:
    chunking:
      preset_overrides: {}   # per-source-kind tweaks; see specs/04-search-pipeline.md
    embedding:
      provider: local-onnx   # local-onnx | openai-compatible | perplexity | voyage
      model: pplx-embed-context-v1-0.6b
```

> **Default embedder:** `provider: local-onnx`, `model: pplx-embed-context-v1-0.6b`. The first
> `localdb index` or `localdb search` downloads the model (~706 MB) from the public HuggingFace
> repo `perplexity-ai/pplx-embed-context-v1-0.6b` — no API key required. The model is cached
> under `paths.models` for subsequent runs. Alternative local model: `bge-small-en-v1.5`
> (384-dim, much smaller). Hosted alternatives: `provider: perplexity` (requires API key) or
> `provider: openai-compatible`.

---

### `stores`

A list of YAML-declared stores. Each store entry can carry inline sources.

```yaml
stores:
  - name: handbook
    visibility: private      # private (default) | shared (not yet functional)
    backend: lancedb         # only lancedb is supported in v1
    indexing:                # optional; null = inherit defaults
      chunking:
        preset_overrides: {}
      embedding:
        provider: local-onnx
        model: pplx-embed-context-v1-0.6b
    sources:
      - kind: path
        root: ~/docs/handbook
        include: ["**/*.md", "**/*.pdf"]
        exclude: ["**/node_modules/**"]
        preset: prose        # prose (default) | messages | code
      - kind: url
        url: https://example.com/api-docs
        refresh: 24h
```

**Store fields:**

| Field | Default | Description |
|-------|---------|-------------|
| `name` | — (required) | Unique store name |
| `visibility` | `private` | `private` or `shared` (`shared` not functional in v1) |
| `backend` | `lancedb` | Storage backend; only `lancedb` in v1 |
| `indexing` | `null` (inherit) | Override the global `defaults.indexing` block |
| `sources` | `[]` | Inline source declarations |

**Source fields:**

| Field | Applies to | Default | Description |
|-------|-----------|---------|-------------|
| `kind` | both | — (required) | `path` or `url` |
| `root` | `path` | — (required for `path`) | Filesystem directory to scan |
| `url` | `url` | — (required for `url`) | URL to fetch |
| `include` | both | `["**/*"]` | Glob patterns; files must match at least one |
| `exclude` | both | `[]` | Glob patterns; matching files are skipped |
| `preset` | both | `prose` | Chunking preset: `prose`, `messages`, or `code` |
| `refresh` | `url` | — | Refresh interval (e.g. `24h`, `30m`) |

> **Important — YAML-declared stores cannot be indexed yet.**
>
> Stores declared in the YAML file appear in `localdb store list` with ownership `(yaml)`, but
> `localdb index --store <name>` returns `error: store not found: <name>` (exit 3) because the
> indexer resolves stores only from the runtime-state database.
>
> **Working path today:** create stores at runtime with `localdb store add <name>` and add
> sources with `localdb source add <path> --store <name>`. Runtime stores are fully indexable.
> YAML store indexing will be wired in a future release.

---

### `providers`

Optional external embedding endpoints (OpenAI-compatible API).

```yaml
providers:
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY   # name of the env var that holds the key
```

| Field | Description |
|-------|-------------|
| `name` | Reference name used in `defaults.indexing.embedding.provider` |
| `kind` | Provider type; `openai-compatible` in v1 |
| `base_url` | Base URL of the API endpoint |
| `api_key_env` | Environment variable that holds the API key (never inline the key) |

Secrets must come from the environment. See [specs/03-config.md §6](../specs/03-config.md#6-secrets).

---

## Bootstrap config vs. runtime state

localdb splits configuration ownership into two layers:

| Layer | Owner | Storage | Mutated by |
|-------|-------|---------|-----------|
| **Declarative bootstrap config** | User | YAML file | Text editor only; never rewritten by the machine |
| **Mutable runtime state** | Machine | `<data>/runtime-state.redb` | `store add`, `source add`, HTTP API |

YAML wins for any object it declares (matched by store name / source identity). YAML-owned
objects are read-only via the API. Objects created via CLI or API are runtime-owned and never
appear in YAML. `localdb store list` and `localdb status` show each object's owner
(`(yaml)` or `(runtime)`) so the split is always visible.

For the full ownership design and rationale, see [specs/03-config.md §3](../specs/03-config.md#3-the-two-writer-problem-bootstrap-config-vs-runtime-state).

---

## Validation errors

localdb validates the config file at startup and exits with code `2` on any error. Error
messages include a precise location.

**Unknown top-level key:**
```
error: invalid config: unknown field `bogus_key`, expected one of `version`, `server`,
`paths`, `defaults`, `stores`, `providers` at line 2 column 1
```
Unknown keys are a hard error, not a warning — they catch typos before they silently take
no effect.

**Wrong or missing version:**
```
error: invalid config: unsupported config version 2; only version 1 is supported.
Hint: add `version: 1` at the top of your config file.
```

**Missing required field in a source:**
```
error: invalid config: stores[0].sources[0].root: required for kind 'path'
```

**File is not valid YAML:**
```
error: invalid config: invalid type: map, expected field identifier at line 1 column 2
```

**Missing config file:**
```
error: invalid config: cannot read config file '/path/to/config.yaml':
No such file or directory (os error 2)
```

---

## Daemon limitations

The HTTP daemon (`localdb serve`) is an **experimental preview** in v1. Key limitations:

- The daemon uses an in-memory store. It does not read from or write to the LanceDB indexes
  created by CLI indexing (`localdb index`). Search via the HTTP API returns empty results
  even when CLI-indexed data exists.
- While the daemon is running, CLI commands on the same data directory fail with
  `error: internal error ... cannot open runtime-state DB ... Database already open` (exit 1)
  because the CLI opens the redb database before attempting to route to the daemon.
  Stop the daemon before using CLI commands.
- If the daemon process is killed (not stopped cleanly), the unix socket
  `<data_dir>/daemon.sock` is not cleaned up. Subsequent CLI commands report
  `daemon: running` and searches exit with `error: daemon is unreachable` (exit 5).
  Fix: `rm <data_dir>/daemon.sock`.

---

## Annotated complete example

The following config is a valid, verified example that localdb 0.1.0 will parse without
error. Remember that the `handbook` store will appear in `store list` but cannot be indexed
until YAML store indexing is wired (see note in [`stores`](#stores) above).

```yaml
version: 1

# --- Server (HTTP daemon, experimental) ---
server:
  bind: 127.0.0.1
  port: 7700

# --- Data paths (all optional; platform defaults used for any omitted key) ---
paths:
  data: ~/localdb/data
  models: ~/localdb/models
  logs: ~/localdb/logs

# --- Global indexing defaults (inherited by all stores) ---
# The default local-onnx model is downloaded (~706 MB) on first index/search.
defaults:
  indexing:
    chunking:
      preset_overrides: {}
    embedding:
      provider: local-onnx
      model: pplx-embed-context-v1-0.6b

# --- Declarative stores (visibility: yaml) ---
# These stores are visible in `store list` but CANNOT be indexed yet.
# Use `localdb store add` + `localdb source add` for a fully working store today.
stores:
  - name: handbook
    visibility: private
    backend: lancedb
    sources:
      - kind: path
        root: ~/docs/handbook
        include: ["**/*.md"]
        exclude: ["**/drafts/**"]
        preset: prose
      - kind: url
        url: https://example.com/api-docs
        refresh: 24h

# --- External embedding providers (optional) ---
# Secrets must come from environment variables, never be inlined.
providers:
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY
```

For design decisions behind each section, see [specs/03-config.md](../specs/03-config.md).
