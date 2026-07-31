# Follow-up issues from the pdf_oxide swap + page citations (#87/#65/#103/#45)

These were identified while implementing the parser swap and page-number
plumbing. They are intentionally **not** on that branch. File each as its own
issue (drafts below are ready to paste). `gh` was unauthenticated in the dev
environment, so they could not be filed automatically — run `gh auth login`
(e.g. type `! gh auth login` in the session) and create them, or paste the
drafts into the tracker.

---

## 1. Upstream (pdf_oxide): relax the `ort` exact pin `=2.0.0-rc.11`

**Repo:** yfedoseev/pdf_oxide · **Type:** dependency-compat

pdf_oxide's `ocr` / `gpu` features pin `ort = "=2.0.0-rc.11"`. Our `embed` crate
pins `ort = "=2.0.0-rc.12"` (issue #133 setup: load-dynamic, default-features
off). If we ever enable pdf_oxide's `ocr` feature in the same build, the two
exact pins conflict and the workspace won't resolve. Request: widen the `ort`
constraint (e.g. `>=2.0.0-rc.11, <2.1` or track our rc.12) so downstreams that
already depend on `ort` can share one version.

Not blocking us today — we ship pdf_oxide with **default features only, no
`ocr`**, so `ort` is absent from the PDF path entirely.

## 2. Upstream (pdf_oxide): expose execution-provider / session-options config for OCR

**Repo:** yfedoseev/pdf_oxide · **Type:** feature-request

The `ocr` feature runs PaddleOCR through `ort` but exposes no
execution-provider / session-options API, so OCR is CPU-only through the public
API — no CoreML (ANE/GPU) or CUDA. Request: a way to pass EP/session options
(mirroring `ort`'s `SessionBuilder`) so callers can select hardware
acceleration. Blocks efficient OCR when we pick up #43.

## 3. Thread a real `extractor_version` into the reindex skip-check (cross-ref #47)

**Repo:** dokterbob/localdb · **Type:** correctness / tech-debt

`extractor_version` is dead code: hardcoded `"1"` in both ingestors and in
`store-libsql/src/tenant/write.rs` (the resource upsert), and never read by the
skip-check at `core/src/ingestion.rs` (which keys only on `content_hash`).

The pdf_oxide swap self-triggers reindexing because extracted text — and thus
`compute_blocks_hash` — changes. But a parser change that produces
byte-identical text for some document would leave it stale (and page-less) with
no re-extraction. Thread a real per-parser `extractor_version` from the parser
through the ingestors into the store and into the skip-check, so a parser
version bump forces re-extraction deterministically and yields a natural
"N PDFs re-extracted" log line. See known-gaps §8 in `docs/architecture.md`.

## 4. Fix workspace license metadata: `MIT` vs AGPL-3.0 `LICENSE`

**Repo:** dokterbob/localdb · **Type:** metadata / one-line PR

The workspace `Cargo.toml` declares `license = "MIT"` (line ~19) while the repo
`LICENSE` file is AGPL-3.0. Reconcile — most likely
`license = "AGPL-3.0-or-later"` in the workspace `[workspace.package]`. Separate
one-line PR, not on the PDF branch.

## 5. #157 quality gate: `is_indexable_text` filter in `index_resource`

**Repo:** dokterbob/localdb · **Type:** quality (existing issue #157)

Add an `is_indexable_text` filter over `chunk_outputs` in
`core/src/ingestion.rs::index_resource`, right after the `chunk_blocks` call and
before the `is_empty` check, to drop mojibake / non-indexable chunks. Calibrate
against the Phase A mojibake fixtures (`extract/tests/fixtures/malformed/cid_no_tounicode.pdf`)
and the corpus CJK cases. The corpus test's `forbid_substrings` (U+FFFD) guard is
the regression net.

## 6. #43 OCR: scanned-PDF support behind a Cargo feature

**Repo:** dokterbob/localdb · **Type:** feature (existing issue #43)

Scanned PDFs still hard-`Err` (`UnsupportedFormat`). When picked up, OCR slots
into the same `extract_pdf` seam: `detect_page_type`/`classify_page` →
`extract_text_with_ocr`, behind a Cargo feature, with load-dynamic `ort`
matching `embed`'s #133 pattern (and hardware accel gated on follow-up #2 above).
Open design question for that ticket: does `extract` gain an `ort` dependency, or
does OCR live in a separate crate?

---

## Release-note callout (for the next release notes, not an issue)

> **PDFs re-index automatically.** The PDF text extractor was replaced
> (`pdf-extract` → `pdf_oxide`): PDFs now extract to structured Markdown, no
> longer crash on malformed content streams, and stop emitting mojibake for
> CMap-less fonts. Search citations from PDFs now carry a page number
> (`(p.N)`). Because the extracted text changes, every PDF gets a new content
> hash and re-indexes on your next `localdb index` — a one-time re-embedding
> cost. (Edge case: a PDF whose new extraction is byte-identical to the old keeps
> its old hash and stays page-less until re-added; see known-gaps §8.)
