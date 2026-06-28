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

> **Experimental:** the HTTP daemon is an early preview. It opens the same unified database
> (`<data_dir>/localdb.db`) as the CLI, so CLI-indexed data IS visible. The one open limitation
> is that ingestion via `POST /v1/jobs` is a no-op. See [Daemon limitations](#daemon-limitations).

---

### `paths`

All path overrides are optional. Platform defaults apply to any key you omit.

```yaml
paths:
  data: ~/localdb/data      # unified database (localdb.db), unix socket
  models: ~/localdb/models  # embedding model cache
  logs: ~/localdb/logs      # structured log output
```

**Platform defaults:**

| Item | macOS | Linux |
|------|-------|-------|
| Data (unified database, socket) | `~/Library/Application Support/com.localdb.localdb.localdb/data/` | `$XDG_DATA_HOME/localdb/` |
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

## Config vs. runtime state

The YAML config file covers static settings: paths, server bind, embedding defaults, and providers. Stores and sources are managed exclusively via the CLI (`localdb store add`, `localdb source add`) or HTTP API — no store declarations in YAML are supported. The unified database (`<data_dir>/localdb.db`) is the single source of truth for all stores and sources.

For full details, see [specs/03-config.md §3](../specs/03-config.md#3-store-and-source-management).

---

## Validation errors

localdb validates the config file at startup and exits with code `2` on any error. Error
messages include a precise location.

**Unknown top-level key:**
```
error: invalid config: unknown field `bogus_key`, expected one of `version`, `server`,
`paths`, `defaults`, `providers` at line 2 column 1
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

- **Ingestion via `POST /v1/jobs` is a no-op.** The endpoint accepts the request, transitions
  the job state machine (`pending → done`), and reports `chunks_written: 0`. To actually index,
  run `localdb index` from the CLI — this works while the daemon is running because both
  processes share the same unified database and concurrent writers serialise via SQLite WAL
  + `busy_timeout=5000`. Daemon-side reads (`/v1/search`, `/v1/documents/{id}`, `/v1/status`)
  see CLI-indexed data correctly.
- **Stale socket after a crash.** If the daemon process is killed (not stopped cleanly), the
  unix socket `<data_dir>/daemon.sock` is not cleaned up. Subsequent CLI commands report
  `daemon: running` and searches exit with `error: daemon is unreachable` (exit 5).
  Fix: `rm <data_dir>/daemon.sock`.

---

## Annotated complete example

The following config is a valid, verified example that localdb 0.1.0 will parse without error.

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

# --- External embedding providers (optional) ---
# Secrets must come from environment variables, never be inlined.
providers:
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY
```

For design decisions behind each section, see [specs/03-config.md](../specs/03-config.md).
