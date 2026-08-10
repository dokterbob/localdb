# Follow-up issues from the pdf_oxide swap + page citations (#87/#65/#103/#45)

These were identified while implementing the parser swap and page-number
plumbing. They are intentionally **not** on that branch. File each as its own
issue (drafts below are ready to paste). `gh` was unauthenticated when the
drafts were written; it has since been authenticated and the extraction-quality
follow-ups **have been filed** (see the index below). The remaining drafts in
§1, §2, §3, §4, §5 and §6 are still unfiled.

## Filed (extraction-quality pass)

| Report | Filed as |
|---|---|
| `strip_running_headers_footers` deletes body text | [pdf_oxide#1022](https://github.com/yfedoseev/pdf_oxide/issues/1022) (§2a) |
| Dehyphenation unreachable from `to_markdown()` | [pdf_oxide#1023](https://github.com/yfedoseev/pdf_oxide/issues/1023) (§2b) |
| Monospace→code and heading over-detection | [pdf_oxide#1024](https://github.com/yfedoseev/pdf_oxide/issues/1024) (§2c) |
| Mid-word chunk splits | [#191](https://github.com/dokterbob/localdb/issues/191) |
| Chunk-span contiguity undecided | [#192](https://github.com/dokterbob/localdb/issues/192) |
| Spurious intra-word spaces (`"consid ered"`) | [#193](https://github.com/dokterbob/localdb/issues/193) |
| `page_count` / `word_count` never populated | [#194](https://github.com/dokterbob/localdb/issues/194) |

Extended rather than duplicated, as comments on existing issues: **#157**
(gibberish-chunk calibration cases), **#159** (back-matter boilerplate, not just
headings), **#95** (near-duplicates across formats are *not* covered by
`content_hash` dedup), **#97** (EPUB `heading_path` still empty), **#94**
(widened to the `Citation.snippet` boundary-snap contract), **#43** (an `OCR/`
path is no evidence OCR succeeded).

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

## 2a. Upstream (pdf_oxide): `strip_running_headers_footers` deletes body text

**Repo:** yfedoseev/pdf_oxide · **Type:** bug · **Severity:** data loss

`ConversionOptions::strip_running_headers_footers: true` silently removes words
from the middle of body paragraphs in multi-column documents. We enabled it,
measured the damage, and reverted to `include_artifacts: false` alone.

**Root cause** (0.3.77, `src/document.rs`):

- `repeated_running_head_foot(0.6)` collects candidate strings from individual
  **`TextSpan`s** (glyph runs), not from assembled lines, keeping any whose
  normalized text is longer than 3 chars and occurs in the head/foot band on
  ≥60% of pages.
- `in_head_foot_band` is the top or bottom **15%** of the page. In a
  two-column journal the first line of *every column* falls inside that band
  and is genuine body text.
- `is_running_head_foot` then drops **any** span in the band whose normalized
  text is in that set — on every page, regardless of context.

So a short, frequent phrase-fragment becomes a global deletion rule.

**Reproduction** — `extract/tests/fixtures/corpus/plos-compbio-two-column.pdf`
(CC BY 4.0, committed in this repo), per-page `to_markdown` with only
`strip_running_headers_footers: true` flipped:

```
"gene function—how individual genes contribute to"
  → "gene function—how  contribute to"
"and performed a statistical analysis, the results of which"
  → "and  analysis, the results of which"
"As representatives of the international consortium that produces the GO,
 we show how the apparent evidence"
  → "As representatives  consortium that produces the  apparent evidence"
"knowledge obtained in mouse and human experimental systems was incorrectly
 interpreted as a disagreement"
  → "knowledge obtained  experimental systems was incorrectly  disagreement"
```

Net −874 chars (−2.2%) with no indication anything was lost.

**Suggested fixes:** match assembled *lines* rather than spans; require the
candidate to be the full band line, not a substring of body text; narrow the
band (15% of a page is a lot of body text in a dense layout); and require a
much higher minimum length than 3 chars.

### Residual risk accepted with `include_artifacts: false`

We now drop `/Artifact`-tagged spans (`spans.retain(|s| s.artifact_type.is_none())`, dropping
`Pagination`/`Layout`/`Page`/`Background`). That is spec-correct per ISO 32000-1 §14.8.2.2.1 and
is the safe half of the running-header fix — but it is the first configuration in which a
correctly-parsed span can be dropped on purpose. A producer that **over-tags** body content as
`/Artifact` (a known real-world tagging bug) would therefore lose that content silently. No
corpus fixture exercises an over-tagged PDF today; worth adding one if such a document turns up.
The trade is deliberate: the failure requires a broken producer, whereas indexing running
headers and page-number folios as content was happening on every well-formed tagged PDF.

## 2b. Upstream (pdf_oxide): dehyphenation is not wired into `to_markdown()`

**Repo:** yfedoseev/pdf_oxide · **Type:** bug / inconsistency

`TextPostProcessor::rejoin_hyphenated_words` correctly strips U+00AD and joins
line-break hyphens per PDF spec ISO 32000-1 §14.8.2.2.3, but it is reachable
only from the **deprecated** `MarkdownConverter` path and from the opt-in
`apply_intelligent_text_processing()`. The `to_markdown()` path — the
documented, non-deprecated API — never calls it, so soft hyphens land in the
output verbatim.

We work around it with a local `strip_soft_hyphens` (delete U+00AD
unconditionally; we deliberately do **not** re-implement the line-break join,
which is where `well-being` → `wellbeing` corruption comes from). Request:
either call the post-processor from `to_markdown()`, or expose it as a
`ConversionOptions` flag so callers do not have to reimplement it.

## 2c. Upstream (pdf_oxide): monospace→code fencing over-fires on book prose

**Repo:** yfedoseev/pdf_oxide · **Type:** bug / heuristic-tuning

`fence_monospace_blocks()` wraps a paragraph in a bare ``` fence whenever its
glyph runs report `is_monospace`. On typeset novels this fires on ordinary
quoted dialogue, so narrative prose is emitted as a code block and downstream
consumers label it `block_kind: "code"`.

Relatedly, `detect_headings`' font-clustering promotes multi-line terminal
transcripts to `#` headings in technical books; `is_valid_heading_text` /
`demote_body_like_headings` do not catch them.

We guard both locally (`demote_prose_fenced_as_code`,
`demote_spurious_headings` in `extract/src/pdf.rs`) and will delete our guards
if these are fixed upstream. Request: require corroborating signals beyond
`is_monospace` (line-length variance, indentation structure, symbol density)
before fencing, and beyond font size before promoting a heading.

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
>
> **PDF extraction quality.** Extraction is now tuned for retrieval rather than
> visual fidelity. Running headers, footers, page-number folios and watermarks
> tagged as artifacts are dropped instead of becoming their own chunks;
> ligatures (`ﬁ`, `ﬂ`, …) are expanded so search matches real words; soft
> hyphens are removed so `recon‐struction` indexes as `reconstruction`; pages
> with no text layer are dropped with a warning naming them, instead of having a
> placeholder marker indexed as content. Misdetected headings no longer poison
> the `heading_path` breadcrumb of every following chunk, and a novel's dialogue
> is no longer served as `block_kind: "code"`.
>
> **PDF metadata.** PDFs now populate Dublin Core (`creator`, `subject`,
> `description`, `date`, `language`, `rights`) from the Info dictionary and XMP,
> closing the gap where the same book as PDF and as EPUB looked like two
> differently-described resources.
>
> These changes alter extracted text, so — like the swap itself — they change
> every PDF's content hash. Both land in the same release **on purpose**: that is
> one re-index and one re-embedding pass rather than two. On a large store (the
> QA corpus was 1,063 documents / 642k chunks) this is a significant one-time
> cost, so plan the first `localdb index` after upgrading accordingly.
