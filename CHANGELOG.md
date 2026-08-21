# Changelog

All notable changes to this project are documented in this file.

The format follows [Common Changelog](https://common-changelog.org).

## [0.1.1] - 2026-08-21

### Changed

- Pin the Rust toolchain, and fix the lints the newer stable flags ([#257](https://github.com/dokterbob/localdb/pull/257)) ([`4ae121b`](https://github.com/dokterbob/localdb/commit/4ae121b1601978237694f5bf4c307b0bd16a585d))
- Make `localdb init` honest and optional ([#225](https://github.com/dokterbob/localdb/pull/225)) ([#256](https://github.com/dokterbob/localdb/pull/256)) ([`b574ef4`](https://github.com/dokterbob/localdb/commit/b574ef424b18ce507ec3681c09a8da7987607f4d))


## [0.1.0] - 2026-08-18

_First release._

localdb is a local-first knowledge server: one binary that indexes your files and URLs into a
local store and answers hybrid search queries with verifiable citations — from the terminal or
from any MCP-capable AI assistant. No Python, no Docker, no cloud, no API key; nothing needs to
be running for search.

### Added

- Hybrid search: BM25 (FTS5) + dense vectors (DiskANN, binary-quantized) fused with RRF,
  returning structured citations — URI, heading path, exact snippet, byte span, content hash and
  Dublin Core document metadata ([#92](https://github.com/dokterbob/localdb/pull/92),
  [#202](https://github.com/dokterbob/localdb/pull/202))
- In-process extraction to Markdown for plain text, HTML, PDF (with page-number citations),
  Office documents (DOCX/PPTX/XLSX/XLS/CSV) and EPUB
  ([#151](https://github.com/dokterbob/localdb/pull/151),
  [#169](https://github.com/dokterbob/localdb/pull/169))
- Sources: local files and directories, URLs, and Atom/RSS feeds with per-source refresh
  intervals ([#170](https://github.com/dokterbob/localdb/pull/170))
- Local embeddings by default — `pplx-embed-context-v1-0.6b`, a context-aware late-chunking
  model (ONNX on CPU; CoreML on the Apple Silicon Neural Engine automatically) — with hosted
  alternatives (OpenAI-compatible, Perplexity, Voyage)
- MCP server (`localdb mcp`) with `search`, `get_document`, `get_chunks` and `list_stores`
  tools, over stdio or HTTP ([#145](https://github.com/dokterbob/localdb/pull/145))
- CLI: `init`, `add`, `store`, `source`, `document`, `index`, `search`, `status`, `db`, `job`,
  `completions` — human-readable output with `--json` everywhere, stable exit codes, and
  multi-store scoping via a repeatable `--store` filter
  ([#203](https://github.com/dokterbob/localdb/pull/203),
  [#231](https://github.com/dokterbob/localdb/pull/231))
- Experimental HTTP daemon (`localdb serve`): REST API under `/v1`, shared unified database with
  the CLI, async ingestion job queue with live SSE progress, cancellation and a configurable
  worker pool, plus file watching ([#212](https://github.com/dokterbob/localdb/pull/212),
  [#226](https://github.com/dokterbob/localdb/pull/226),
  [#227](https://github.com/dokterbob/localdb/pull/227))
- Explicit, reversible schema migrations (`localdb db migrate` / `downgrade` / `vacuum`)
  ([#152](https://github.com/dokterbob/localdb/pull/152))
- Implicit first-run scaffolding and a versioned, JSON-Schema-validated YAML config
  ([#215](https://github.com/dokterbob/localdb/pull/215))
- Distribution: Homebrew tap (`brew install dokterbob/localdb/localdb`) with shell completions
  and opt-in `brew services` daemon, shell installer, and signed/attested tarballs for macOS
  (Apple Silicon, CoreML built in) and Linux (x86_64 + arm64, glibc ≥ 2.35)
  ([#232](https://github.com/dokterbob/localdb/pull/232),
  [#233](https://github.com/dokterbob/localdb/pull/233))
