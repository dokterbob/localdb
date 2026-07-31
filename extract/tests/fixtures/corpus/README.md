# Difficult-PDF corpus

A small corpus of deliberately-hard, freely-licensed PDFs used by
`extract/tests/corpus.rs` to validate the `pdf_oxide` text-extraction path
against real-world failure classes (issues #87, #45, #65, #157).

Ground truth was produced by having an agent **read the rendered page images**
(pypdfium2 at ~144 DPI) — independent of any text extractor — and transcribe
exact body-text phrases per page. Each PDF has a sibling `<stem>.json`
expectation file the test consumes; `manifest.json` records provenance,
license, sha256, page count, and why each file is difficult.

## What the test checks

For every expectation file whose PDF is present:

- **No panic** — extraction of a malformed PDF must never panic (the #87 class).
- **`expect`** — `"ok"` requires successful extraction, `"err"` requires an
  error, `"any"` accepts either (used for encrypted / heavily-damaged inputs
  where a clean refusal is also correct).
- **`forbid_substrings`** — must never appear in the output (always includes the
  Unicode replacement char `U+FFFD`; the anti-mojibake guarantee, #45/#157).
- **`page_phrases`** — for small, clean `"ok"` documents (≤ 12 pages), a high
  fraction of the transcribed phrases must appear in the extracted text.
  Large books and reading-order-complex layouts are exempt from phrase recall
  (extraction reading order legitimately differs from visual order) but still
  get the no-panic, page-count, and anti-mojibake checks.

## Redistribution

All files except `sewtha-sustainable-energy.pdf` are redistributable under the
licenses listed below and are committed here. `sewtha.pdf` is free to download
but carries no redistribution grant, so it is **not vendored** — fetch it with
`scripts/fetch_test_pdfs.sh` (it lands in this directory, gitignored). The
corpus test is ignored-if-absent, so a missing `sewtha.pdf` simply skips that
one case.

## Provenance

GitHub sources are pinned to the commit SHA they were fetched from.

<!-- generated from manifest.json -->
| File | Difficulty | License | Source |
|---|---|---|---|
| `sewtha-sustainable-energy.pdf` _(fetch-on-demand)_ | Real 383-page book (issue #87 repro), mixed text / vector graphics / margin notes | Free download from author's site, no redistribution grant | https://www.inference.org.uk/sustainable/book/tex/sewtha.pdf |
| `plos-compbio-two-column.pdf` | Genuine two-column academic journal layout with tables | CC BY 4.0 (PLOS) | https://journals.plos.org/ploscompbiol/ |
| `financial-table-layout.pdf` | Financial table, each cell a separate right-aligned text-show op | MIT OR Apache-2.0 (pdf_oxide fixtures) | github.com/yfedoseev/pdf_oxide @ 10b87f1 |
| `cjk-vertical-jo.pdf` | DVIPDFMx vertical Japanese poem (Miyazawa Kenji), no ToUnicode | MIT (via pdf_oxide fixtures) | github.com/yfedoseev/pdf_oxide @ 10b87f1 |
| `cjk-kampo-multipage.pdf` | Japanese reference mixing Ryumin-Light / GothicBBB CID fonts | MIT (via pdf_oxide fixtures) | github.com/yfedoseev/pdf_oxide @ 10b87f1 |
| `encrypted-cid-truetype.pdf` | RC4-128 encrypted (empty user password) + CID TrueType Identity-H | MIT OR Apache-2.0 (pdf_oxide fixtures) | github.com/yfedoseev/pdf_oxide @ 10b87f1 |
| `broken-xref-verapdf.pdf` | veraPDF ISO 32000-1 §6.1.4 cross-reference-table conformance fail | CC BY 4.0 (veraPDF-corpus) | github.com/veraPDF/veraPDF-corpus @ 3777285 |
| `damaged-content-stream.pdf` | qpdf's damaged/truncated content-stream regression fixture | Apache-2.0 (qpdf) | github.com/qpdf/qpdf @ 8ff6b5c |
| `broken-xref-qpdf.pdf` | 819-byte qpdf corrupted-xref regression fixture | Apache-2.0 (qpdf) | github.com/qpdf/qpdf @ 8ff6b5c |
| `simple-multipage-text.pdf` | Clean 4-page text control with distinguishable per-page content | CC0 / public domain (self-authored) | — |

See `manifest.json` for exact URLs, commit SHAs, and sha256 checksums.
