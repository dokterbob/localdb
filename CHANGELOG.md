# Changelog

All notable changes to this project are documented in this file.

The format follows [Common Changelog](https://common-changelog.org).

## [0.2.0] - 2026-09-06

Indexing gets dramatically cheaper on unchanged content: conditional GETs, a feed entry recheck
gate, and metadata-aware skips mean a re-index run now touches the network and the embedder only
for documents that actually changed. Search grows metadata filters, including all four date axes.

**Upgrading:** this release adds two schema migrations (v7: nullable source-claimed `modified_at`
plus `index_updated_at`; v8: conditional-GET validators and `last_checked_at`). Run
`localdb db migrate` once — localdb never migrates on open and refuses with a hint until you do.

### Added

- Search filters, identical across CLI, HTTP, and MCP: `--path`, `--mime`, and `-after`/`-before`
  bounds on all four date axes (`added`, `updated`, `modified`, `document`)
  ([#247](https://github.com/dokterbob/localdb/pull/247),
  [#298](https://github.com/dokterbob/localdb/pull/298),
  [#303](https://github.com/dokterbob/localdb/pull/303))
- Conditional GET for URL and feed fetches: `ETag`/`Last-Modified` validators are captured and
  replayed, so an unchanged document costs a `304` instead of a refetch and re-extraction
  ([#307](https://github.com/dokterbob/localdb/pull/307),
  [#308](https://github.com/dokterbob/localdb/pull/308),
  [#309](https://github.com/dokterbob/localdb/pull/309),
  [#310](https://github.com/dokterbob/localdb/pull/310))
- A recheck gate on feed entries: an entry whose feed claim is unchanged and whose last successful
  origin contact is within the recheck floor (the source’s refresh interval, floored at 24h) is
  skipped without any HTTP request, reported as "N rechecks deferred"; a quiet `304`-answering feed
  still drains overdue entries through a bounded due-entry revisit
  ([#331](https://github.com/dokterbob/localdb/pull/331),
  [#333](https://github.com/dokterbob/localdb/pull/333),
  [#334](https://github.com/dokterbob/localdb/pull/334),
  [#335](https://github.com/dokterbob/localdb/pull/335))
- `localdb index --refetch`: bypass the recheck gate and feed validators for one run, forcing a
  full re-check even when nothing looks stale
  ([#336](https://github.com/dokterbob/localdb/pull/336))
- Feed sources register with the daemon’s refresh scheduler, so a running daemon re-polls them on
  their configured interval ([#311](https://github.com/dokterbob/localdb/pull/311))
- A bounded liveness sweep under `index --delete` probes feed entries that scrolled off the feed
  window and prunes the ones the origin confirms gone (404/410)
  ([#312](https://github.com/dokterbob/localdb/pull/312))
- Document dates as first-class metadata: `dc:date` (with provenance) extracted from PDF, Office,
  HTML, EPUB and Markdown front matter, exposed on every document surface alongside the other
  three axes ([#280](https://github.com/dokterbob/localdb/pull/280),
  [#282](https://github.com/dokterbob/localdb/pull/282))

### Changed

- Incremental indexing is metadata-aware: a metadata-only change (title, authors, dates) updates
  the stored document without refetching, re-extracting, or re-embedding its content
  ([#176](https://github.com/dokterbob/localdb/pull/176),
  [#279](https://github.com/dokterbob/localdb/pull/279),
  [#281](https://github.com/dokterbob/localdb/pull/281))
- A source that claims no modification time no longer gets a fabricated one: `modified_at` is
  nullable end-to-end ([#283](https://github.com/dokterbob/localdb/pull/283),
  [#286](https://github.com/dokterbob/localdb/pull/286))
- `localdb init` is optional and honest: every command scaffolds implicitly on first use, and
  `init` reports exactly what it did ([#225](https://github.com/dokterbob/localdb/pull/225),
  [#256](https://github.com/dokterbob/localdb/pull/256))

### Fixed

- Search filter values are bound as SQL parameters instead of being interpolated
  ([#255](https://github.com/dokterbob/localdb/pull/255),
  [#295](https://github.com/dokterbob/localdb/pull/295))
- Search flags typed after the query words are parsed as flags instead of being swallowed into
  the query text ([#296](https://github.com/dokterbob/localdb/pull/296))
- `--json` error envelopes print to stdout, where the rest of the JSON output goes
  ([#263](https://github.com/dokterbob/localdb/pull/263))


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
