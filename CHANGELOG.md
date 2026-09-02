# Changelog

All notable changes to this project are documented in this file.

The format follows [Common Changelog](https://common-changelog.org).

## [0.1.1] - 2026-09-02

### Changed

- T171: Register feed sources with the refresh scheduler ([#311](https://github.com/dokterbob/localdb/pull/311)) ([`1d0ca08`](https://github.com/dokterbob/localdb/commit/1d0ca088d68b5d61adf7ed64afbaab1244ec85b0))
- Share one ONNX model cache across embed tests; retry transient model-download failures ([#276](https://github.com/dokterbob/localdb/pull/276)) ([#290](https://github.com/dokterbob/localdb/pull/290)) ([`d7fb480`](https://github.com/dokterbob/localdb/commit/d7fb480c1dd4b3db7cfcc8ddf426b704bbf776b5))
- T250: spec the four date axes; file ingestor added_at from ingestion clock ([#278](https://github.com/dokterbob/localdb/pull/278)) ([`db8d33f`](https://github.com/dokterbob/localdb/commit/db8d33f782d4891a506a218fa99a3be2625ca634))
- Pin the Rust toolchain, and fix the lints the newer stable flags ([#257](https://github.com/dokterbob/localdb/pull/257)) ([`4ae121b`](https://github.com/dokterbob/localdb/commit/4ae121b1601978237694f5bf4c307b0bd16a585d))
- Extract transient state from the repo: delete issue drafts, design-question and branch-coordination files ([#271](https://github.com/dokterbob/localdb/pull/271)) ([#294](https://github.com/dokterbob/localdb/pull/294)) ([`800be28`](https://github.com/dokterbob/localdb/commit/800be28356a9b0f954d74ab5a9f81151947e26d7))
- T250: metadata-aware incremental skip; metadata-only store update ([#176](https://github.com/dokterbob/localdb/pull/176)) ([#281](https://github.com/dokterbob/localdb/pull/281)) ([`e29a095`](https://github.com/dokterbob/localdb/commit/e29a0952dfab075b0980d4bdd24c7622d618d235))
- T250: extract dc:date + date_source for office, HTML, and markdown front matter ([#282](https://github.com/dokterbob/localdb/pull/282)) ([`c7289aa`](https://github.com/dokterbob/localdb/commit/c7289aafc63411922e4f1498a3828d354824285e))
- T250: source-claimed modified_at is nullable end-to-end ([#283](https://github.com/dokterbob/localdb/pull/283)) ([#286](https://github.com/dokterbob/localdb/pull/286)) ([`7c31f51`](https://github.com/dokterbob/localdb/commit/7c31f5108e107d0c84326627aa4e609ca98654d0))
- T247: bind search filter values as SQL parameters ([#255](https://github.com/dokterbob/localdb/pull/255)) ([#295](https://github.com/dokterbob/localdb/pull/295)) ([`74420d1`](https://github.com/dokterbob/localdb/commit/74420d13eb0ee96b113465d757b6ee0613f56801))
- T247: replace hand-rolled date arithmetic with chrono ([#297](https://github.com/dokterbob/localdb/pull/297)) ([`8ca547d`](https://github.com/dokterbob/localdb/commit/8ca547de420d333f01d8fa46d1e68c307f716e93))
- T247: Add DateAxis so all four date axes are filterable ([#298](https://github.com/dokterbob/localdb/pull/298)) ([`a95bd1c`](https://github.com/dokterbob/localdb/commit/a95bd1ccb1fe804fa6bb59d6a7726d6edb9c274e))
- T171: Report the validators a 304 response carries ([#308](https://github.com/dokterbob/localdb/pull/308)) ([`aaa6fd2`](https://github.com/dokterbob/localdb/commit/aaa6fd2f4e25daef0a14b920b659b6de9aa1433a))
- T171: Capture and replay conditional-GET validators ([#309](https://github.com/dokterbob/localdb/pull/309)) ([`e117b35`](https://github.com/dokterbob/localdb/commit/e117b35ade89324cf0becffd8e084ddbea113826))
- T171: Make the feed document's own fetch conditional ([#310](https://github.com/dokterbob/localdb/pull/310)) ([`bf57941`](https://github.com/dokterbob/localdb/commit/bf57941c9f3cddb545b1d442283c4819eeedaf23))
- T171: Prune confirmed-gone feed entries with a bounded liveness sweep ([#312](https://github.com/dokterbob/localdb/pull/312)) ([`b7ba669`](https://github.com/dokterbob/localdb/commit/b7ba66939e1482b11430de96a95fb2e13254e27b))
- Make `localdb init` honest and optional ([#225](https://github.com/dokterbob/localdb/pull/225)) ([#256](https://github.com/dokterbob/localdb/pull/256)) ([`b574ef4`](https://github.com/dokterbob/localdb/commit/b574ef424b18ce507ec3681c09a8da7987607f4d))
- Print --json error envelopes to stdout instead of stderr ([#263](https://github.com/dokterbob/localdb/pull/263)) ([`c56b759`](https://github.com/dokterbob/localdb/commit/c56b759168b9d4b2a985dbafa8b732bccf505223))
- T250: migration v7 index_updated_at; persist real modified_at; preserve added_at on policy reindex ([#279](https://github.com/dokterbob/localdb/pull/279)) ([`10bbba0`](https://github.com/dokterbob/localdb/commit/10bbba03b8307647f0a14af93f260cf52652232d))
- T250: populate date_original/date_parsed + external_id/external_etag; expose dates on document surfaces ([#280](https://github.com/dokterbob/localdb/pull/280)) ([`4ef6411`](https://github.com/dokterbob/localdb/commit/4ef6411ae7332e25803914bff737d8034b1feb1a))
- Capture child output in the concurrent store-list test ([#181](https://github.com/dokterbob/localdb/pull/181)) ([#291](https://github.com/dokterbob/localdb/pull/291)) ([`de56ad3`](https://github.com/dokterbob/localdb/commit/de56ad30c3af68552727a04871c9963fba85341e))
- T247: fix search flags typed after the query being swallowed as text ([#296](https://github.com/dokterbob/localdb/pull/296)) ([`d134d4e`](https://github.com/dokterbob/localdb/commit/d134d4ec92cb1790d31a997d2e684d47b62e9940))
- T247: Add search filters to the CLI, HTTP, and MCP surfaces ([#247](https://github.com/dokterbob/localdb/pull/247)) ([#303](https://github.com/dokterbob/localdb/pull/303)) ([`8a5b439`](https://github.com/dokterbob/localdb/commit/8a5b4395bfb4565b582baff366fa76dbcbf0b96d))
- T171: Add schema v8 — conditional-GET and liveness columns ([#307](https://github.com/dokterbob/localdb/pull/307)) ([`3c1eb1d`](https://github.com/dokterbob/localdb/commit/3c1eb1d94215986bae5cb31eb86ad9724ccb6b40))


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
