//! PDF extraction via `pdf_oxide`: per-page Markdown plus page-start offsets.
//!
//! Each page is converted to Markdown independently and concatenated;
//! `PdfExtract::page_starts` records the byte offset where each page's content
//! begins, which downstream block building resolves into per-block page
//! numbers (#103).
//!
//! Scanned PDFs (no text layer) yield [`Error::UnsupportedFormat`], not
//! garbage text. No pdf_oxide type leaks out of this module — the rest of the
//! crate sees only [`PdfExtract`], keeping a parser swap a one-file change.

use std::borrow::Cow;

use localdb_core::metadata::DublinCoreMetadata;
use localdb_core::Error;
use pdf_oxide::converters::ConversionOptions;
use pdf_oxide::PdfDocument;

/// Minimum ratio of printable characters required to consider a PDF text-bearing.
///
/// Below this threshold the PDF is treated as a scanned image document.
const MIN_PRINTABLE_RATIO: f64 = 0.1;

/// Minimum absolute character count to consider a PDF text-bearing.
const MIN_TEXT_CHARS: usize = 20;

/// Result of PDF extraction: Markdown, Dublin Core metadata, and page-start
/// offsets.
#[derive(Debug, Clone)]
pub struct PdfExtract {
    /// The whole document as Markdown, pages concatenated in order.
    pub markdown: String,
    /// Dublin Core metadata: Info dictionary first per field, XMP as fallback
    /// (see [`document_metadata`]). `format` is left unset — `parsers/pdf.rs`
    /// fills it from the sniffed MIME type.
    pub metadata: DublinCoreMetadata,
    /// `(byte_offset, page_number)` for every page that contributed content,
    /// ascending in both fields. `byte_offset` indexes into `markdown`;
    /// `page_number` is 1-based. Pages that yielded no text are absent.
    pub page_starts: Vec<(usize, u32)>,
}

/// Extract a PDF into Markdown with per-page offsets and a title.
///
/// Returns [`Error::ExtractionFailed`] for corrupt/malformed PDFs where no
/// page could be extracted, and [`Error::UnsupportedFormat`] for scanned
/// (no text layer) or password-protected PDFs.
///
/// A page that individually fails to convert is skipped with a warning —
/// one broken page must not lose a whole book — but if *every* page fails
/// the document as a whole is an extraction failure.
pub fn extract_pdf(bytes: &[u8]) -> Result<PdfExtract, Error> {
    let doc = PdfDocument::from_bytes(bytes.to_vec()).map_err(|e| Error::ExtractionFailed {
        format: "pdf".into(),
        reason: e.to_string(),
    })?;

    // Encrypted and not decryptable with the empty owner/user password:
    // `to_markdown` would silently return empty pages, which would then be
    // misreported as "scanned". Fail with an honest reason instead.
    if !doc.is_authenticated() {
        return Err(Error::UnsupportedFormat {
            format: "pdf (encrypted — password required)".to_string(),
        });
    }

    let page_count = doc.page_count().map_err(|e| Error::ExtractionFailed {
        format: "pdf".into(),
        reason: e.to_string(),
    })?;

    let options = retrieval_conversion_options();
    let mut markdown = String::new();
    let mut page_starts: Vec<(usize, u32)> = Vec::new();
    let mut ok_pages = 0usize;
    let mut textless_pages: Vec<u32> = Vec::new();
    let mut last_err: Option<String> = None;

    for page in 0..page_count {
        match doc.to_markdown(page, &options) {
            Ok(md) => {
                ok_pages += 1;
                let processed = postprocess_page_markdown(&md);
                let trimmed = processed.trim();
                if trimmed.is_empty() {
                    // With `annotate_skipped_pages: false` a page whose text
                    // layer is missing (a scan inside an otherwise-text PDF)
                    // produces nothing at all rather than an `[OCR REQUIRED]`
                    // marker. Record it so the whole set can be reported once.
                    textless_pages.push((page + 1) as u32);
                    continue;
                }
                if !markdown.is_empty() {
                    markdown.push_str("\n\n");
                }
                page_starts.push((markdown.len(), (page + 1) as u32));
                markdown.push_str(trimmed);
            }
            Err(e) => {
                tracing::warn!(page = page + 1, error = %e, "skipping unextractable PDF page");
                last_err = Some(e.to_string());
            }
        }
    }

    if ok_pages == 0 {
        return Err(Error::ExtractionFailed {
            format: "pdf".into(),
            reason: last_err.unwrap_or_else(|| "PDF has no extractable pages".to_string()),
        });
    }

    if is_scanned_pdf(&markdown) {
        return Err(Error::UnsupportedFormat {
            format: "pdf (scanned — no text layer detected)".to_string(),
        });
    }

    // A *mixed* PDF — real text on some pages, bare scans on others — must
    // still index its text, so this is a warning and not an error. But the
    // dropped pages must be visible: before `annotate_skipped_pages: false`
    // their `[OCR REQUIRED …]` markers were silently indexed as content, which
    // both polluted retrieval and let such documents slip past
    // `is_scanned_pdf` (marker text counts as printable).
    if !textless_pages.is_empty() {
        tracing::warn!(
            textless_pages = textless_pages.len(),
            page_count,
            pages = %format_page_ranges(&textless_pages),
            "PDF pages have no text layer and were dropped (OCR is out of scope)"
        );
    }

    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }

    Ok(PdfExtract {
        markdown,
        metadata: document_metadata(&doc),
        page_starts,
    })
}

/// Conversion options tuned for a *retrieval corpus* rather than for
/// faithful visual reproduction.
///
/// Every field set here is a deliberate deviation from a `pdf_oxide` default
/// that is wrong for indexing. The defaults are chosen for round-tripping and
/// for scoring well against ground-truth corpora that preserve page furniture;
/// we want the opposite — only the words an author wrote.
fn retrieval_conversion_options() -> ConversionOptions {
    ConversionOptions {
        // Drop `/Artifact`-tagged content: running headers and footers, page
        // numbers, watermarks. Spec-correct per ISO 32000-1 §14.8.2.2.1 —
        // pdf_oxide only defaults this on for backward compatibility. Handles
        // *tagged* PDFs, where the producer marked the furniture for us.
        include_artifacts: false,
        // NOT enabled: `strip_running_headers_footers: true`.
        //
        // It is the intended geometric counterpart for *untagged* PDFs, but it
        // destroys body text. `repeated_running_head_foot` matches individual
        // `TextSpan`s (glyph runs), not assembled lines, against the top/bottom
        // 15% band — and in a multi-column journal the first line of every
        // column sits inside that band. Any fragment longer than 3 chars that
        // recurs there on ≥60% of pages is then deleted from the band on
        // *every* page. Measured on `tests/fixtures/corpus/plos-compbio-two-column.pdf`:
        //   "gene function—how individual genes contribute to"
        //     → "gene function—how  contribute to"
        //   "As representatives of the international consortium that produces
        //    the GO, we show how the apparent evidence"
        //     → "As representatives  consortium that produces the  apparent evidence"
        // Silent mid-sentence word loss is far worse than an indexed running
        // header, so we take the artifact-tag path only until this is fixed
        // upstream. See docs/followups-pdf-oxide-swap.md.
        //
        // ﬁ/ﬂ/ﬀ/ﬃ/ﬄ (U+FB00–U+FB06) → fi/fl/ff/ffi/ffl, so BM25 tokenization
        // and the embedder see real words. pdf_oxide defaults this off to
        // avoid a Jaccard penalty against ligature-preserving ground truth,
        // which is not a trade-off that applies to search.
        expand_ligatures: true,
        // Do not emit the `[OCR REQUIRED — page N]` block-quote for a page with
        // no text layer. That marker is not content: indexing it pollutes
        // retrieval, and because it is printable it also let mixed
        // scanned/text PDFs evade `is_scanned_pdf`. `extract_pdf` reports the
        // dropped pages with a single warning instead.
        annotate_skipped_pages: false,
        ..ConversionOptions::default()
    }
}

/// Per-page repair of the Markdown `pdf_oxide` produces, applied inside
/// `extract_pdf`'s page loop before pages are concatenated.
///
/// Each step is a guard against a specific upstream defect and is written as a
/// pure function so it can be unit-tested on string literals; see the
/// individual doc comments for what each one does and does not do.
fn postprocess_page_markdown(md: &str) -> String {
    let no_shy = strip_soft_hyphens(md);
    demote_prose_fenced_as_code(&demote_spurious_headings(&no_shy))
}

/// Maximum length (in `char`s) a legitimate heading is expected to have.
///
/// 200, not the ~120 a book chapter title needs: real single-line titles in
/// legal filings ("AGREEMENT AND PLAN OF MERGER BY AND AMONG …", 160 chars)
/// and academic papers routinely exceed 120, and demoting those is the
/// expensive error. A misdetected body *paragraph* — what this cap actually
/// targets — runs to several hundred characters, so 200 still separates them.
const MAX_HEADING_LEN: usize = 200;

/// Closed-class words a well-formed heading essentially never ends with —
/// at the end of a line they signal a truncated, continuing sentence
/// ("… configuring the network and") rather than a title.
///
/// Deliberately narrow. Words that *do* end real titles were excluded even
/// though they are closed-class: `is`/`are`/`was`/`were` ("The Way We Were"),
/// `no`/`not` ("Just Say No"), `this`/`that` ("Remember This"),
/// `who`/`whom`/`whose` ("Who's Who"), `on` ("Carry On"), `in` ("Checking
/// In"), `for` ("What Are We Fighting For"). Missing a dangling continuation
/// merely preserves today's behavior; demoting a real heading loses a
/// breadcrumb on every well-formed PDF.
///
/// `a` and `or` are also excluded, for a subtler reason: matching is
/// case-insensitive, so they collide with single-letter and initialism
/// *labels* rather than the article and the conjunction — "Appendix A",
/// "Exhibit A", "Schedule A", "Vitamin A", "Hepatitis A", "Anesthesia in the
/// OR". Those are boilerplate headings in legal, medical and technical
/// documents. [`is_dangling_continuation`] additionally requires the trailing
/// word to be lowercase *as written* for the same reason.
const CLOSED_CLASS_TAIL_WORDS: &[&str] = &[
    "and", "the", "of", "to", "with", "an", "from", "by", "as", "than", "into", "onto", "upon",
];

/// Demote any ATX heading line (`#` through `######`) in `md` that fails a
/// sanity check to an ordinary paragraph line, by stripping the leading `#`s
/// (and one following space) while keeping the text itself.
///
/// This exists because pdf_oxide's font-clustering heading detector
/// occasionally promotes a body paragraph or a terminal transcript to a
/// top-level `#` heading. `heading_path_from_blocks` then propagates that
/// bogus heading as a breadcrumb onto every following chunk in the document,
/// poisoning search results for the rest of the file.
///
/// Demotion is safe by construction: a demoted heading becomes ordinary text,
/// so it stays indexed and searchable — it just stops being a breadcrumb.
///
/// # Design principle
///
/// The bar is set high against false positives: a real, short, well-formed
/// heading (`# Chapter Three: The Reckoning`, `## Fuzzing the CAN Bus`,
/// non-ASCII headings, headings with an internal `.` from an abbreviation or
/// version number) must *always* survive. A line is demoted only when it
/// trips one of a small set of disqualifiers that a genuine short heading
/// essentially never trips:
///
/// - longer than [`MAX_HEADING_LEN`] chars;
/// - 2+ sentence terminators each followed by whitespace and a capital
///   (i.e. it reads as multiple sentences);
/// - ends in a dangling continuation (`,` `;` `:` `-` `—` `(`) or in a
///   [`CLOSED_CLASS_TAIL_WORDS`] word;
/// - starts with a lowercase letter;
/// - contains a *generic* shell/transcript marker (`$ `, leading `> `,
///   `C:\`, `#!`). Tool-specific patterns (`msf>`, `meterpreter`) are
///   deliberately avoided — they do not generalize, and the length and
///   sentence-structure rules already catch the Metasploit transcript,
///   which is long and prose-shaped by construction;
/// - contains an embedded newline — a heading is one line.
///
/// Only ATX headings are considered. Setext headings, `#` inside fenced code
/// blocks (tracked across lines), and `#` occurring mid-line are untouched.
///
/// # Known limitation
///
/// This is a false-positive guard only. It cannot *recover* a heading
/// pdf_oxide failed to detect (e.g. a missed Part title), which leaves the
/// last correctly-detected heading stale in the breadcrumb for the pages that
/// follow. Suppressing false positives cannot invent a missing true positive.
fn demote_spurious_headings(md: &str) -> String {
    let had_trailing_newline = md.ends_with('\n');
    let mut out_lines: Vec<String> = Vec::new();

    let mut in_fence = false;
    let mut fence_char = '`';
    let mut fence_len = 0usize;

    for line in md.lines() {
        let trimmed_start = line.trim_start_matches(' ');
        let indent_len = line.len() - trimmed_start.len();

        if in_fence {
            if let Some((_indent, ch, len, info)) = parse_fence_line(line) {
                if ch == fence_char && len >= fence_len && info.is_empty() {
                    in_fence = false;
                }
            }
            out_lines.push(line.to_string());
            continue;
        }

        if let Some((_indent, ch, len, _info)) = parse_fence_line(line) {
            in_fence = true;
            fence_char = ch;
            fence_len = len;
            out_lines.push(line.to_string());
            continue;
        }

        let spurious = parse_atx_heading(line).filter(|(_level, content)| should_demote(content));
        if let Some((_level, content)) = spurious {
            let indent = &line[..indent_len];
            out_lines.push(format!("{indent}{content}"));
            continue;
        }

        out_lines.push(line.to_string());
    }

    let mut result = out_lines.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    result
}

/// Parse one line as an ATX heading, returning `(level, content)` with any
/// trailing closing sequence (`## Title ##`) stripped per the ATX spec.
///
/// Byte-index operations only ever advance over single-byte ASCII (`' '`,
/// `'#'`), so slicing stays on UTF-8 boundaries.
fn parse_atx_heading(line: &str) -> Option<(usize, &str)> {
    let bytes = line.as_bytes();

    let mut idx = 0usize;
    let mut spaces = 0usize;
    while spaces < 3 && bytes.get(idx) == Some(&b' ') {
        idx += 1;
        spaces += 1;
    }

    let hash_start = idx;
    while bytes.get(idx) == Some(&b'#') {
        idx += 1;
    }
    let level = idx - hash_start;
    if level == 0 || level > 6 {
        return None;
    }

    match bytes.get(idx) {
        None => return Some((level, "")),
        Some(&b' ') | Some(&b'\t') => idx += 1,
        _ => return None,
    }

    let mut content = line[idx..].trim();

    let without_hashes = content.trim_end_matches('#');
    if without_hashes.len() != content.len()
        && (without_hashes.is_empty()
            || without_hashes.ends_with(' ')
            || without_hashes.ends_with('\t'))
    {
        content = without_hashes.trim_end();
    }

    Some((level, content))
}

/// True when `text` trips any disqualifier and should stop being a heading.
fn should_demote(text: &str) -> bool {
    text.chars().count() > MAX_HEADING_LEN
        || has_embedded_newline(text)
        || has_multiple_sentences(text)
        || is_dangling_continuation(text)
        || starts_lowercase(text)
        || has_shell_marker(text)
}

fn has_embedded_newline(text: &str) -> bool {
    text.contains('\n') || text.contains('\r')
}

/// True when `text` contains 2+ sentence terminators each followed by
/// whitespace and a capital — i.e. it reads as more than one sentence. A
/// single boundary is allowed through: that is the shape of "Dr. Watson's
/// Method" and "Section 2.1 Overview".
///
/// A terminator that closes a **single-letter token** is not counted: spaced
/// initials are a standard convention in biography and anthology titles
/// ("U. S. Grant: General and President", "On Fairy-Stories by J. R. R.
/// Tolkien") and would otherwise register two or three false boundaries.
fn has_multiple_sentences(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut boundaries = 0usize;

    for i in 0..chars.len() {
        if !matches!(chars[i], '.' | '!' | '?') {
            continue;
        }
        // "J. R. R. Tolkien": the `.` closes a lone letter, so it is an
        // initial, not a sentence end.
        let closes_initial = chars[i] == '.'
            && i >= 1
            && chars[i - 1].is_alphabetic()
            && (i == 1 || !chars[i - 2].is_alphanumeric());
        if closes_initial {
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if j > i + 1 && j < chars.len() && chars[j].is_uppercase() {
            boundaries += 1;
            if boundaries >= 2 {
                return true;
            }
        }
    }
    false
}

/// True when `text` ends mid-thought: dangling punctuation, or a closed-class
/// word a title would not end on.
///
/// A trailing `:` is deliberately *not* dangling — "Note:", "Warning:",
/// "Caution:", "In This Chapter:" are ordinary callout headings throughout
/// technical and medical manuals.
fn is_dangling_continuation(text: &str) -> bool {
    let trimmed = text.trim_end();
    let Some(last_char) = trimmed.chars().last() else {
        return false;
    };
    if matches!(last_char, ',' | ';' | '-' | '—' | '(') {
        return true;
    }
    if let Some(last_word) = trimmed.split_whitespace().last() {
        let cleaned: String = last_word.chars().filter(|c| c.is_alphanumeric()).collect();
        // Lowercase *as written*, and more than one letter: a trailing "A" in
        // "Appendix A" or "OR" in "… in the OR" is a label, not a function
        // word, and casing is what tells them apart.
        let is_function_word = cleaned.chars().count() > 1
            && cleaned.chars().all(|c| !c.is_uppercase())
            && CLOSED_CLASS_TAIL_WORDS.contains(&cleaned.to_lowercase().as_str());
        if is_function_word {
            return true;
        }
    }
    false
}

/// True when the heading begins lowercase in a way that reads as a sentence
/// fragment rather than a title.
///
/// This is the disqualifier that catches the motivating case — a promoted
/// terminal transcript, `msf exploit(ms08_067_netapi) > set RHOST …` — which
/// trips no length, punctuation or sentence rule. But a bare "first letter is
/// lowercase" test wrongly demotes a large class of real headings, so two
/// exemptions apply:
///
/// - the first word contains an uppercase letter later on — the camel-cased
///   brand and scientific-notation class: `iOS`, `eBay`, `pH`, `mRNA`,
///   `cAMP`, `macOS`;
/// - the first word is followed by a capitalised word — the lowercase-particle
///   class: `von Neumann Architecture`, `de Broglie Wavelength`, and
///   `x86 Assembly Language Basics`.
///
/// Text with no cased alphabetic character (digits only, or an uncased script
/// like Han) is never flagged.
fn starts_lowercase(text: &str) -> bool {
    let Some(first) = text.chars().find(|c| c.is_alphabetic()) else {
        return false;
    };
    if !first.is_lowercase() {
        return false;
    }
    let mut words = text.split_whitespace();
    let Some(first_word) = words.next() else {
        return false;
    };
    // `iOS`, `pH`, `mRNA`: an internal capital marks a brand or notation.
    if first_word.chars().skip(1).any(|c| c.is_uppercase()) {
        return false;
    }
    // `von Neumann`, `x86 Assembly`: a capitalised next word marks a title.
    if words
        .next()
        .and_then(|w| w.chars().find(|c| c.is_alphabetic()))
        .is_some_and(|c| c.is_uppercase())
    {
        return false;
    }
    true
}

/// True when `text` carries a *generic* shell/transcript marker. Deliberately
/// not tool-specific (no `msf>`, no `meterpreter`) so it generalizes.
///
/// Every marker is anchored to the **start** of the line, because that is
/// where a prompt sits. Searching anywhere in the line wrongly demoted real
/// headings that merely mention one: "Shell Scripting Basics: #!/bin/bash
/// Explained", "Navigating C:\\Windows and System Files", and — since PDF
/// extraction readily inserts a space after a kerned glyph — "Financing the
/// US$ 2 Trillion Gap".
fn has_shell_marker(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("$ ")
        || t.starts_with("> ")
        || t.starts_with("C:\\")
        || t.starts_with("#!")
        || starts_with_status_marker(t)
}

/// True for a leading single-**symbol** bracket status marker: `[*] `, `[+] `,
/// `[-] `, `[!] `. This is the canonical console convention (msfconsole,
/// sqlmap, countless pentest scripts) for lines like
/// `[*] Started reverse TCP handler on 10.0.0.1:4444`, which otherwise trip
/// no disqualifier at all — short, capitalised, single-sentence.
///
/// Restricted to a lone non-alphanumeric character on purpose. Word-style
/// tags (`[INFO]`, `[ERROR]`, `[1]`) are excluded because they are ambiguous
/// with genuine front-matter headings: `[Part I] The Awakening`,
/// `[Draft] Chapter Three`, `[Redacted] Names Have Been Changed`.
fn starts_with_status_marker(t: &str) -> bool {
    let Some(rest) = t.strip_prefix('[') else {
        return false;
    };
    let mut chars = rest.chars();
    matches!(chars.next(), Some(c) if !c.is_alphanumeric() && !c.is_whitespace())
        && chars.next() == Some(']')
        && chars.next() == Some(' ')
}

// ---------------------------------------------------------------------------
// F: prose wrongly fenced as code.
// ---------------------------------------------------------------------------

/// Byte range of one line's content, *excluding* its trailing `\n` (if any).
type LineSpan = (usize, usize);

/// Un-fence any *bare* fenced block in `md` whose content reads as ordinary
/// prose, leaving everything else byte-for-byte identical.
///
/// pdf_oxide's `fence_monospace_blocks()` wraps a paragraph in a fence
/// whenever the glyph runs it saw reported `is_monospace`. On real book PDFs
/// this fires on ordinary quoted dialogue set in a monospace-ish face,
/// mislabelling narrative prose as `block_kind: "code"` in search results.
///
/// # Design principle
///
/// Strongly biased toward leaving fences alone: mis-un-fencing a real
/// terminal transcript is worse than leaving one novel's dialogue mislabeled.
/// A block is demoted only when it is BOTH free of code signals AND clearly
/// dominated by prose signals — a conjunction, not a comparison. Any
/// ambiguity (an info string, an unclosed fence, an indented fence, a single
/// code signal anywhere) leaves the block untouched.
///
/// Two structural safety margins:
/// 1. Only **bare** fences are considered. `fence_monospace_blocks` emits no
///    info string, so a ```` ```rust ```` fence is by construction never ours.
/// 2. Any single code signal blocks demotion outright, however much prose
///    surrounds it.
///
/// # Known misses (accepted)
///
/// Adversarial review found genuine code that reads as prose and is therefore
/// un-fenced: **Inform 7** (a programming language deliberately written as
/// English), period-terminated **Gherkin/BDD** steps, English **pseudocode**
/// paragraphs, and GNU `--help` epilogue prose. These are accepted rather than
/// fixed. The only signal that separates them from novel dialogue is the
/// double quote, and adding `"` to [`CODE_SYMBOLS`] would re-break the
/// dialogue case this guard exists for — precisely backwards. The cost here is
/// a wrong `block_kind` on a rare block, not lost or altered text: the content
/// is byte-identical either way and stays fully indexed and searchable.
fn demote_prose_fenced_as_code(md: &str) -> String {
    let line_spans = split_lines(md);
    let mut out = String::with_capacity(md.len());
    let mut i = 0usize;

    while i < line_spans.len() {
        let Some(open) = find_fence_open(md, &line_spans, i) else {
            let end = line_full_end(md, &line_spans, i);
            out.push_str(&md[line_spans[i].0..end]);
            i += 1;
            continue;
        };

        // Copy through any plain lines before the fence verbatim.
        if open.open_line > i {
            out.push_str(&md[line_spans[i].0..line_spans[open.open_line].0]);
        }

        let Some(close_line) = find_fence_close(md, &line_spans, open.first_content_line, &open)
        else {
            // Unclosed fence: leave the rest of the document alone.
            out.push_str(&md[line_spans[open.open_line].0..]);
            break;
        };

        let has_body = open.first_content_line < close_line;
        let content_start = if has_body {
            line_spans[open.first_content_line].0
        } else {
            line_full_end(md, &line_spans, open.open_line)
        };
        let content_end = if has_body {
            line_full_end(md, &line_spans, close_line - 1)
        } else {
            content_start
        };
        let content = &md[content_start..content_end];

        let eligible = open.indent == 0 && open.info_string.is_empty();
        if eligible && !is_code_like(content) && is_prose_like(content) {
            // Drop the fence lines, keep the content bytes verbatim.
            out.push_str(content);
        } else {
            let block_end = line_full_end(md, &line_spans, close_line);
            out.push_str(&md[line_spans[open.open_line].0..block_end]);
        }
        i = close_line + 1;
    }

    out
}

/// Split `md` into line spans (byte ranges), each excluding its trailing `\n`.
fn split_lines(md: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (idx, _) in md.match_indices('\n') {
        spans.push((start, idx));
        start = idx + 1;
    }
    spans.push((start, md.len()));
    spans
}

/// End byte offset of line `idx` *including* its trailing `\n` if present.
fn line_full_end(md: &str, spans: &[LineSpan], idx: usize) -> usize {
    if idx + 1 < spans.len() {
        spans[idx + 1].0
    } else {
        md.len()
    }
}

struct FenceOpen {
    open_line: usize,
    first_content_line: usize,
    fence_char: char,
    fence_len: usize,
    indent: usize,
    info_string: String,
}

/// Scan forward from `from` for a fence-opening line.
fn find_fence_open(md: &str, spans: &[LineSpan], from: usize) -> Option<FenceOpen> {
    (from..spans.len()).find_map(|i| {
        let line = &md[spans[i].0..spans[i].1];
        parse_fence_line(line).map(|(indent, ch, len, info)| FenceOpen {
            open_line: i,
            first_content_line: i + 1,
            fence_char: ch,
            fence_len: len,
            indent,
            info_string: info.to_string(),
        })
    })
}

/// Scan forward for the line that closes `open`: same fence char, run at
/// least as long, and no info string.
fn find_fence_close(md: &str, spans: &[LineSpan], from: usize, open: &FenceOpen) -> Option<usize> {
    (from..spans.len()).find(|&j| {
        let line = &md[spans[j].0..spans[j].1];
        parse_fence_line(line).is_some_and(|(_i, ch, len, info)| {
            ch == open.fence_char && len >= open.fence_len && info.is_empty()
        })
    })
}

/// If `line` is a fence line (0–3 leading spaces then a run of 3+ identical
/// `` ` `` or `~`), return `(indent, fence_char, run_len, trimmed_info)`.
fn parse_fence_line(line: &str) -> Option<(usize, char, usize, &str)> {
    let no_cr = line.strip_suffix('\r').unwrap_or(line);
    let indent = no_cr.len() - no_cr.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &no_cr[indent..];
    let fence_char = rest.chars().next()?;
    if fence_char != '`' && fence_char != '~' {
        return None;
    }
    let run_len = rest.chars().take_while(|&c| c == fence_char).count();
    if run_len < 3 {
        return None;
    }
    let after = &rest[run_len..];
    // A backtick fence may not carry a backtick in its info string.
    if fence_char == '`' && after.contains('`') {
        return None;
    }
    Some((indent, fence_char, run_len, after.trim()))
}

/// Symbol characters source code uses at far higher density than prose.
const CODE_SYMBOLS: [char; 11] = [';', '{', '}', '(', ')', '[', ']', '=', '<', '>', '|'];

/// True when `content` shows ANY meaningful signal of source code or a
/// terminal transcript. Checked before prose scoring; if true it blocks
/// demotion unconditionally.
fn is_code_like(content: &str) -> bool {
    let non_blank: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if non_blank.is_empty() {
        return false;
    }

    // Consistent leading indentation across a meaningful fraction of lines.
    let indented = non_blank
        .iter()
        .filter(|l| l.starts_with("  ") || l.starts_with('\t'))
        .count();
    if non_blank.len() >= 2 && (indented as f64 / non_blank.len() as f64) >= 0.4 {
        return true;
    }

    // Density of code-ish punctuation.
    let total_chars = content.chars().count().max(1);
    let symbol_count = content.chars().filter(|c| CODE_SYMBOLS.contains(c)).count();
    if symbol_count >= 3 && (symbol_count as f64 / total_chars as f64) > 0.02 {
        return true;
    }

    non_blank.iter().any(|raw| {
        let t = raw.trim_start();
        // Shell prompts / Windows drive paths.
        t.starts_with("$ ")
            || t.starts_with("#!")
            || t.starts_with("> ")
            || t.starts_with("% ")
            || t.contains("C:\\")
            // Unix-ish paths.
            || t.contains("/usr/")
            || t.contains("./")
            || t.contains("../")
            // CLI flags.
            || t.split_whitespace().any(is_flag_token)
            || has_code_keyword(t)
            || has_key_value_pair(t)
            // Explicit line continuation.
            || t.trim_end().ends_with('\\')
            // Hex dump / CAN-bus frame, and hex literals.
            || has_hex_frame(t)
            || t.contains("0x")
            || contains_ip_like(t)
            // Multi-segment slash paths, e.g. `exploit/windows/smb/...`.
            || t.split_whitespace().any(|w| w.matches('/').count() >= 2)
    })
}

fn is_flag_token(w: &str) -> bool {
    if let Some(stripped) = w.strip_prefix("--") {
        return stripped.chars().next().is_some_and(|c| c.is_alphabetic());
    }
    if let Some(stripped) = w.strip_prefix('-') {
        let mut chars = stripped.chars();
        return matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric());
    }
    false
}

fn has_code_keyword(t: &str) -> bool {
    const KEYWORDS: [&str; 8] = [
        "use ", "import ", "def ", "fn ", "class ", "SELECT ", "select ", "impl ",
    ];
    KEYWORDS.iter().any(|kw| {
        t.starts_with(kw) || t.contains(&format!(" {kw}")) || t.contains(&format!("\t{kw}"))
    })
}

/// True for a `key: value` / `key=value` line, where the key is a bare
/// identifier. Deliberately catches URLs too (`https://…`), which keeps any
/// prose containing a link safely fenced.
fn has_key_value_pair(t: &str) -> bool {
    [':', '='].iter().any(|&sep| {
        t.find(sep).is_some_and(|idx| {
            let key = &t[..idx];
            let val = &t[idx + sep.len_utf8()..];
            !key.is_empty()
                && key.len() <= 30
                && !key.contains(char::is_whitespace)
                && key
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                && !val.trim().is_empty()
        })
    })
}

fn has_hex_frame(t: &str) -> bool {
    t.find('#').is_some_and(|idx| {
        let left = &t[..idx];
        let right = t[idx + 1..].trim();
        !left.is_empty()
            && left.chars().all(|c| c.is_ascii_digit())
            && right.len() >= 2
            && right.chars().all(|c| c.is_ascii_hexdigit())
    })
}

fn contains_ip_like(s: &str) -> bool {
    s.split_whitespace().any(|word| {
        let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
        let parts: Vec<&str> = core.split('.').collect();
        parts.len() == 4
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()))
    })
}

/// True when `content`'s prose signals clearly dominate: high alphabetic
/// density, ordinary word shape, and real sentence punctuation. Requires
/// enough words to judge, so a short fence is never demoted on its own.
fn is_prose_like(content: &str) -> bool {
    let trimmed = content.trim();
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.len() < 8 {
        return false;
    }

    let total_chars = trimmed.chars().count().max(1);
    // Letters among *non-whitespace* characters: whitespace is structural in
    // both prose and code, so it would only dilute the score.
    let non_ws_chars = trimmed
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
        .max(1);
    let alpha_ratio =
        trimmed.chars().filter(|c| c.is_alphabetic()).count() as f64 / non_ws_chars as f64;

    let alpha_word_count = words
        .iter()
        .filter(|w| {
            let core = w.trim_matches(|c: char| !c.is_alphanumeric());
            !core.is_empty() && core.chars().all(|c| c.is_alphabetic())
        })
        .count();
    let alpha_word_ratio = alpha_word_count as f64 / words.len() as f64;

    let word_alpha_lens: usize = words
        .iter()
        .map(|w| w.chars().filter(|c| c.is_alphabetic()).count())
        .sum();
    let avg_word_len = word_alpha_lens as f64 / words.len() as f64;

    let sentence_punct = trimmed
        .chars()
        .filter(|&c| matches!(c, '.' | ',' | '?' | '!'))
        .count();
    let punct_ratio = sentence_punct as f64 / total_chars as f64;

    // `alpha_word_ratio` is deliberately looser than the character-level
    // `alpha_ratio`. It counts whitespace-split *tokens* that are purely
    // alphabetic, so ordinary nonfiction prose ("Nearly 3.5 percent of the
    // population, or about 1.2 million people, had already left by 6.30")
    // scores ~0.85 on numbers alone and would never demote. The char-level
    // gate above still carries the real weight — numeric tables, CSV rows and
    // log lines fail it (or `is_code_like`) regardless of this bound.
    alpha_ratio > 0.85
        && alpha_word_ratio > 0.70
        && (3.0..=8.0).contains(&avg_word_len)
        && punct_ratio > 0.005
}

/// Render a sorted, ascending list of 1-based page numbers as compact ranges
/// (`[3, 4, 5, 9]` → `"3-5, 9"`), so a warning about a 400-page scan stays one
/// readable line.
fn format_page_ranges(pages: &[u32]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < pages.len() {
        let start = pages[i];
        let mut end = start;
        while i + 1 < pages.len() && pages[i + 1] == end + 1 {
            i += 1;
            end = pages[i];
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

/// Strips every U+00AD SOFT HYPHEN from `md`, returning the input unchanged
/// (no allocation) when none are present.
///
/// pdf_oxide's page-to-Markdown conversion can leave U+00AD characters
/// embedded in the reflowed text. A soft hyphen is purely a discretionary
/// line-break hint for a rendering engine — it carries no textual meaning of
/// its own (PDF spec ISO 32000-1 §14.8.2.2.3), and Unicode classifies it as
/// Cf (format), i.e. invisible when not acting on a line break. Because
/// pdf_oxide has already reflowed each paragraph onto a single logical line
/// before this function runs, the two halves of any hyphenated word are
/// already adjacent in the string — there is no `-\n` split to rejoin.
///
/// Deletion is therefore both correct and sufficient: it reconstructs the
/// original word with no join logic, and, unlike a join step keyed on `-\n`,
/// it can never conflate a soft hyphen with a real hyphen-minus (`-`, U+002D)
/// or a Unicode HYPHEN (‐, U+2010), neither of which this function touches.
/// That join is exactly where `well-being` → `wellbeing` corruption comes
/// from, so it is deliberately not implemented.
fn strip_soft_hyphens(md: &str) -> Cow<'_, str> {
    if !md.contains('\u{00AD}') {
        return Cow::Borrowed(md);
    }
    Cow::Owned(md.chars().filter(|&c| c != '\u{00AD}').collect())
}

fn parse_pdf_date(raw: &str) -> Option<String> {
    // Generous upper bound for a well-formed value ("D:" + 14 digits + 1
    // O-char + "HH'mm'" = 2+14+1+6 = 23), padded for a trailing apostrophe.
    // Anything longer is not a spec-valid date; reject up front so pathological
    // input never reaches the field-by-field parser below.
    const MAX_LEN: usize = 32;
    if raw.len() > MAX_LEN {
        return None;
    }

    let rest = raw.strip_prefix("D:")?;
    let bytes = rest.as_bytes();

    // ---- YYYY (mandatory) ----
    if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year: u32 = rest[0..4].parse().ok()?;
    if bytes.len() == 4 {
        return Some(format!("{year:04}"));
    }

    // ---- MM ----
    if bytes.len() < 6 || !bytes[4..6].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let month: u32 = rest[4..6].parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    if bytes.len() == 6 {
        return Some(format!("{year:04}-{month:02}"));
    }

    // ---- DD ----
    if bytes.len() < 8 || !bytes[6..8].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let day: u32 = rest[6..8].parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let date_str = format!("{year:04}-{month:02}-{day:02}");
    if bytes.len() == 8 {
        return Some(date_str);
    }

    // ---- HH ----
    if bytes.len() < 10 || !bytes[8..10].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let hour: u32 = rest[8..10].parse().ok()?;
    if hour > 23 {
        return None;
    }
    if bytes.len() == 10 {
        return Some(date_str);
    }

    // ---- mm (minute) ----
    if bytes.len() < 12 || !bytes[10..12].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let minute: u32 = rest[10..12].parse().ok()?;
    if minute > 59 {
        return None;
    }
    if bytes.len() == 12 {
        return Some(date_str);
    }

    // ---- SS ----
    if bytes.len() < 14 || !bytes[12..14].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let second: u32 = rest[12..14].parse().ok()?;
    if second > 59 {
        return None;
    }
    if bytes.len() == 14 {
        return Some(date_str);
    }

    // ---- O (timezone sign) ----
    let tz = bytes[14];
    match tz {
        b'Z' => {
            // `Z` denotes UTC and per spec stands alone (default HH'=mm'=00
            // is implied, not written out). Anything after it is garbage.
            if bytes.len() == 15 {
                Some(date_str)
            } else {
                None
            }
        }
        b'+' | b'-' => {
            // `O` alone (offset unspecified beyond its sign) is accepted:
            // spec examples always pair it with HH'mm', but nothing in the
            // grammar forbids a producer stopping right after `O`.
            if bytes.len() == 15 {
                return Some(date_str);
            }

            // ---- timezone HH' ----
            if bytes.len() < 18
                || !bytes[15..17].iter().all(u8::is_ascii_digit)
                || bytes[17] != b'\''
            {
                return None;
            }
            let tz_hour: u32 = rest[15..17].parse().ok()?;
            if tz_hour > 23 {
                return None;
            }
            if bytes.len() == 18 {
                return Some(date_str);
            }

            // ---- timezone mm' (trailing apostrophe optional) ----
            if bytes.len() < 20 || !bytes[18..20].iter().all(u8::is_ascii_digit) {
                return None;
            }
            let tz_minute: u32 = rest[18..20].parse().ok()?;
            if tz_minute > 59 {
                return None;
            }
            match bytes.len() {
                20 => Some(date_str),
                21 if bytes[20] == b'\'' => Some(date_str),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Check if a PDF appears to be scanned (no meaningful text layer).
fn is_scanned_pdf(text: &str) -> bool {
    let total = text.len();
    if total == 0 {
        return true;
    }
    let printable: usize = text
        .chars()
        .filter(|c| !c.is_whitespace() && c.is_alphanumeric())
        .count();
    if printable < MIN_TEXT_CHARS {
        return true;
    }
    let ratio = printable as f64 / total as f64;
    ratio < MIN_PRINTABLE_RATIO
}

/// Full Dublin Core metadata for a PDF: Info dictionary first per field, XMP
/// as fallback where Info has no equivalent key or its value is absent/empty
/// — the same precedence [`document_title`] already used.
///
/// Brings PDF to parity with the EPUB path (`parsers/epub.rs::map_metadata`),
/// which populates the full set from the OPF. Before this, a PDF got nothing
/// but `format`, so the same book as PDF and as EPUB looked like two
/// completely differently-described resources.
///
/// `format` is deliberately left unset: it comes from the sniffed MIME type in
/// `parsers/pdf.rs`, not from document content.
///
/// `publisher` is deliberately left unset too. The Info dictionary's closest
/// key is `/Producer`, but that is the *generating software* ("Adobe PDF
/// Library 15.0"), not the publisher of the work — writing it into
/// `publisher` would put toolchain strings into a field consumers read as
/// provenance. Neither the Info dictionary nor pdf_oxide's XMP exposes a real
/// publisher, so it stays `None`.
///
/// This does not fix `title: None` for a PDF that genuinely carries no
/// `/Title` and no XMP: there is deliberately no filename fallback.
fn document_metadata(doc: &PdfDocument) -> DublinCoreMetadata {
    let xmp = pdf_oxide::extractors::xmp::XmpExtractor::extract(doc)
        .ok()
        .flatten();

    let creator = info_dict_string(doc, "Author")
        .map(|a| vec![a])
        .unwrap_or_else(|| {
            xmp.as_ref()
                .map(|x| x.dc_creator.clone())
                .unwrap_or_default()
        });

    let subject = info_dict_string(doc, "Keywords")
        .map(|k| split_keywords(&k))
        .unwrap_or_else(|| {
            xmp.as_ref()
                .map(|x| x.dc_subject.clone())
                .unwrap_or_default()
        });

    DublinCoreMetadata {
        title: document_title(doc),
        creator,
        subject,
        description: info_dict_string(doc, "Subject")
            .or_else(|| xmp.as_ref().and_then(|x| x.dc_description.clone())),
        date: info_dict_string(doc, "CreationDate")
            .and_then(|d| parse_pdf_date(&d))
            .or_else(|| {
                xmp.as_ref()
                    .and_then(|x| x.xmp_create_date.as_deref())
                    .map(xmp_date_to_iso_date)
            }),
        language: xmp.as_ref().and_then(|x| x.dc_language.clone()),
        rights: xmp.as_ref().and_then(|x| x.dc_rights.clone()),
        ..DublinCoreMetadata::default()
    }
}

/// Document title: Info dictionary `/Title` first (the canonical viewer
/// title), XMP `dc:title` as fallback.
fn document_title(doc: &PdfDocument) -> Option<String> {
    info_dict_string(doc, "Title").or_else(|| xmp_title(doc))
}

/// Read one string key from the trailer's `/Info` dictionary: resolve the
/// reference chain, decode per PDF text-string rules, trim, and treat an
/// empty result as absent.
fn info_dict_string(doc: &PdfDocument, key: &str) -> Option<String> {
    let info_raw = doc.trailer().as_dict()?.get("Info")?.clone();
    let info = doc.resolve_references(&info_raw, 2).ok()?;
    let value_raw = info.as_dict()?.get(key)?.clone();
    let value = doc.resolve_references(&value_raw, 2).ok()?;
    let decoded = decode_pdf_text_string(value.as_string()?);
    let trimmed = decoded.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Split a `/Keywords` string on `,` and `;`, trim, drop empties.
fn split_keywords(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// XMP's `xmp:CreateDate` is W3CDTF (`"2018-03-04T09:00:00Z"`). Truncate to
/// the date so it matches the granularity `DublinCoreMetadata::date` carries
/// elsewhere — the EPUB path stores a plain `"2021-05-01"`, never a timestamp.
fn xmp_date_to_iso_date(raw: &str) -> String {
    raw.split('T').next().unwrap_or(raw).to_string()
}

/// `dc:title` from XMP metadata, trimmed.
fn xmp_title(doc: &PdfDocument) -> Option<String> {
    let xmp = pdf_oxide::extractors::xmp::XmpExtractor::extract(doc)
        .ok()
        .flatten()?;
    let title = xmp.dc_title?;
    let trimmed = title.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Decode a PDF text string (PDF spec ISO 32000-1 §7.9.2.2): UTF-16BE when it
/// carries the `FE FF` BOM, UTF-8 when it carries the PDF 2.0 `EF BB BF` BOM,
/// otherwise PDFDocEncoding. UTF-8 is not a PDF text string encoding, but
/// real-world producers emit it without a BOM, so we try it for the non-BOM
/// case and fall back to PDFDocEncoding.
///
/// The UTF-8 fast path is guarded: it is only taken when the bytes contain no
/// `0x18..=0x1F` byte. Those bytes are valid ASCII controls (so `from_utf8`
/// would accept them and shadow the table), but in PDFDocEncoding they are
/// accent modifiers (BREVE, CARON, …). A genuine UTF-8 title never contains
/// literal C0 controls, so their presence means the string is PDFDocEncoding
/// and must go straight to the table.
fn decode_pdf_text_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else if !bytes.iter().any(|&b| (0x18..=0x1F).contains(&b)) {
        if let Ok(s) = std::str::from_utf8(bytes) {
            return s.to_string();
        }
        bytes.iter().map(|&b| pdf_doc_encoding_char(b)).collect()
    } else {
        bytes.iter().map(|&b| pdf_doc_encoding_char(b)).collect()
    }
}

/// Map one PDFDocEncoding byte to its Unicode scalar (PDF spec ISO 32000-1
/// Annex D.2). `0x00–0x7F` is ASCII and `0xA1–0xFF` matches Latin-1 with a few
/// exceptions; the `0x18–0x1F` and `0x80–0xA0` blocks and a few undefined slots
/// differ — casting the byte straight to `char` (Latin-1) mangles those (e.g.
/// `0x80` is a bullet `•`, not U+0080, and `0x18` is a BREVE, not a C0 control).
/// Undefined code points map to U+FFFD.
fn pdf_doc_encoding_char(b: u8) -> char {
    // The 0x18..=0x1F block: accent modifiers, in order.
    const LOW: [char; 8] = [
        '\u{02D8}', '\u{02C7}', '\u{02C6}', '\u{02D9}', '\u{02DD}', '\u{02DB}', '\u{02DA}',
        '\u{02DC}',
    ];
    // The 0x80..=0xA0 block, in order. `\u{FFFD}` marks the two undefined
    // slots (0x9F and 0xAD is handled below).
    const HIGH: [char; 33] = [
        '\u{2022}', '\u{2020}', '\u{2021}', '\u{2026}', '\u{2014}', '\u{2013}', '\u{0192}',
        '\u{2044}', '\u{2039}', '\u{203A}', '\u{2212}', '\u{2030}', '\u{201E}', '\u{201C}',
        '\u{201D}', '\u{2018}', '\u{2019}', '\u{201A}', '\u{2122}', '\u{FB01}', '\u{FB02}',
        '\u{0141}', '\u{0152}', '\u{0160}', '\u{0178}', '\u{017D}', '\u{0131}', '\u{0142}',
        '\u{0153}', '\u{0161}', '\u{017E}', '\u{FFFD}', '\u{20AC}',
    ];
    match b {
        0x18..=0x1F => LOW[(b - 0x18) as usize],
        0x80..=0xA0 => HIGH[(b - 0x80) as usize],
        // Undefined in PDFDocEncoding.
        0x7F | 0xAD => '\u{FFFD}',
        // 0x00..=0x7F ASCII and 0xA1..=0xFF (minus 0xAD) match Latin-1.
        _ => b as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::Error;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {name} must exist: {e}"))
    }

    /// A malformed fixture must never panic; it may recover (Ok) or fail
    /// with one of the two documented error variants.
    fn assert_no_panic_and_sane(name: &str) -> Result<PdfExtract, Error> {
        let result = extract_pdf(&fixture(name));
        match &result {
            Ok(_) | Err(Error::ExtractionFailed { .. }) | Err(Error::UnsupportedFormat { .. }) => {}
            Err(other) => panic!("{name}: unexpected error variant: {other:?}"),
        }
        result
    }

    // ------------------------------------------------------------------
    // Malformed-PDF fixtures (the #87 class): Err or recovery, never panic.
    // ------------------------------------------------------------------

    #[test]
    fn zero_operand_operators_do_not_panic() {
        // Content stream starts with operand-less Tj/Td/Tf/TJ — the exact
        // class that made pdf-extract panic with "index out of bounds".
        if let Ok(ex) = assert_no_panic_and_sane("malformed/zero_operand_ops.pdf") {
            // If recovery succeeds, the valid trailing text should be there.
            assert!(
                ex.markdown.contains("Recovered text"),
                "recovered output should contain the valid text run: {:?}",
                ex.markdown
            );
        }
    }

    #[test]
    fn truncated_stream_does_not_panic() {
        let _ = assert_no_panic_and_sane("malformed/truncated_stream.pdf");
    }

    #[test]
    fn broken_xref_does_not_panic() {
        let _ = assert_no_panic_and_sane("malformed/broken_xref.pdf");
    }

    #[test]
    fn empty_page_pdf_returns_err() {
        // Structurally valid, but a single page with no /Contents: nothing
        // to index, so this must be an error, not Ok("").
        let result = assert_no_panic_and_sane("malformed/empty_page.pdf");
        assert!(result.is_err(), "empty-page PDF must not yield Ok");
    }

    #[test]
    fn cid_font_without_tounicode_yields_no_mojibake() {
        // Type0/Identity-H font with no /ToUnicode and no embedded font
        // program: glyphs cannot be mapped. Acceptable outcomes are an
        // error or output without U+FFFD replacement chars — never mojibake.
        if let Ok(ex) = assert_no_panic_and_sane("malformed/cid_no_tounicode.pdf") {
            assert!(
                !ex.markdown.contains('\u{FFFD}'),
                "unmappable glyphs must not surface as replacement chars: {:?}",
                ex.markdown
            );
        }
    }

    #[test]
    fn garbage_bytes_return_extraction_failed() {
        let result = extract_pdf(b"%PDF-1.4\nnot a real pdf");
        match result {
            Err(Error::ExtractionFailed { .. }) | Err(Error::UnsupportedFormat { .. }) => {}
            Ok(ex) => panic!("garbage input should not extract: {:?}", ex.markdown),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Happy path: multi-page extraction with page offsets and title.
    // ------------------------------------------------------------------

    #[test]
    fn multipage_page_starts_are_correct() {
        let ex = extract_pdf(&fixture("multipage.pdf")).expect("multipage fixture must extract");

        let pages: Vec<u32> = ex.page_starts.iter().map(|&(_, p)| p).collect();
        assert_eq!(pages, vec![1, 2, 3], "all three pages must contribute");

        // Offsets strictly ascending and in bounds.
        let offsets: Vec<usize> = ex.page_starts.iter().map(|&(o, _)| o).collect();
        assert_eq!(offsets[0], 0, "page 1 starts at offset 0");
        assert!(offsets.windows(2).all(|w| w[0] < w[1]));
        assert!(*offsets.last().unwrap() < ex.markdown.len());

        // Distinctive per-page content lands within that page's span.
        let find = |needle: &str| {
            ex.markdown
                .find(needle)
                .unwrap_or_else(|| panic!("markdown must contain {needle:?}: {:?}", ex.markdown))
        };
        assert!(
            find("quick brown fox") < offsets[1],
            "page-1 text before page 2 start"
        );
        let sphinx = find("Sphinx of black quartz");
        assert!(
            (offsets[1]..offsets[2]).contains(&sphinx),
            "page-2 text within page 2 span"
        );
        assert!(
            find("Pack my box") >= offsets[2],
            "page-3 text after page 3 start"
        );
    }

    #[test]
    fn multipage_title_from_info_dict() {
        let ex = extract_pdf(&fixture("multipage.pdf")).expect("multipage fixture must extract");
        assert_eq!(
            ex.metadata.title.as_deref(),
            Some("Multipage Fixture Title")
        );
    }

    #[test]
    fn flat_body_text_gets_no_hallucinated_headings() {
        // Uniform 11pt body text: heading detection must not invent
        // structure (protects #158's coarse-Text chunk packing).
        let ex = extract_pdf(&fixture("flat_body.pdf")).expect("flat_body fixture must extract");
        for line in ex.markdown.lines() {
            assert!(
                !line.trim_start().starts_with('#'),
                "no line should become a heading, got: {line:?}"
            );
        }
    }

    #[test]
    fn extraction_ends_with_newline() {
        let ex = extract_pdf(&fixture("multipage.pdf")).expect("multipage fixture must extract");
        assert!(ex.markdown.ends_with('\n'));
    }

    // ------------------------------------------------------------------
    // Scanned-PDF heuristic (unchanged semantics).
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Retrieval conversion options (A/C/D).
    // ------------------------------------------------------------------

    #[test]
    fn retrieval_options_deviate_from_defaults_deliberately() {
        let o = retrieval_conversion_options();
        assert!(!o.include_artifacts, "running headers/footers/page numbers");
        assert!(o.expand_ligatures, "ﬁ/ﬂ must become fi/fl for BM25");
        assert!(
            !o.annotate_skipped_pages,
            "[OCR REQUIRED] markers must not be indexed as content"
        );
    }

    /// Regression guard for a *deliberate* non-deviation. This flag deletes
    /// body text from multi-column documents (it matches glyph-run spans, not
    /// lines, inside the top/bottom 15% band, where column-leading body lines
    /// live). Do not flip it without re-measuring
    /// `tests/fixtures/corpus/plos-compbio-two-column.pdf` — see the comment
    /// in `retrieval_conversion_options`.
    #[test]
    fn retrieval_options_do_not_strip_running_headers_geometrically() {
        assert!(!retrieval_conversion_options().strip_running_headers_footers);
    }

    // ------------------------------------------------------------------
    // Page-range formatting for the textless-page warning (D).
    // ------------------------------------------------------------------

    #[test]
    fn format_page_ranges_collapses_runs() {
        assert_eq!(format_page_ranges(&[3, 4, 5, 9]), "3-5, 9");
        assert_eq!(format_page_ranges(&[1]), "1");
        assert_eq!(format_page_ranges(&[1, 3, 5]), "1, 3, 5");
        assert_eq!(format_page_ranges(&[1, 2, 3, 4]), "1-4");
        assert_eq!(format_page_ranges(&[]), "");
        assert_eq!(format_page_ranges(&[2, 3, 7, 8, 9, 20]), "2-3, 7-9, 20");
    }

    // ------------------------------------------------------------------
    // Soft-hyphen stripping (B).
    // ------------------------------------------------------------------

    #[test]
    fn strip_soft_hyphens_removes_mid_word() {
        assert_eq!(strip_soft_hyphens("reconstruc\u{00AD}ted"), "reconstructed");
        assert_eq!(
            strip_soft_hyphens("su\u{00AD}per\u{00AD}cali\u{00AD}fragilistic"),
            "supercalifragilistic"
        );
    }

    #[test]
    fn strip_soft_hyphens_handles_boundaries() {
        assert_eq!(strip_soft_hyphens("\u{00AD}leading"), "leading");
        assert_eq!(strip_soft_hyphens("trailing\u{00AD}"), "trailing");
        assert_eq!(strip_soft_hyphens("\u{00AD}"), "");
        assert_eq!(strip_soft_hyphens(""), "");
    }

    /// The "leaves it alone" case matters more than the "catches it" case:
    /// a false positive would corrupt every well-formed PDF.
    #[test]
    fn strip_soft_hyphens_is_a_borrowing_no_op_when_absent() {
        let input = "no soft hyphens here at all";
        let result = strip_soft_hyphens(input);
        assert_eq!(result, input);
        assert!(matches!(result, Cow::Borrowed(_)), "must not allocate");
    }

    #[test]
    fn strip_soft_hyphens_spares_real_hyphens() {
        // hyphen-minus U+002D and HYPHEN U+2010 carry meaning; only the
        // discretionary U+00AD is a rendering hint.
        let input = "well-being and co\u{2010}operate";
        let result = strip_soft_hyphens(input);
        assert_eq!(result, input);
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn strip_soft_hyphens_is_char_safe_on_non_ascii() {
        assert_eq!(
            strip_soft_hyphens("café\u{00AD}au\u{00AD}lait 日本\u{00AD}語"),
            "caféaulait 日本語"
        );
    }

    // ------------------------------------------------------------------
    // E: spurious-heading demotion.
    //
    // The KEPT cases matter more than the DEMOTED ones: a false positive
    // silently strips breadcrumbs from every well-formed PDF, while a false
    // negative merely preserves today's behavior.
    // ------------------------------------------------------------------

    fn demoted(md: &str) -> String {
        demote_spurious_headings(md)
    }

    #[test]
    fn heading_guard_keeps_well_formed_headings() {
        for kept in [
            "# Chapter Three: The Reckoning",
            "## Fuzzing the CAN Bus",
            "## PART ONE: NON-CONTRADICTION",
            "# Section 2.1 Overview",
            "# Dr. Watson's Method",
            "# Café Society",
            "# 第三章",
            "# Are We There Yet?",
            "# Run!",
            "### The Way We Were",
            "## Just Say No",
            "# Who's Who",
        ] {
            assert_eq!(demoted(kept), kept, "must stay a heading: {kept:?}");
        }
    }

    /// Every case here was found by an adversarial review that ran them
    /// against an earlier version of this guard and got them demoted. They
    /// are ordinary headings from real document genres; each one is a
    /// breadcrumb lost on every PDF of that kind, so they stay pinned.
    #[test]
    fn heading_guard_keeps_headings_that_earlier_versions_wrongly_demoted() {
        for kept in [
            // Camel-cased brands and scientific notation (starts_lowercase).
            "# iOS 17: What's New",
            "# eBay for Beginners",
            "# pH Regulation in Reef Aquariums",
            "# mRNA Vaccines: Mechanism and Application",
            "# cAMP Signaling Pathway",
            "# macOS Ventura Setup Guide",
            // Lowercase particles and alphanumeric prefixes.
            "# von Neumann Architecture",
            "# de Broglie Wavelength",
            "# x86 Assembly Language Basics",
            // Single-letter and initialism labels (closed-class tail words).
            "# Appendix A",
            "# Exhibit A",
            "# Schedule A",
            "# Annex A",
            "# Vitamin A",
            "# Hepatitis A",
            "# Plan A",
            "# Grade A",
            "# Model A",
            "# Anesthesia in the OR",
            "# Logical Operators: AND, OR",
            // Spaced initials (has_multiple_sentences).
            "# U. S. Grant: General and President",
            "# On Fairy-Stories by J. R. R. Tolkien",
            "# C. S. Lewis and the Inklings",
            // Shell markers mentioned rather than used.
            "# Shell Scripting Basics: #!/bin/bash Explained",
            "# Navigating C:\\Windows and System Files",
            "# Financing the US$ 2 Trillion Gap",
            // Callout headings ending in a colon.
            "# Note:",
            "# Warning:",
            "# Caution:",
            "# In This Chapter:",
            // Abbreviations that must not read as sentence boundaries.
            "# Fig. 3. Results and Discussion",
            "# U.S. Foreign Policy in the 20th Century",
        ] {
            assert_eq!(demoted(kept), kept, "must stay a heading: {kept:?}");
        }
    }

    /// A real legal-filing title: 160 chars, one line, genuinely a heading.
    #[test]
    fn heading_guard_keeps_long_legal_and_academic_titles() {
        let merger = "# AGREEMENT AND PLAN OF MERGER BY AND AMONG ACME CORPORATION, A DELAWARE CORPORATION, AND WIDGET HOLDINGS, INC., A NEVADA CORPORATION, DATED AS OF JANUARY 1, 2024";
        assert_eq!(demoted(merger), merger);
    }

    #[test]
    fn heading_guard_keeps_all_six_levels() {
        for level in 1..=6 {
            let md = format!("{} Short Clean Title", "#".repeat(level));
            assert_eq!(demoted(&md), md, "level {level} must survive");
        }
    }

    #[test]
    fn heading_guard_demotes_overlong_heading() {
        // Capitalised, single sentence, no dangling tail, no shell marker, so
        // only MAX_HEADING_LEN can fire. An earlier version used 40 lowercase
        // repetitions totalling 199 chars: under the cap, and demoted only via
        // `starts_lowercase` — leaving this branch covered by nothing.
        let body = "Word ".repeat(60);
        assert!(
            body.trim_end().chars().count() > MAX_HEADING_LEN,
            "fixture must actually exceed the cap"
        );
        let out = demoted(&format!("# {body}"));
        assert!(
            !out.starts_with('#'),
            "overlong heading must demote: {out:?}"
        );
        assert!(out.contains("Word"), "text must survive demotion");

        // A heading comfortably under the cap is kept: pins the other side.
        let short = format!("# {}", "Word ".repeat(20));
        let short = short.trim_end();
        assert_eq!(demoted(short), short, "under-cap heading must survive");
    }

    #[test]
    fn heading_guard_demotes_multi_sentence_heading() {
        let md = "# The scan completed. Nothing was found. We moved on.";
        assert_eq!(
            demoted(md),
            "The scan completed. Nothing was found. We moved on."
        );
    }

    #[test]
    fn heading_guard_demotes_dangling_continuations() {
        assert_eq!(
            demoted("# Configuring the network and"),
            "Configuring the network and"
        );
        assert_eq!(demoted("# Results for the year,"), "Results for the year,");
        assert_eq!(demoted("# A comparison of"), "A comparison of");
    }

    #[test]
    fn heading_guard_demotes_lowercase_initial() {
        assert_eq!(
            demoted("# the rest of the paragraph"),
            "the rest of the paragraph"
        );
    }

    #[test]
    fn heading_guard_demotes_shell_transcript() {
        let md = "# msf exploit(ms08_067_netapi) > set RHOST 192.168.1.10";
        assert!(!demoted(md).starts_with('#'), "transcript must demote");
        assert_eq!(demoted("# C:\\Users\\admin> dir"), "C:\\Users\\admin> dir");
    }

    /// A `#` inside a fenced block is code, not a heading — touching it would
    /// corrupt the block.
    #[test]
    fn heading_guard_ignores_hashes_inside_fences() {
        let md = "```\n# this is a shell comment, not a heading\n```\n";
        assert_eq!(demoted(md), md);
        let tilde = "~~~\n# also not a heading\n~~~\n";
        assert_eq!(demoted(tilde), tilde);
    }

    #[test]
    fn heading_guard_ignores_non_atx_hashes() {
        // No space after the hashes, 7 hashes, and a mid-line hash.
        for untouched in ["#NotAHeading", "####### seven hashes", "text # mid-line"] {
            assert_eq!(demoted(untouched), untouched);
        }
    }

    #[test]
    fn heading_guard_preserves_document_shape() {
        let md = "# Good Title\n\nBody text here.\n\n## Another Good One\n\nMore body.\n";
        assert_eq!(
            demoted(md),
            md,
            "trailing newline and blank lines preserved"
        );
    }

    // ------------------------------------------------------------------
    // F: prose wrongly fenced as code.
    //
    // Both directions are pinned. Mis-un-fencing a real terminal transcript
    // is worse than leaving one novel's dialogue mislabeled, so the KEPT
    // cases are the load-bearing ones.
    // ------------------------------------------------------------------

    fn fenced(body: &str) -> String {
        format!("```\n{body}\n```\n")
    }

    #[test]
    fn code_guard_unfences_novel_dialogue() {
        let body = "\"I don't know what you mean,\" she said. \"It has never been a question of what I wanted.\"\n\"Then what was it a question of?\" he asked, not looking up from the ledger.\n\"Of what was right,\" she said, \"and I have not changed my mind about that, not once.\"";
        let out = demote_prose_fenced_as_code(&fenced(body));
        assert_eq!(out, format!("{body}\n"));
        assert!(!out.contains("```"), "fence must be gone");
    }

    #[test]
    fn code_guard_unfences_plain_narrative() {
        let body = "The morning came up gray over the harbor and nobody on the docks seemed to notice or to care. Ships waited at anchor for a tide that would not turn for hours yet, and the men who worked them stood in small groups smoking and talking about nothing in particular.";
        assert_eq!(
            demote_prose_fenced_as_code(&fenced(body)),
            format!("{body}\n")
        );
    }

    #[test]
    fn code_guard_keeps_terminal_transcript() {
        let body = "$ msfconsole\nmsf > use exploit/windows/smb/ms08_067_netapi\nmsf exploit(ms08_067_netapi) > set RHOST 192.168.1.10\nmsf exploit(ms08_067_netapi) > exploit";
        let md = fenced(body);
        assert_eq!(demote_prose_fenced_as_code(&md), md);
    }

    #[test]
    fn code_guard_keeps_real_code() {
        let c = "int main(int argc, char **argv) {\n    if (argc < 2) {\n        return 1;\n    }\n    return 0;\n}";
        let py = "def greet(name):\n    if not name:\n        return None\n    return f\"Hello, {name}!\"";
        let sql = "SELECT id, name, email\nFROM users\nWHERE created_at > '2024-01-01';";
        let sh = "find . -name \"*.tmp\" -exec rm -f {} \\;";
        let cfg = "host: localhost\nport: 8080\ntimeout: 30s\nretries: 3";
        let can = "123#DEADBEEF12345678\n124#0011223344556677\n125#AABBCCDD";
        for body in [c, py, sql, sh, cfg, can] {
            let md = fenced(body);
            assert_eq!(
                demote_prose_fenced_as_code(&md),
                md,
                "must stay code: {body:?}"
            );
        }
    }

    /// pdf_oxide's monospace fencer emits no info string, so a tagged fence
    /// is never ours to touch — however prose-shaped its content.
    #[test]
    fn code_guard_never_touches_language_tagged_fences() {
        let body = "\"I don't know what you mean,\" she said. \"It has never been a question of what I wanted, not for a single moment of my whole life.\"";
        for tag in ["rust", "bash", "text"] {
            let md = format!("```{tag}\n{body}\n```\n");
            assert_eq!(demote_prose_fenced_as_code(&md), md, "tagged fence {tag}");
        }
    }

    /// One code signal blocks demotion however much prose surrounds it —
    /// the guard requires prose dominance AND zero code signals.
    #[test]
    fn code_guard_keeps_mostly_prose_block_with_one_command() {
        let body = "The next step in the exploitation process was straightforward enough once the target was identified.\n$ nmap -sV 192.168.1.10\nAfter that, the attacker simply waited for the scan to complete before moving on.";
        let md = fenced(body);
        assert_eq!(demote_prose_fenced_as_code(&md), md);
    }

    #[test]
    fn code_guard_keeps_commented_code_and_docstrings() {
        let commented = "// This function calculates the total price for the customer,\n// taking into account any discounts that might apply to the order.\nfn total_price(items: &[Item]) -> f64 {\n    items.iter().map(|i| i.price).sum()\n}";
        let docstring = "def process(data):\n    \"\"\"Process the given data and return a cleaned result.\n\n    This does not mutate the input in place.\n    \"\"\"\n    return [d.strip() for d in data if d]";
        for body in [commented, docstring] {
            let md = fenced(body);
            assert_eq!(demote_prose_fenced_as_code(&md), md);
        }
    }

    #[test]
    fn code_guard_leaves_ambiguous_fences_alone() {
        let prose = "\"I don't know what you mean,\" she said. \"It has never been a question of what I wanted, not for a single moment.\"";
        // Unclosed fence.
        let unclosed = format!("```\n{prose}\n");
        assert_eq!(demote_prose_fenced_as_code(&unclosed), unclosed);
        // Indented fence.
        let indented = format!("  ```\n  {prose}\n  ```\n");
        assert_eq!(demote_prose_fenced_as_code(&indented), indented);
        // Too little content to judge.
        let short = fenced("\"No,\" she said.");
        assert_eq!(demote_prose_fenced_as_code(&short), short);
    }

    #[test]
    fn code_guard_preserves_surrounding_content() {
        let prose = "She walked slowly down the corridor, thinking about everything that had happened that terrible, wonderful year, and about nothing at all.";
        let code = "def f(x):\n    return x + 1";
        let md = format!("Intro paragraph.\n\n```\n{prose}\n```\n\n```\n{code}\n```\n\nOutro.\n");
        let expected = format!("Intro paragraph.\n\n{prose}\n\n```\n{code}\n```\n\nOutro.\n");
        assert_eq!(demote_prose_fenced_as_code(&md), expected);
    }

    #[test]
    fn code_guard_is_byte_safe_on_non_ascii_and_crlf() {
        let body = "Elle marchait lentement dans le couloir, pensant à tout ce qui était arrivé cette année terrible et merveilleuse. C'était étrange, se disait-elle, comme une seule journée pouvait contenir tant de peine et tant de joie.";
        assert_eq!(
            demote_prose_fenced_as_code(&fenced(body)),
            format!("{body}\n")
        );
        // CRLF must not panic.
        let crlf = "```\r\n\"I don't know,\" she said softly.\r\n```\r\n";
        let _ = demote_prose_fenced_as_code(crlf);
    }

    /// The whole per-page pipeline composes without the stages fighting.
    #[test]
    fn postprocess_pipeline_composes() {
        let md = "# Chapter One: The Beginning\n\nA recon\u{00AD}struction of the year.\n";
        let out = postprocess_page_markdown(md);
        assert!(out.contains("# Chapter One: The Beginning"), "heading kept");
        assert!(out.contains("reconstruction"), "soft hyphen removed");
        assert!(!out.contains('\u{00AD}'));
    }

    // ------------------------------------------------------------------
    // G: Dublin Core metadata.
    // ------------------------------------------------------------------

    #[test]
    fn parse_pdf_date_full_precision() {
        assert_eq!(
            parse_pdf_date("D:19570102153000+05'30'").as_deref(),
            Some("1957-01-02")
        );
        assert_eq!(parse_pdf_date("D:19570102").as_deref(), Some("1957-01-02"));
        assert_eq!(
            parse_pdf_date("D:20180304090000Z").as_deref(),
            Some("2018-03-04")
        );
    }

    /// W3CDTF (which Dublin Core's date element is defined against) treats
    /// truncated dates as first-class, so a year-only `/CreationDate` — often
    /// the only date an old scanner wrote — is kept rather than discarded.
    #[test]
    fn parse_pdf_date_partial_precision() {
        assert_eq!(parse_pdf_date("D:1957").as_deref(), Some("1957"));
        assert_eq!(parse_pdf_date("D:195701").as_deref(), Some("1957-01"));
    }

    /// On any malformed input the field is left empty rather than storing the
    /// raw `D:…` string — never surface a guess as data.
    #[test]
    fn parse_pdf_date_rejects_malformed() {
        for bad in [
            "",
            "1957",              // missing the mandatory D: prefix
            "D:",                // no year
            "D:195",             // short year
            "D:19571301",        // month 13
            "D:19570100",        // day 0
            "D:19570132",        // day 32
            "D:19570102253000",  // hour 25
            "D:19570102153000Q", // bad timezone sign
            "D:not-a-date",
            "D:19570102153000+05'30'trailing-garbage",
        ] {
            assert_eq!(parse_pdf_date(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn split_keywords_splits_on_comma_and_semicolon() {
        assert_eq!(
            split_keywords("alpha, beta;gamma ,, delta "),
            vec!["alpha", "beta", "gamma", "delta"]
        );
        assert!(split_keywords("  ,; ").is_empty());
    }

    #[test]
    fn xmp_date_truncates_to_date() {
        assert_eq!(xmp_date_to_iso_date("2018-03-04T09:00:00Z"), "2018-03-04");
        assert_eq!(xmp_date_to_iso_date("2018-03-04"), "2018-03-04");
    }

    /// The Info dictionary wins per field; XMP fills only what Info lacks.
    #[test]
    fn metadata_info_dict_wins_over_xmp() {
        let ex = extract_pdf(&fixture("metadata_full.pdf")).expect("fixture must extract");
        let m = &ex.metadata;
        // XMP carries a conflicting value for every one of these; Info wins.
        assert_eq!(m.title.as_deref(), Some("Info Title"));
        assert_eq!(m.creator, vec!["Info Author".to_string()]);
        assert_eq!(m.description.as_deref(), Some("Info Subject text"));
        assert_eq!(
            m.subject,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
        assert_eq!(m.date.as_deref(), Some("2021-01-02"));
        // No Info-dict equivalent, so these come from XMP.
        assert_eq!(m.language.as_deref(), Some("en"));
        assert_eq!(m.rights.as_deref(), Some("XMP Rights statement"));
        // `/Producer` is the generating *software*, never the publisher.
        assert_eq!(m.publisher, None);
        // `format` is the probe's job, not the document's.
        assert_eq!(m.format, None);
    }

    #[test]
    fn metadata_falls_back_to_xmp_per_field() {
        let ex = extract_pdf(&fixture("metadata_xmp_fallback.pdf")).expect("fixture must extract");
        let m = &ex.metadata;
        // `/Title` is the only Info key present, so it still wins...
        assert_eq!(m.title.as_deref(), Some("Fallback Fixture Title"));
        // ...and every other field falls back to XMP, field by field.
        assert_eq!(m.creator, vec!["XMP Fallback Creator".to_string()]);
        assert_eq!(m.description.as_deref(), Some("XMP Fallback Description"));
        assert_eq!(
            m.subject,
            vec![
                "fallback-subject-1".to_string(),
                "fallback-subject-2".to_string()
            ]
        );
        assert_eq!(m.date.as_deref(), Some("2018-03-04"));
        assert_eq!(m.language.as_deref(), Some("fr"));
        assert_eq!(m.rights.as_deref(), Some("XMP Fallback Rights"));
    }

    /// The deliberate non-goal: no filename or first-page fallback, so a PDF
    /// with neither `/Title` nor XMP still has no title.
    #[test]
    fn metadata_absent_stays_absent() {
        let ex = extract_pdf(&fixture("flat_body.pdf")).expect("flat_body must extract");
        assert_eq!(ex.metadata.title, None);
        assert!(ex.metadata.creator.is_empty());
        assert_eq!(ex.metadata.date, None);
    }

    #[test]
    fn is_scanned_pdf_detects_empty_text() {
        assert!(is_scanned_pdf(""));
        assert!(is_scanned_pdf("   \n  \t  \n"));
    }

    #[test]
    fn is_scanned_pdf_accepts_real_text() {
        let real_text = "This is a real paragraph with meaningful text content. \
                         It has many words and sentences that indicate a real document.";
        assert!(!is_scanned_pdf(real_text));
    }

    #[test]
    fn decode_pdf_text_string_handles_utf16be_bom() {
        let bytes = [0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69]; // "Hi"
        assert_eq!(decode_pdf_text_string(&bytes), "Hi");
    }

    #[test]
    fn decode_pdf_text_string_handles_plain_ascii() {
        assert_eq!(decode_pdf_text_string(b"Plain Title"), "Plain Title");
    }

    #[test]
    fn decode_pdf_text_string_handles_high_latin1_bytes() {
        // 0xE9 = é: PDFDocEncoding matches Latin-1 for 0xA1..=0xFF. Invalid as
        // standalone UTF-8, so this exercises the PDFDocEncoding fallback.
        assert_eq!(decode_pdf_text_string(&[0x63, 0x61, 0x66, 0xE9]), "café");
    }

    #[test]
    fn decode_pdf_text_string_maps_pdfdocencoding_high_block() {
        // 0x80 is a bullet in PDFDocEncoding, NOT the Latin-1 control U+0080.
        assert_eq!(decode_pdf_text_string(&[0x80]), "•");
        // 0xA0 is the Euro sign in PDFDocEncoding.
        assert_eq!(decode_pdf_text_string(&[0xA0]), "€");
        // A word using the fi-ligature slot (0x93).
        assert_eq!(decode_pdf_text_string(&[0x93]), "ﬁ");
    }

    #[test]
    fn pdf_doc_encoding_char_ascii_and_undefined() {
        assert_eq!(pdf_doc_encoding_char(b'A'), 'A');
        assert_eq!(pdf_doc_encoding_char(0xAD), '\u{FFFD}'); // undefined slot
        assert_eq!(pdf_doc_encoding_char(0x9F), '\u{FFFD}'); // undefined slot
        assert_eq!(pdf_doc_encoding_char(0x7F), '\u{FFFD}'); // undefined slot
    }

    #[test]
    fn pdf_doc_encoding_char_low_accent_modifiers() {
        // The 0x18..=0x1F block: accent modifiers, NOT the C0 controls that a
        // straight Latin-1 cast (or a UTF-8 fast path) would produce.
        assert_eq!(pdf_doc_encoding_char(0x18), '\u{02D8}'); // BREVE
        assert_eq!(pdf_doc_encoding_char(0x19), '\u{02C7}'); // CARON
        assert_eq!(pdf_doc_encoding_char(0x1A), '\u{02C6}'); // MODIFIER CIRCUMFLEX
        assert_eq!(pdf_doc_encoding_char(0x1B), '\u{02D9}'); // DOT ABOVE
        assert_eq!(pdf_doc_encoding_char(0x1C), '\u{02DD}'); // DOUBLE ACUTE
        assert_eq!(pdf_doc_encoding_char(0x1D), '\u{02DB}'); // OGONEK
        assert_eq!(pdf_doc_encoding_char(0x1E), '\u{02DA}'); // RING ABOVE
        assert_eq!(pdf_doc_encoding_char(0x1F), '\u{02DC}'); // SMALL TILDE
    }

    #[test]
    fn decode_pdf_text_string_low_control_routes_to_pdfdocencoding() {
        // 0x18 is valid ASCII, so `from_utf8` would accept it and return
        // U+0018 — the exact shadowing bug. The guard must route it to the
        // PDFDocEncoding table, yielding U+02D8 (BREVE), not U+0018.
        let decoded = decode_pdf_text_string(&[0x18]);
        assert_eq!(decoded, "\u{02D8}");
        assert_ne!(decoded, "\u{0018}");
    }

    #[test]
    fn decode_pdf_text_string_honors_utf8_bom() {
        // PDF 2.0 EF BB BF UTF-8 BOM: strip it and decode the remainder.
        let bytes = [0xEF, 0xBB, 0xBF, 0x63, 0x61, 0x66, 0xC3, 0xA9]; // "café"
        assert_eq!(decode_pdf_text_string(&bytes), "café");
    }

    #[test]
    fn decode_pdf_text_string_keeps_utf8_without_bom() {
        // Common producer behavior: UTF-8 without BOM and without C0 controls
        // must survive via the fast path, not become mojibake.
        assert_eq!(
            decode_pdf_text_string(&[0x63, 0x61, 0x66, 0xC3, 0xA9]),
            "café"
        );
    }
}
