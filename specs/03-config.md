# Spec 03 — Configuration

> Status: accepted draft, revised 2026-08-12.

## 1. Shape

**Decision:** YAML config file, declarative, user-owned. Schema (illustrative, normative for
structure):

```yaml
version: 1

server:
  bind: 127.0.0.1 # local-only by default; see 05-surfaces.md §3
  port: 7700

paths: # all optional; platform defaults in §4
  data: ~ # index data, socket
  models: ~ # embedding model cache
  logs: ~

defaults: # global indexing policy; stores inherit
  indexing:
    chunking:
      preset_overrides: {} # per-source-kind tweaks, see §2
    embedding:
      model: pplx-embed-context-v1-0.6b # see 04-search-pipeline.md §4
      provider:
        local # local | local-coreml | local-onnx |
        #   openai-compatible | perplexity | voyage
      # pplx-embed-context-v1-0.6b (default): context-aware late-chunking, runs locally,
      #   MIT-licensed public repo — no API key or token required. Downloads ~706 MB
      #   (quantized ONNX) from HuggingFace on first use.
      # Local provider variants (see §7):
      #   local (default): AUTO — on Apple Silicon macOS built with the local-coreml
      #     feature, uses the CoreML ANE/GPU backend; otherwise falls back to ONNX (CPU).
      #   local-coreml: force CoreML; hard error if unavailable (no fallback).
      #   local-onnx: force ONNX (CPU). Existing local-onnx configs keep working unchanged.
      # Local alternatives: model: pplx-embed-v1-0.6b (1024-dim, non-context, gated — needs
      #   HF_TOKEN, ~2.4 GB); model: bge-small-en-v1.5 (384-dim, no creds).
      # Hosted alternative: provider: perplexity, model: pplx-embed-context-v1
      #   (requires providers: entry with kind: perplexity and api_key_env set).
    parsers:
      [pdf, epub, office, html, markdown, plaintext] # tried in order, first match wins;
      #   ids: pdf|epub|office|html|markdown|plaintext;
      #   order is load-bearing (affects policy_version, §2)

providers: # optional external endpoints, OpenAI-compatible
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY # secrets come from env/keychain, never inline (§6)
```

## 2. Indexing policy: one unit per store

**Decision:** `indexing: {chunking, embedding, parsers}` is configured **as a single unit, per
store**, with global defaults and per-source-kind presets (`prose`: split by headings; `messages`:
thread/turn windows; `code`: structural). Defaults live in
[04-search-pipeline.md](04-search-pipeline.md) §3.

**Rationale:** under contextualized/late chunking the chunker and embedder are coupled — chunk
boundaries are an input to the embedding pass. Changing either invalidates the other's output, so
they version together: any change to a store's effective `indexing` policy changes the
`policy_version` hash and **triggers a reindex of that store**
([04-search-pipeline.md](04-search-pipeline.md) §4). **Rejected:** independent global chunking and
embedding knobs — allows silently incoherent combinations and unclear reindex semantics.

`parsers` is an ordered list of parser IDs tried in sequence; the first parser to return a document
wins (chain of responsibility). The valid IDs are `pdf`, `epub`, `office`, `html`, `markdown`, and
`plaintext`; any unknown ID is a hard error at config load (consistent with §5 strict unknown-key
rejection). Order is load-bearing — placing `plaintext` before `html` would cause `.html` files to
be parsed as plain text — and **parser order is part of the `policy_version` hash** (unlike
`chunking`/`embedding` keys, which are hashed order-independently; see
[04-search-pipeline.md](04-search-pipeline.md) §4). Reordering the list therefore triggers a store
reindex.

## 3. Store and source management

Stores and sources are managed exclusively via the CLI (`localdb store add`, `localdb source add`)
or HTTP API. No YAML store declarations are supported. The unified database
(`<data_dir>/localdb.db`) is the single source of truth for all stores and sources.

### Ingestor configuration

Each source references an `ingestor_kind` and carries ingestor-specific configuration in
`config_json`. The `IngestorConfig` trait in `core` describes the typed configuration for each
ingestor kind via `ConfigField` descriptors:

```rust
struct ConfigField {
    key: &'static str,
    label: &'static str,
    description: &'static str,
    required: bool,
    secret: bool,       // stored in credentials table, not config_json
    field_type: ConfigFieldType,  // String, Path, Url, Integer, Boolean, Choice
    default: Option<String>,
}
```

**Interactive setup:** when a source is added for an ingestor kind that requires configuration (API
tokens, auth flows), the CLI uses the ingestor's `ConfigField` descriptors to prompt the user
interactively. Non-interactive creation (HTTP API, `--non-interactive` flag) requires all required
fields to be provided upfront.

**File and URL ingestors** use the existing `SourceSpec` shape (root/include/exclude for paths,
url/refresh for URLs) and require no additional interactive setup.

**Feed ingestor** (`ingestor_kind: feed`) likewise needs no interactive setup — it's added via the
CLI or HTTP API like any other source, never YAML. `config_json` carries `max_entries` and
`fetch_full_content`; `refresh_interval_secs` lives in the existing `refresh` column, same as `url`.
Illustrative shape (persisted as JSON; shown here as YAML for readability, consistent with §1):

```yaml
# localdb source add https://blog.example.com/feed.xml --store notes --kind feed --max-entries 50
max_entries: 50 # cap on entries considered per fetch, applied after date-sort; 0 rejected
fetch_full_content:
  true # default: discovery mode — fetch each entry's linked page as its own
  #   Resource. false: single-document mode — the whole feed becomes one
  #   Resource assembled from entry summaries. See 02-domain-model.md §2.
```

See [05-surfaces.md](05-surfaces.md) §2.2 / §3 for the CLI flags and HTTP body shape.

## 4. File locations

| Item                            | macOS                                               | Linux                                  |
| ------------------------------- | --------------------------------------------------- | -------------------------------------- |
| Config                          | `~/Library/Application Support/localdb/config.yaml` | `$XDG_CONFIG_HOME/localdb/config.yaml` |
| Data (unified database, socket) | `~/Library/Application Support/localdb/data/`       | `$XDG_DATA_HOME/localdb/`              |
| Model cache                     | `~/Library/Caches/localdb/models/`                  | `$XDG_CACHE_HOME/localdb/models/`      |
| Logs                            | `~/Library/Logs/localdb/`                           | `$XDG_STATE_HOME/localdb/logs/`        |

Unix socket: `<data>/daemon.sock`; daemon discovery URL: `<data>/daemon.url`
([01-architecture.md](01-architecture.md) §3). `--config` / `LOCALDB_CONFIG` override the config
path; `paths.*` in config override the rest.

## 5. Validation, unknown keys, versioning

- **Validation:** fail fast at load with path-precise errors
  (`stores[0].sources[1].refresh: invalid duration`). Surfaces map this to `invalid_config`
  ([05-surfaces.md](05-surfaces.md) §5).
- **Unknown keys:** hard error, not a warning. Catches typos (`chunking` vs `chunkng`) — the cost of
  strictness is low while there is no plugin ecosystem. Revisit if third-party extensions appear.
- **Versioning:** top-level `version: 1` required. Breaking schema changes bump the version; the
  loader migrates old versions **in memory** and logs a deprecation note — it never rewrites the
  user's file (§3). Unversioned files are rejected with a hint.
- **Scaffolding is orthogonal to validation:** first-run config generation (§8) only ever writes a
  config file where the resolved path had none — it is never triggered by, and never softens the
  outcome of, loading a config file that already exists. A present-but-malformed file always goes
  through the same strict/lenient validation path and the same `invalid_config` / exit 2 outcome it
  always did; scaffolding cannot mask a real validation failure.

## 6. Secrets

Never inline in YAML. Provider credentials are referenced by environment variable name
(`api_key_env`) in MVP; OS keychain integration is a roadmap item ([06-roadmap.md](06-roadmap.md)
§5).

Ingestor credentials (API tokens, phone auth sessions) are stored in the `credentials` table in the
unified database, keyed by `(ingestor_kind, source_id, key)`. The values are stored encrypted
(details TBD per ingestor). Interactive credential setup is handled by the ingestor's setup flow in
`cli`, not by YAML config.

## 7. Local embedding provider selection (`local` / `local-coreml` / `local-onnx`)

The default local model `pplx-embed-context-v1-0.6b` can run on two backends; three `provider`
values select between them:

| Provider          | Backend          | Behavior                                                                                                                                                                                                                                                           |
| ----------------- | ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `local` (default) | auto             | On Apple Silicon macOS built with the `local-coreml` cargo feature, when the CoreML bundle is loadable, runs on the **CoreML (ANE/GPU)** backend. Otherwise — non-macOS, feature not built, or a CoreML load failure — transparently falls back to **ONNX (CPU)**. |
| `local-coreml`    | CoreML (ANE/GPU) | Forces CoreML. **Hard error** if unavailable (non-macOS, feature off, or load failure) — there is no fallback.                                                                                                                                                     |
| `local-onnx`      | ONNX (CPU)       | Forces ONNX. Existing `local-onnx` configs keep working unchanged.                                                                                                                                                                                                 |

The CoreML backend is macOS-only and gated behind the opt-in `local-coreml` cargo feature; default
builds are ONNX-only and unaffected. Building `--features local-coreml` requires **Rust ≥ 1.85** —
subsumed in practice by the workspace floor of **1.88**, which `pdf_oxide` sets for every build.

**Index interchangeability.** Both backends share `model_id = pplx-embed-context-v1-0.6b`, are
1024-dim, and emit binary-quantized vectors (`VectorEncoding::Binary`). Only the sign survives
binarization; measured cosine parity is ~0.995–0.9995 and per-dimension sign agreement ~98–99% (the
~1–2% of flips are near-zero dimensions that round to a different int8 sign under fp16). An index
built by one backend is queryable by the other — switching providers requires **no reindex** and
does not change the `policy_version` ([04-search-pipeline.md](04-search-pipeline.md) §4).

## 8. Config file generation and schema

### First-run scaffolding

A genuinely absent config file is no longer a hard stop. Every CLI command that loads config —
`search`, `status`, `store add`/`remove`/`list`, `source add`/`list`/`remove`, `index`, `mcp`, and
`serve` — creates one on first use, transparently, before doing its own work, whether it goes
through the strict load path (`store add`/`remove`, `source add`/`list`/`remove`, `index`, `mcp`,
`serve`) or the lenient, fallback-to-platform-defaults read path (`search`, `status`, `store list`).
`localdb init` ([05-surfaces.md](05-surfaces.md) §2) remains available as an explicit, idempotent
alias — useful for setting things up before running anything else, but no longer required.

On a genuine first run (the resolved config path does not exist at all), scaffolding:

1. creates the config file's parent directory, plus `paths.data`/`paths.models`/`paths.logs`
   (platform defaults, per §4);
2. writes the handwritten, commented default template (`core/src/config/config.template.yaml`) to
   the config path, atomically (below);
3. creates a store named `default`, idempotently — a name lookup first, so a concurrent racer never
   produces two.

**Existence, not validity, gates scaffolding.** The check is a plain existence test, never "try to
parse, scaffold on failure": a config file that is present but malformed is left completely
untouched, and the caller's own strict/lenient load reports the same parse error it always did (exit
2, `invalid_config`, §5). Scaffolding only ever fires into a true void.

**`db status`/`migrate`/`downgrade`/`vacuum` deliberately do not scaffold**
([05-surfaces.md](05-surfaces.md) §2.5). They exist to inspect or repair an _existing_ store's
schema; scaffolding underneath them would let a schema-repair command silently paper over "there is
no store here yet" instead of surfacing it.

**Atomic write.** The template is written to a uniquely-named temp file
(`config.yaml.tmp-<pid>-<ulid>`) in the same directory as the target, then hard-linked into place; a
concurrent racer that loses the link race (`AlreadyExists`) is treated as success rather than an
error, since every racer is writing byte-identical content. The temp file is removed in every case
(success, lost race, or hard failure). This guarantees a concurrent reader — in particular the
daemon's config file watcher — never observes partial content.

**An explicit `--config` with a missing parent directory is still a hard failure, exit 2, on every
surface that scaffolds** — not only `init`. Previously the CLI's lenient path silently fell back to
an unrelated platform-default config when an explicit `--config` pointed into a missing directory;
it now fails the same way `init` always did.

### Editor schema reference (`$schema`)

`RawConfig` accepts an optional `$schema` key (serde-renamed from `schema`); it is validated as a
string if present but is otherwise semantically inert — nothing in the loader reads it. The
generated template writes two forms of the same URL:

- a `# yaml-language-server: $schema=<url>` modeline as the file's first line;
- a literal `$schema: <url>` key alongside `version: 1`.

Both point at the same stable URL:
`https://raw.githubusercontent.com/dokterbob/localdb/main/schema/config.schema.json`
(`localdb_core::config::jsonschema::SCHEMA_URL`) — a live main-branch reference, not a per-release
pin, so an editor always validates against the schema matching the newest released loader behavior.
The `$schema` key form works for any tooling that reads a YAML document's own `$schema` property;
the modeline form is read by the `yaml-language-server`-based ecosystem (VS Code's "YAML" extension,
Zed, and most other LSP-backed editors), including before the document body itself is parsed.

### The router schema and versioning

`generate_router_schema()` (`core/src/config/jsonschema.rs`) emits one draft 2020-12 JSON Schema
document, published at `$id: <SCHEMA_URL>`, that dispatches on the config's top-level `version`
field:

```
version: integer, required
if version == 1: $ref #/$defs/v1
else: false
```

`$defs.v1` is the full `schemars`-derived schema of `RawConfig` — `additionalProperties: false`
(mirroring `#[serde(deny_unknown_fields)]`), field descriptions taken verbatim from Rust doc
comments, nested struct schemas flattened into the same `$defs` map with an inserted `v1_` prefix
(so `ServerConfig`'s def becomes `$defs.v1_ServerConfig`) to keep a future version's defs from
colliding with `v1`'s.

**Deprecation policy:** a future `version: 2` gets its own `$defs.v2` and an added `if`/`then`/
`else` branch nested inside the existing `else`. Dropping support for an old version _is_ removing
its branch — the router schema then rejects that version's configs outright (`else: false`), in the
same place, and for the same reason, the loader itself hard-errors on an unsupported `version`.
Schema and loader are meant to fail in lockstep: an editor should never accept a config version the
binary refuses to load, or vice versa.

### Regenerating the committed artifact

The router schema is committed at `schema/config.schema.json` (repo root) and regenerated with:

```sh
cargo run -p localdb -- internal print-schema > schema/config.schema.json
```

`internal print-schema` is a hidden subcommand (never shown in `--help`) — pure, offline codegen
with no config load and no daemon probe. `core/tests/config_schema_drift.rs` byte-compares the
committed file against a fresh `generate_router_schema()` call and fails if anyone edits the schema
generator, or a `RawConfig`-reachable doc comment that feeds it, without regenerating the artifact.

### Keeping the template honest

`core/src/config/config.template.yaml` is handwritten prose, not generated, so
`core/src/config/template.rs`'s test suite pins it against the same sources of truth it documents:
the rendered template must parse to the identical `RawConfig` a minimal `version: 1` config produces
(the `$schema` field aside), must mention every `$defs.v1*` schema property by name (live or
commented-out), and must itself validate against `generate_router_schema()`'s output.
