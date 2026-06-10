# Rust Backend Scope and Open Questions

This document captures the current architecture decisions for a FOSS local-first semantic search and knowledge server, plus the main areas that still need specification.

## Current decisions

### Product shape

The product will start as a **Rust-first backend** with three primary surfaces: a web API, a CLI, and an MCP server.[cite:96][cite:99] The main UI in the first shipped version will be a web interface, while a native Swift frontend for macOS is explicitly deferred until later.[cite:88]

The backend is intended to be usable both as a standalone application and as a set of layers that other applications can reuse. That layered approach also supports splitting the project into multiple repositories over time.[cite:88]

### Runtime model

The backend will be **Rust-only by default**. Python-based extraction or enrichment is out of scope for the initial default runtime, partly to avoid dependency sprawl and packaging complexity associated with Python-heavy production runtimes.[cite:88][cite:89]

Local LLM inference is not part of the core backend process. Embeddings should default to internal local models executed via ONNX-based runtimes, while LLMs and potentially external embedding providers can be accessed over OpenAI-compatible APIs so model loading and inference stay in separate processes.[cite:109][cite:107][cite:111]

### Storage and retrieval

The default vector and hybrid retrieval backend will be **on-disk Qdrant** stored in a standard application directory. Qdrant is a strong fit because it supports dense, sparse, and hybrid retrieval, supports local and remote deployment modes, and integrates well with OpenAI-compatible embedding workflows.[cite:104][cite:102][cite:98]

Hybrid retrieval is a core requirement from day one. Dense plus sparse retrieval should be the default search mode rather than an optional advanced feature.[cite:104][cite:102]

### Configuration

The backend will use a **YAML configuration file** by default. The configuration should remain understandable and minimal in the common case, but structured enough to support future extension for external providers, remote stores, extraction options, and deployment modes.[cite:88]

### Interfaces

The backend will expose:

- A **web API** for application and UI integration.
- A **CLI** for local operations, administration, indexing, and scripting.
- An **MCP server** for agent integration across MCP-capable tools and clients.[cite:96][cite:99]

These interfaces should sit on top of the same backend core rather than duplicating logic in separate implementations.[cite:88]

### UI direction

The first user-facing management UI will be web-based. A later macOS application can act as a wrapper or native frontend around the same backend, likely talking to the backend over the web API or another internal API boundary.[cite:88]

## Implied architecture

Based on the decisions above, the architecture currently points toward the following layers.

| Layer | Role | Status |
|---|---|---|
| Core Rust domain | Collections, documents, chunks, indexing jobs, search, metadata, citations | Decided |
| Retrieval adapter | Qdrant local by default, remote Qdrant later | Decided for local default; remote still open |
| Model adapter | ONNX local embeddings by default; OpenAI-compatible endpoints optional | Decided |
| API surface | Web API, CLI, MCP on one shared core | Decided |
| UI | Web UI first; Swift wrapper/app later | Decided |
| Extraction pipeline | Rust-only default for now; broader fallback strategy still open | Partially open |

## Sensible defaults

The current shape implies a strong “works locally on first install” path:

- Local on-disk Qdrant as the default store.[cite:104]
- Internal ONNX-based embedding models as the default embedding path.[cite:109][cite:112]
- OpenAI-compatible endpoints accepted for embeddings and/or LLM use when users want external providers or separate inference servers.[cite:107][cite:110][cite:111]
- Web UI as the initial admin and search interface.[cite:88]
- YAML configuration as the main persisted config surface.[cite:88]

This keeps the first version operational without requiring a separately managed vector database, external model hosting, or a native frontend.[cite:88][cite:104]

## Open questions and items still to spec

### 1. Canonical repo layout

The layered approach suggests either a monorepo with multiple packages or several separate repositories. That decision is still open and matters for release cadence, reuse, contributor onboarding, and API stability.[cite:88]

Open points:

- Monorepo vs multi-repo.
- Which components are public libraries versus product-only crates.
- Whether MCP, CLI, and web API live in one binary or separate deliverables.

### 2. Document extraction strategy

The Rust-only default backend is decided, but the extraction stack is not. There is no single mature Rust equivalent to Unstructured covering all document types at the same quality level, so a format-by-format strategy still needs to be defined.[cite:124][cite:127][cite:129]

Open points:

- Which Rust-native extractors are used for PDF, DOCX, PPTX, XLSX, HTML, Markdown, and images.[cite:124][cite:125][cite:133][cite:139]
- Whether OCR is in scope for v1, and if so whether Tesseract-based Rust bindings are sufficient.[cite:135][cite:138]
- Whether scanned PDFs and complex tables are deferred, weakly supported, or handled by an optional future sidecar.
- What the canonical normalized document schema looks like.

### 3. Embedding defaults

The requirement for internal ONNX-based embeddings is decided, but the actual model choices are still open. FastEmbed is a natural candidate because it supports lightweight ONNX-based dense and sparse embedding generation.[cite:109][cite:112]

Open points:

- Which dense embedding model is the default.
- Which sparse model is the default.
- Whether reranking is included in v1.
- Whether models are bundled, downloaded on first run, or managed as optional packs.
- How model selection is expressed in YAML and surfaced in the UI.

### 4. External provider contract

OpenAI-compatible external APIs are in scope, but the exact provider abstraction is not yet specified.[cite:107][cite:110][cite:111]

Open points:

- Whether embeddings and LLM endpoints use a unified provider abstraction or separate ones.
- Whether Ollama-compatible, LiteLLM-style, and generic OpenAI-compatible endpoints are all supported explicitly or via one generic adapter.
- Timeout, retry, batching, and health-check behavior.
- Authentication and secret storage model.

### 5. Search semantics and ranking

Hybrid search is a firm requirement, but the exact retrieval pipeline remains open.[cite:104][cite:102]

Open points:

- Default fusion method for dense and sparse scores.
- Chunking policy and chunk overlap defaults.
- Metadata filtering design.
- Whether query rewriting, reranking, or semantic answer generation is part of the backend core or delegated to higher layers.
- Whether URL and image search are first-class query modes in v1.

### 6. API design

The presence of a web API is decided, but not its style or scope.

Open points:

- REST vs gRPC vs both.
- Which surface is considered canonical for external integrators.
- Auth model for local-only mode versus remote/server mode.
- Long-running job model for indexing and enrichment.
- Pagination, streaming, and event subscription approach.

### 7. MCP design

The backend will expose MCP, but the MCP surface still needs to be designed carefully.[cite:96][cite:99]

Open points:

- Which tools/resources/prompts are exposed.
- Whether MCP is read-only initially or includes mutating operations like add source, reindex, delete collection, or update metadata.
- How citations and source spans are represented through MCP.
- Permission model for local MCP clients.

### 8. CLI scope

A CLI exists in the plan, but the command model is still open.

Open points:

- Whether the CLI is a thin client for the daemon or also supports standalone local commands.
- How configuration and profiles are selected.
- Whether the CLI supports interactive search/browse or is strictly command-oriented.

### 9. Web UI scope

The web UI is the first main management UI, but its initial scope is not defined yet.[cite:88]

Open points:

- Whether v1 web UI is admin-only or also a full search/browse experience.
- Collection management, chunk inspection, result citations, source previews, and saved searches.
- Whether web UI authentication is needed in local-only mode.

### 10. Filesystem and data layout

Qdrant will live in a standard application directory, but the full on-disk layout is not yet specified.[cite:104]

Open points:

- Config location.
- Data location.
- Cache/model location.
- Logs and telemetry location.
- Backup/export/import strategy.

### 11. Deployment modes

The local-first default is clear, but supported deployment modes still need explicit definition.[cite:88]

Open points:

- Single-user local desktop mode.
- Home-server mode.
- Linux headless mode.
- Reverse-proxied shared mode.
- Whether remote Qdrant is v1 or v1.5.

### 12. Packaging and installation

The initial intent is simple installation, but the exact packaging plan for the Rust backend plus web UI is still open.[cite:88]

Open points:

- Homebrew formula strategy.
- Standalone tarball/binary releases.
- macOS launchd service support.
- Linux systemd service support.
- Asset bundling for the web UI.

### 13. Entities and graph

Entities and graph-aware search were identified as highly valuable, but they are not part of the fixed initial scope yet. Graph construction can be layered in later, and there are existing patterns combining vector search with graph layers such as Qdrant plus Neo4j.[cite:97][cite:103]

Open points:

- Are entities in v1 metadata only, or actual first-class indexed objects.
- Is graph extraction delayed until after baseline retrieval quality is strong.
- Does graph live in the same store, a side store, or an optional external graph backend.

### 14. Observability and operations

The backend is meant to act as a reusable server component, so operational concerns need to be specified early.[cite:88]

Open points:

- Structured logs and log levels.
- Metrics and tracing.
- Health/readiness endpoints.
- Index job visibility.
- Error taxonomy across API, CLI, and MCP.

### 15. Security model

The product begins local-first, but security boundaries still need to be specified before remote/server deployment.

Open points:

- Local-only trust assumptions.
- API auth in remote mode.
- MCP exposure policy.
- Secret storage for external providers.
- Multi-user versus single-user assumptions.

## Suggested immediate next spec documents

The most useful follow-up specs would likely be:

1. **Architecture and crate boundaries** — core crates, adapter crates, binaries, and repo structure.
2. **Canonical document model** — files, blocks, chunks, metadata, citations, entities.
3. **Config schema** — YAML structure, defaults, validation, migration policy.
4. **Search pipeline spec** — ingestion, chunking, dense/sparse embeddings, hybrid ranking, optional reranking.
5. **API/MCP/CLI surface spec** — commands, endpoints, tools, auth, and error handling.
6. **Packaging and deployment spec** — local mode, server mode, paths, Homebrew, service management.

## Current summary

The agreed direction is now fairly concrete: a **Rust-only backend first**, with **web API + CLI + MCP**, **web UI first**, **Qdrant on disk by default**, **YAML config**, and **local ONNX embedding models as the default retrieval path**, while **LLMs and optional external providers sit behind OpenAI-compatible APIs in separate processes**.[cite:88][cite:104][cite:109][cite:107][cite:111]

The biggest open areas are not the broad architecture anymore; they are the detailed specs for extraction, model defaults, API shape, packaging, filesystem layout, and the roadmap for entities/graph and remote deployment.[cite:124][cite:127][cite:129]
