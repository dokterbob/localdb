# Configuration Reference

localdb is configured through a single YAML file. This document covers every field, platform
defaults, config lookup rules, and validation behaviour. For the ownership model and design
rationale, see [specs/03-config.md](../specs/03-config.md).

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

| Platform | Default path                                                                           |
| -------- | -------------------------------------------------------------------------------------- |
| macOS    | `~/Library/Application Support/com.localdb.localdb.localdb/config.yaml`                |
| Linux    | `$XDG_CONFIG_HOME/localdb/config.yaml` (falls back to `~/.config/localdb/config.yaml`) |

Whichever path this resolves to, it's created automatically the first time you run a command against
it if nothing is there yet (see [Config is created for you](#config-is-created-for-you) below) —
`localdb init` writes to the same resolved path explicitly, if you'd rather run it up front. The
config location and the data directory are **independent** — the config file does not have to live
inside the data directory. See `paths.data` below.

> **Note (macOS bundle ID):** on macOS the platform-directories library derives a bundle identifier
> `com.localdb.localdb.localdb` that is used for the config dir, data dir, and model cache (the spec
> table in [specs/03-config.md](../specs/03-config.md) shows the intended shorter `localdb/` paths —
> a known divergence). Override `paths.*` in the config for cleaner locations.

---

## Config is created for you

You don't need to run anything before using localdb. The first time you run almost any command —
`search`, `status`, `store add`/`remove`/`list`, `source add`/`list`/`remove`, `index`, `mcp`, or
`serve` — and no config file exists yet at the resolved path, localdb writes one automatically,
along with the data/models/logs directories and a store named `default`. `localdb init` still exists
as an explicit, idempotent step if you'd rather set things up first (see [cli.md](cli.md)); it's
optional, and running it again is always safe.

This only happens the _first_ time: if a config file already exists — even a malformed one — it is
left untouched, and normal validation runs against it as usual (see
[Validation errors](#validation-errors) below).

The generated `config.yaml` is a commented template (`core/src/config/config.template.yaml` in the
source tree), not a bare 3-line stub. It looks like this in full:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/dokterbob/localdb/main/schema/config.schema.json
#
# localdb configuration — generated automatically on first run.
# Full reference: docs/configuration.md
#
# Every key below is optional except `version`. Unknown keys are a hard
# error (typos are caught, not silently ignored).
# `$schema` (above and below) enables autocomplete/inline validation in any
# editor speaking the yaml-language-server protocol (VS Code + "YAML"
# extension, Zed, most LSP-aware editors).

version: 1
$schema: https://raw.githubusercontent.com/dokterbob/localdb/main/schema/config.schema.json

# --- HTTP daemon (`localdb serve`; values below are the defaults) ---
server:
  bind: 127.0.0.1 # loopback only; set to 0.0.0.0 to listen on all interfaces
  port: 7700 # set to 0 to let the OS assign an ephemeral port

# --- Path overrides (optional; platform defaults apply to omitted keys) ---
# paths:
#   data: ~/localdb/data      # unified database (localdb.db)
#   models: ~/localdb/models  # embedding model cache
#   logs: ~/localdb/logs      # structured log output

# --- Global indexing policy: inherited by every store unless overridden.
#     Chunking + embedding + parsers version together; changes trigger
#     reindexing of affected stores. ---
defaults:
  indexing:
    chunking:
      preset_overrides: {} # per-source-kind tweaks; see specs/04-search-pipeline.md
    embedding:
      model: pplx-embed-context-v1-0.6b # context-aware late-chunking, local, MIT-licensed
      provider: local # local (auto CoreML/ONNX) | openai-compatible | perplexity | voyage
      # `local` (default): downloads ~706 MB from HuggingFace on first use,
      #   no API key required. Smaller alternative: model: bge-small-en-v1.5.
      # Hosted: provider: perplexity, model: pplx-embed-context-v1 —
      #   requires a `providers:` entry below with api_key_env set.
    parsers: [pdf, epub, office, html, markdown, plaintext] # tried in order; order is load-bearing
# --- External embedding providers (optional; OpenAI-compatible API) ---
# providers:
#   - name: my-ollama              # referenced by defaults.indexing.embedding.provider
#     kind: openai-compatible      # openai-compatible | perplexity | voyage
#     base_url: http://localhost:11434/v1
#     api_key_env: OLLAMA_KEY      # env var holding the key — secrets are never inlined
```

`version: 1` is the only required field; every other key shown above is already at its default
value, spelled out for discoverability rather than left implicit. See
[specs/03-config.md §8](../specs/03-config.md#8-config-file-generation-and-schema) for the full
generation and schema design.

---

## Editor integration (`$schema`)

The generated config carries a schema reference in two forms, both pointing at the same URL:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/dokterbob/localdb/main/schema/config.schema.json
---
$schema: https://raw.githubusercontent.com/dokterbob/localdb/main/schema/config.schema.json
```

- The **modeline** (first line, `# yaml-language-server: $schema=...`) is read by the
  `yaml-language-server`-based ecosystem: VS Code's "YAML" extension (redhat.vscode-yaml), Zed, and
  most other LSP-backed editors. It works even for tooling that doesn't otherwise inspect a YAML
  document's own keys.
- The **`$schema:` key** is the same URL as a regular top-level property, for tooling that reads a
  document's own `$schema` field instead of (or in addition to) the modeline convention. localdb
  accepts and ignores this key semantically — it exists purely for editor tooling.

Either form gets you autocomplete, inline validation, and hover documentation (pulled from the same
Rust doc comments this reference page is written from) as you edit `config.yaml`. The schema itself
is published at `https://raw.githubusercontent.com/dokterbob/localdb/main/schema/config.schema.json`
— a live reference to `main`, not a per-release pin — and versions itself on the config's own
`version` field, so an editor rejects an unsupported `version` in the same place the loader would.

---

## Full field reference

### `version` (required)

```yaml
version: 1
```

Must be the integer `1`. Any other value (including a missing key) is a validation error. Breaking
schema changes will increment this value; the loader will migrate in-memory and log a deprecation
note without rewriting your file.

---

### `server`

Controls the HTTP daemon started by `localdb serve`.

```yaml
server:
  bind: 127.0.0.1 # interface to listen on (default: 127.0.0.1)
  port: 7700 # port (default: 7700)
```

| Field  | Default     | Notes                                             |
| ------ | ----------- | ------------------------------------------------- |
| `bind` | `127.0.0.1` | Set to `0.0.0.0` to listen on all interfaces      |
| `port` | `7700`      | Set to `0` to let the OS assign an ephemeral port |

> **Experimental:** the HTTP daemon is an early preview. It opens the same unified database
> (`<data_dir>/localdb.db`) as the CLI, so CLI-indexed data IS visible, and `POST /v1/jobs` runs
> real ingestion through an async job queue. See [Daemon limitations](#daemon-limitations).

---

### `paths`

All path overrides are optional. Platform defaults apply to any key you omit.

```yaml
paths:
  data: ~/localdb/data # unified database (localdb.db), unix socket
  models: ~/localdb/models # embedding model cache
  logs: ~/localdb/logs # structured log output
```

**Platform defaults:**

| Item                            | macOS                                                             | Linux                             |
| ------------------------------- | ----------------------------------------------------------------- | --------------------------------- |
| Data (unified database, socket) | `~/Library/Application Support/com.localdb.localdb.localdb/data/` | `$XDG_DATA_HOME/localdb/`         |
| Model cache                     | `~/Library/Caches/com.localdb.localdb.localdb/models/`            | `$XDG_CACHE_HOME/localdb/models/` |
| Logs                            | `~/Library/Logs/localdb/`                                         | `$XDG_STATE_HOME/localdb/logs/`   |

Tilde expansion (`~`) is supported.

---

### `defaults`

Global indexing policy inherited by every store that does not override it.

```yaml
defaults:
  indexing:
    chunking:
      preset_overrides: {} # per-source-kind tweaks; see specs/04-search-pipeline.md
    embedding:
      provider: local # local | local-coreml | local-onnx | openai-compatible | perplexity | voyage
      model: pplx-embed-context-v1-0.6b
```

> **Default embedder:** `provider: local`, `model: pplx-embed-context-v1-0.6b`. `local` auto-picks a
> backend: CoreML (ANE/GPU) on Apple Silicon macOS builds, ONNX (CPU) everywhere else — see
> [specs/03-config.md §7](../specs/03-config.md#7-local-embedding-provider-selection-local--local-coreml--local-onnx)
> to force one explicitly with `local-coreml`/`local-onnx`. The first `localdb index` or
> `localdb search` downloads the model (~706 MB) from the public HuggingFace repo
> `perplexity-ai/pplx-embed-context-v1-0.6b` — no API key required. The model is cached under
> `paths.models` for subsequent runs. Alternative local model: `bge-small-en-v1.5` (384-dim, much
> smaller). Hosted alternatives: `provider: perplexity` (requires API key) or
> `provider: openai-compatible`.

---

### `providers`

Optional external embedding endpoints (OpenAI-compatible API).

```yaml
providers:
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY # name of the env var that holds the key
```

| Field         | Description                                                        |
| ------------- | ------------------------------------------------------------------ |
| `name`        | Reference name used in `defaults.indexing.embedding.provider`      |
| `kind`        | Provider type; `openai-compatible` in v1                           |
| `base_url`    | Base URL of the API endpoint                                       |
| `api_key_env` | Environment variable that holds the API key (never inline the key) |

Secrets must come from the environment. See
[specs/03-config.md §6](../specs/03-config.md#6-secrets).

---

## Config vs. runtime state

The YAML config file covers static settings: paths, server bind, embedding defaults, and providers.
Stores and sources are managed exclusively via the CLI (`localdb store add`, `localdb source add`)
or HTTP API — no store declarations in YAML are supported. The unified database
(`<data_dir>/localdb.db`) is the single source of truth for all stores and sources.

For full details, see [specs/03-config.md §3](../specs/03-config.md#3-store-and-source-management).

---

## Validation errors

localdb validates the config file at startup and exits with code `2` on any error. Error messages
include a precise location.

**Unknown top-level key:**

```
error: invalid config: unknown field `bogus_key`, expected one of `version`, `server`,
`paths`, `defaults`, `providers` at line 2 column 1
```

Unknown keys are a hard error, not a warning — they catch typos before they silently take no effect.

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

- **`POST /v1/jobs` runs real ingestion**, through an async, single-worker job queue with a
  per-store in-flight guard (a second submission for a store already running gets
  `index_in_progress`, 409). `localdb index` submits a job and attaches to its live progress
  (`GET /v1/jobs/{id}/events`, SSE, falling back to polling) whenever a daemon is running, with
  output identical to embedded mode; concurrent writers (CLI and daemon alike) serialise via SQLite
  WAL + `busy_timeout=5000`. Daemon-side reads (`/v1/search`, `/v1/documents/{id}`, `/v1/status`)
  see the same data.
- **Stale socket after a crash.** If the daemon process is killed (not stopped cleanly), the unix
  socket `<data_dir>/daemon.sock` is not cleaned up. Subsequent CLI commands report
  `daemon: running` and searches exit with `error: daemon is unreachable` (exit 5). Fix:
  `rm <data_dir>/daemon.sock`.

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
# The default local model is downloaded (~706 MB) on first index/search.
defaults:
  indexing:
    chunking:
      preset_overrides: {}
    embedding:
      provider: local
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
