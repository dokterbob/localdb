# Spec 03 — Configuration

> Status: accepted draft, 2026-06-10.

## 1. Shape

**Decision:** YAML config file, declarative, user-owned. Schema (illustrative, normative for
structure):

```yaml
version: 1

server:
  bind: 127.0.0.1        # local-only by default; see 05-surfaces.md §3
  port: 7700

paths:                    # all optional; platform defaults in §4
  data: ~                 # index data, locks, socket
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
    parsers: [pdf, html, markdown, plaintext]  # tried in order, first match wins;
                                               #   ids: pdf|html|markdown|plaintext; order is
                                               #   load-bearing (affects policy_version, §2)

stores:
  - name: notes
    visibility: private   # private | shared (shared non-functional in MVP)
    backend: lancedb
    indexing: ~           # null = inherit defaults; or override {chunking, embedding, parsers}
    sources:
      - kind: path
        root: ~/Documents/notes
        include: ["**/*.md", "**/*.pdf"]
        exclude: ["**/node_modules/**"]
        preset: prose     # prose | messages | code  (source-kind preset, §2)
      - kind: url
        url: https://example.com/handbook
        refresh: 24h

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
document wins (chain of responsibility). The four valid IDs are `pdf`, `html`, `markdown`, and
`plaintext`; any unknown ID is a hard error at config load (consistent with §5 strict
unknown-key rejection). Order is load-bearing — placing `plaintext` before `html` would cause
`.html` files to be parsed as plain text — and **parser order is part of the `policy_version`
hash** (unlike `chunking`/`embedding` keys, which are hashed order-independently; see
[04-search-pipeline.md](04-search-pipeline.md) §4). Reordering the list therefore triggers a
store reindex.

## 3. The two-writer problem: bootstrap config vs. runtime state

The config must be editable both as a file and via API/GUI. Two writers on one YAML file means
lost comments, lost ordering, and race conditions.

**Decision: split the model.**

- **Declarative bootstrap config (YAML):** owned by the user, read at startup and watched for
  changes by the daemon. **Never rewritten by the machine.** Defines: paths, server bind,
  defaults, providers, and any stores/sources the user chooses to manage declaratively.
- **Mutable runtime state (DB-backed, in the data dir):** stores/sources/policy edits made via
  API, CLI mutation commands, or future GUI land here, not in YAML.

**Precedence:** YAML wins for any object it declares. An object is *YAML-owned* if it appears in
the file (matched by store name / source identity); YAML-owned objects are read-only via the API
(error `config_readonly`, [05-surfaces.md](05-surfaces.md) §5). Objects created via API/GUI are
*runtime-owned* and never appear in YAML. `localdb status` reports each object's owner so the
split is always inspectable.

**Rejected:** round-trip YAML editing (machine rewrites the file) — loses comments/formatting,
fights concurrent human edits, and turns a config file into a database with worse durability;
"API writes a second YAML overlay file" — two files with merge semantics is the same problem with
more states.

## 4. File locations

| Item | macOS | Linux |
|---|---|---|
| Config | `~/Library/Application Support/localdb/config.yaml` | `$XDG_CONFIG_HOME/localdb/config.yaml` |
| Data (indexes, runtime-state DB, lock, socket) | `~/Library/Application Support/localdb/data/` | `$XDG_DATA_HOME/localdb/` |
| Model cache | `~/Library/Caches/localdb/models/` | `$XDG_CACHE_HOME/localdb/models/` |
| Logs | `~/Library/Logs/localdb/` | `$XDG_STATE_HOME/localdb/logs/` |

Unix socket: `<data>/daemon.sock`; write lock: `<data>/.write.lock`
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
