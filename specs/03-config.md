# Spec 03 — Configuration

> Status: accepted draft, revised 2026-06-30.

## 1. Shape

**Decision:** YAML config file, declarative, user-owned. Schema (illustrative, normative for
structure):

```yaml
version: 1

server:
  bind: 127.0.0.1        # local-only by default; see 05-surfaces.md §3
  port: 7700

paths:                    # all optional; platform defaults in §4
  data: ~                 # index data, socket
  models: ~               # embedding model cache
  logs: ~

defaults:                 # global indexing policy; stores inherit
  indexing:
    chunking:
      preset_overrides: {}     # per-source-kind tweaks, see §2
    embedding:
      model: pplx-embed-context-v1-0.6b  # see 04-search-pipeline.md §4
      provider: local                    # local | local-coreml | local-onnx |
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
    parsers: [pdf, epub, office, html, markdown, plaintext]  # tried in order, first match wins;
                                               #   ids: pdf|epub|office|html|markdown|plaintext;
                                               #   order is load-bearing (affects policy_version, §2)

providers:                # optional external endpoints, OpenAI-compatible
  - name: my-ollama
    kind: openai-compatible
    base_url: http://localhost:11434/v1
    api_key_env: OLLAMA_KEY    # secrets come from env/keychain, never inline (§6)
```

## 2. Indexing policy: one unit per store

**Decision:** `indexing: {chunking, embedding, parsers}` is configured **as a single unit, per
store**, with global defaults and per-source-kind presets (`prose`: split by headings;
`messages`: thread/turn windows; `code`: structural). Defaults live in
[04-search-pipeline.md](04-search-pipeline.md) §3.

**Rationale:** under contextualized/late chunking the chunker and embedder are coupled — chunk
boundaries are an input to the embedding pass. Changing either invalidates the other's output, so
they version together: any change to a store's effective `indexing` policy changes the
`policy_version` hash and **triggers a reindex of that store**
([04-search-pipeline.md](04-search-pipeline.md) §4). **Rejected:** independent global chunking
and embedding knobs — allows silently incoherent combinations and unclear reindex semantics.

`parsers` is an ordered list of parser IDs tried in sequence; the first parser to return a
document wins (chain of responsibility). The valid IDs are `pdf`, `epub`, `office`, `html`,
`markdown`, and `plaintext`; any unknown ID is a hard error at config load (consistent with §5 strict
unknown-key rejection). Order is load-bearing — placing `plaintext` before `html` would cause
`.html` files to be parsed as plain text — and **parser order is part of the `policy_version`
hash** (unlike `chunking`/`embedding` keys, which are hashed order-independently; see
[04-search-pipeline.md](04-search-pipeline.md) §4). Reordering the list therefore triggers a
store reindex.

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

**Interactive setup:** when a source is added for an ingestor kind that requires configuration
(API tokens, auth flows), the CLI uses the ingestor's `ConfigField` descriptors to prompt the
user interactively. Non-interactive creation (HTTP API, `--non-interactive` flag) requires all
required fields to be provided upfront.

**File and URL ingestors** use the existing `SourceSpec` shape (root/include/exclude for paths,
url/refresh for URLs) and require no additional interactive setup.

## 4. File locations

| Item | macOS | Linux |
|---|---|---|
| Config | `~/Library/Application Support/localdb/config.yaml` | `$XDG_CONFIG_HOME/localdb/config.yaml` |
| Data (unified database, socket) | `~/Library/Application Support/localdb/data/` | `$XDG_DATA_HOME/localdb/` |
| Model cache | `~/Library/Caches/localdb/models/` | `$XDG_CACHE_HOME/localdb/models/` |
| Logs | `~/Library/Logs/localdb/` | `$XDG_STATE_HOME/localdb/logs/` |

Unix socket: `<data>/daemon.sock`
([01-architecture.md](01-architecture.md) §3). `--config` / `LOCALDB_CONFIG` override the config
path; `paths.*` in config override the rest.

## 5. Validation, unknown keys, versioning

- **Validation:** fail fast at load with path-precise errors (`stores[0].sources[1].refresh:
  invalid duration`). Surfaces map this to `invalid_config` ([05-surfaces.md](05-surfaces.md) §5).
- **Unknown keys:** hard error, not a warning. Catches typos (`chunking` vs `chunkng`) — the cost
  of strictness is low while there is no plugin ecosystem. Revisit if third-party extensions appear.
- **Versioning:** top-level `version: 1` required. Breaking schema changes bump the version;
  the loader migrates old versions **in memory** and logs a deprecation note — it never rewrites
  the user's file (§3). Unversioned files are rejected with a hint.

## 6. Secrets

Never inline in YAML. Provider credentials are referenced by environment variable name
(`api_key_env`) in MVP; OS keychain integration is a roadmap item
([06-roadmap.md](06-roadmap.md) §5).

Ingestor credentials (API tokens, phone auth sessions) are stored in the `credentials` table
in the unified database, keyed by `(ingestor_kind, source_id, key)`. The values are stored
encrypted (details TBD per ingestor). Interactive credential setup is handled by the ingestor's
setup flow in `cli`, not by YAML config.

## 7. Local embedding provider selection (`local` / `local-coreml` / `local-onnx`)

The default local model `pplx-embed-context-v1-0.6b` can run on two backends; three `provider`
values select between them:

| Provider | Backend | Behavior |
|---|---|---|
| `local` (default) | auto | On Apple Silicon macOS built with the `local-coreml` cargo feature, when the CoreML bundle is loadable, runs on the **CoreML (ANE/GPU)** backend. Otherwise — non-macOS, feature not built, or a CoreML load failure — transparently falls back to **ONNX (CPU)**. |
| `local-coreml` | CoreML (ANE/GPU) | Forces CoreML. **Hard error** if unavailable (non-macOS, feature off, or load failure) — there is no fallback. |
| `local-onnx` | ONNX (CPU) | Forces ONNX. Existing `local-onnx` configs keep working unchanged. |

The CoreML backend is macOS-only and gated behind the opt-in `local-coreml` cargo feature; default
builds are ONNX-only and unaffected. Building `--features local-coreml` requires **Rust ≥ 1.85**.

**Index interchangeability.** Both backends share `model_id = pplx-embed-context-v1-0.6b`, are
1024-dim, and emit binary-quantized vectors (`VectorEncoding::Binary`). Only the sign survives
binarization; measured cosine parity is ~0.995–0.9995 and per-dimension sign agreement ~98–99%
(the ~1–2% of flips are near-zero dimensions that round to a different int8 sign under fp16). An
index built by one backend is queryable by the other — switching providers requires **no reindex**
and does not change the `policy_version` ([04-search-pipeline.md](04-search-pipeline.md) §4).
