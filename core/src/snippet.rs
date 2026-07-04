//! Boundary-aware snippet truncation.
//!
//! Search snippets are truncated for display (MCP text rendering, CLI
//! `--content-length`) at a configurable soft cap. A naive `.chars().take(n)`
//! hard cut regularly lands mid-word or mid-sentence, which reads badly and
//! can even split a multi-byte grapheme cluster awkwardly across the visual
//! cut. [`truncate_snippet`] instead snaps the cut point to the nearest
//! natural boundary — paragraph, sentence, then word — before falling back
//! to a hard character-boundary cut, per specs/05-surfaces.md §4.

/// Returns the largest byte index ≤ `index` that is a valid UTF-8 char boundary.
///
/// MSRV-safe replacement for `str::floor_char_boundary` (stable since 1.91).
/// Copied from `core/src/chunker.rs` — kept private there, so duplicated here
/// rather than exposed as shared crate-internal API for a single 8-line helper.
#[inline]
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let index = index.min(s.len());
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Trim trailing whitespace from `s`, but never return an empty string for a
/// non-empty input — protects the "never empty" guarantee against
/// pathological all-whitespace candidate windows.
fn safe_trim_end(s: &str) -> &str {
    let t = s.trim_end();
    if t.is_empty() {
        s
    } else {
        t
    }
}

/// Find the byte offset just past the last "complete sentence" end within
/// `window`, if any.
///
/// A sentence end is one of `.! ?`, optionally followed by a single closing
/// quote/bracket (`"`, `'`, `)`, `]`, `）`), followed by whitespace or the end
/// of the full text. Scans forward and keeps overwriting the candidate, so the
/// *last* valid match in `window` wins.
///
/// `next_after_window` is the char in the full text immediately following
/// `window` (i.e. `text[hi_byte..].chars().next()`), or `None` iff `window`
/// reaches the real end of the text. This lets a terminator sitting at the
/// window's trailing edge be judged against the *actual* next char rather than
/// being falsely treated as end-of-text — a terminator mid-token (e.g. the `.`
/// in `example.com`) that merely happens to land on the window edge must not be
/// accepted as a sentence end.
fn last_sentence_end(window: &str, next_after_window: Option<char>) -> Option<usize> {
    const TERMINATORS: [char; 3] = ['.', '!', '?'];
    const CLOSERS: [char; 5] = ['"', '\'', ')', ']', '）'];

    let chars: Vec<(usize, char)> = window.char_indices().collect();
    let mut best: Option<usize> = None;

    for i in 0..chars.len() {
        let (byte_pos, ch) = chars[i];
        if !TERMINATORS.contains(&ch) {
            continue;
        }
        // Byte offset just past this terminator (and its char width).
        let mut end_idx = i + 1;
        let mut end_byte = byte_pos + ch.len_utf8();

        // Optionally consume one immediately-following closer.
        if let Some(&(cb, cc)) = chars.get(end_idx) {
            if CLOSERS.contains(&cc) {
                end_idx += 1;
                end_byte = cb + cc.len_utf8();
            }
        }

        let followed_by_boundary = match chars.get(end_idx) {
            // At the window's trailing edge: resolve against the real next char
            // in the full text — only a genuine end-of-text (or trailing
            // whitespace) counts as a boundary.
            None => match next_after_window {
                None => true, // real end of text
                Some(c) => c.is_whitespace(),
            },
            Some(&(_, next_ch)) => next_ch.is_whitespace(),
        };

        if followed_by_boundary {
            best = Some(end_byte);
        }
    }

    best
}

/// Truncate `text` to approximately `soft_cap` Unicode scalar values (chars),
/// snapping the cut point to a natural boundary instead of cutting
/// mid-word/mid-sentence.
///
/// Returns `(prefix, was_truncated)` where `prefix` borrows from `text`.
///
/// Boundary search order, within the char window `[soft_cap/2, soft_cap +
/// soft_cap/5]`:
/// 1. paragraph break (`\n\n`)
/// 2. sentence terminator (`.`/`!`/`?`, optionally followed by a closing
///    quote/bracket, then whitespace or end-of-text)
/// 3. last whitespace at or before `soft_cap` (no overshoot)
/// 4. hard cut at a UTF-8 char boundary at `soft_cap` (no overshoot)
///
/// If `soft_cap >= text.chars().count()`, returns `(text, false)` — the
/// whole text, untruncated.
///
/// Guarantees:
/// - Never panics on any valid UTF-8 input.
/// - Never returns an empty prefix for non-empty `text` (for any `soft_cap >= 1`).
/// - The returned prefix is at most `soft_cap + soft_cap / 5` chars.
///
/// Callers that collapse whitespace before calling this (e.g. the CLI's
/// `format_snippet`) destroy `\n\n` paragraph breaks, so only sentence/word
/// snapping will ever fire on that path — paragraph snapping is effectively
/// MCP-only today.
///
/// `context_sentences` (an alternative sentence-count-based truncation unit)
/// is explicitly out of scope for this helper.
pub fn truncate_snippet(text: &str, soft_cap: usize) -> (&str, bool) {
    if text.is_empty() {
        return (text, false);
    }

    let char_bytes: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    let total_chars = char_bytes.len();

    if total_chars <= soft_cap {
        return (text, false);
    }

    let char_to_byte = |k: usize| -> usize {
        if k < char_bytes.len() {
            char_bytes[k]
        } else {
            text.len()
        }
    };

    let hi_chars = (soft_cap + soft_cap / 5).min(total_chars);
    let lo_chars = (soft_cap / 2).min(hi_chars);
    let lo_byte = char_to_byte(lo_chars);
    let hi_byte = char_to_byte(hi_chars);
    let window = &text[lo_byte..hi_byte];

    // 1. Paragraph break.
    if let Some(p) = window.rfind("\n\n") {
        let cut = lo_byte + p;
        if cut > 0 {
            return (safe_trim_end(&text[..cut]), true);
        }
    }

    // 2. Sentence terminator.
    let next_after_window = text[hi_byte..].chars().next();
    if let Some(end) = last_sentence_end(window, next_after_window) {
        let cut = lo_byte + end;
        if cut > 0 {
            return (safe_trim_end(&text[..cut]), true);
        }
    }

    // 3. Word boundary (no overshoot past soft_cap). Search up to and including
    // char-index `soft_cap` so a whitespace sitting exactly at the cap (which
    // yields a prefix of exactly `soft_cap` chars — no overshoot) is a valid
    // cut, not needlessly discarded in favour of the previous whitespace.
    let cap_byte = char_to_byte(soft_cap);
    let word_search_byte = char_to_byte((soft_cap + 1).min(total_chars));
    if let Some(w) = text[..word_search_byte].rfind(|c: char| c.is_whitespace()) {
        // A cut at byte `w` must not exceed `soft_cap` chars. The only offset in
        // `[cap_byte, word_search_byte)` is the whitespace at char-index
        // `soft_cap` itself, which cuts to exactly `soft_cap` chars — allowed.
        if w > 0 && w <= cap_byte {
            return (safe_trim_end(&text[..w]), true);
        }
    }

    // 4. Hard fallback at a UTF-8 char boundary.
    let hard_byte = floor_char_boundary(text, cap_byte);
    (safe_trim_end(&text[..hard_byte]), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_break_is_preferred() {
        let first = "A".repeat(40);
        let second = "this continues on and on well past the cap point here";
        let text = format!("{first}\n\n{second}");
        let (prefix, truncated) = truncate_snippet(&text, 50);
        assert!(truncated);
        assert!(prefix.ends_with(&first));
        assert!(!prefix.contains("this continues"));
    }

    #[test]
    fn sentence_snap_period_space() {
        let text = "This is sentence one. This is sentence two that keeps going and going and going further.";
        let (prefix, truncated) = truncate_snippet(text, 25);
        assert!(truncated);
        assert!(prefix.ends_with('.'));
        assert_eq!(prefix, "This is sentence one.");
    }

    #[test]
    fn sentence_snap_question_mark() {
        let text = "Is this the one? Yes it definitely is the one we were looking for all along.";
        let (prefix, truncated) = truncate_snippet(text, 20);
        assert!(truncated);
        assert!(prefix.ends_with('?'));
        assert_eq!(prefix, "Is this the one?");
    }

    #[test]
    fn sentence_snap_exclamation() {
        let text = "Watch out! There is a large rock rolling down the hill towards the village.";
        let (prefix, truncated) = truncate_snippet(text, 13);
        assert!(truncated);
        assert!(prefix.ends_with('!'));
        assert_eq!(prefix, "Watch out!");
    }

    #[test]
    fn sentence_snap_quoted_period() {
        let text =
            r#"She said "stop." Then everyone in the room went completely and utterly silent."#;
        let (prefix, truncated) = truncate_snippet(text, 18);
        assert!(truncated);
        assert!(prefix.ends_with('"'));
        assert_eq!(prefix, "She said \"stop.\"");
    }

    #[test]
    fn word_boundary_fallback_no_sentence_or_paragraph() {
        let text = "word ".repeat(100);
        let (prefix, truncated) = truncate_snippet(&text, 50);
        assert!(truncated);
        assert!(prefix.chars().count() <= 50);
        assert!(!prefix.ends_with("wor"));
        assert!(prefix.ends_with("word") || prefix.trim_end().ends_with("word"));
    }

    #[test]
    fn word_boundary_includes_whitespace_at_cap() {
        // The whitespace between "world" and "again" sits at char-index 11 ==
        // soft_cap; cutting there yields exactly 11 chars (no overshoot), so it
        // must be chosen rather than falling back to the earlier space.
        let (prefix, truncated) = truncate_snippet("hello world again", 11);
        assert_eq!(prefix, "hello world");
        assert!(truncated);
    }

    #[test]
    fn no_false_sentence_end_at_window_edge() {
        // The `.` in "example.com" lands on the window's trailing edge for
        // soft_cap=10 (window covers chars [5,12)), but the real next char is
        // 'c' (non-whitespace), so it is NOT a sentence end. The result must
        // fall back to a word boundary rather than cutting at that `.`.
        let (prefix, truncated) = truncate_snippet("see example.com/path for details", 10);
        assert!(truncated);
        assert!(
            !prefix.ends_with('.'),
            "must not cut at a mid-token period, got: {prefix:?}"
        );
        assert_eq!(prefix, "see");
    }

    #[test]
    fn hard_fallback_on_unbroken_token() {
        let text = "a".repeat(1000);
        let (prefix, truncated) = truncate_snippet(&text, 50);
        assert!(truncated);
        assert_eq!(prefix.chars().count(), 50);
        assert_eq!(prefix, "a".repeat(50));
    }

    #[test]
    fn multibyte_safety_accented() {
        let text = format!("{}{}", "é".repeat(60), "more text after the accents here");
        for cap in [1, 10, 30, 50, 65] {
            let (prefix, _truncated) = truncate_snippet(&text, cap);
            assert!(prefix.chars().count() <= cap + cap / 5);
            assert!(!prefix.is_empty());
        }
    }

    #[test]
    fn multibyte_safety_emoji() {
        let text = format!(
            "{}{}",
            "🎉".repeat(60),
            "more text after the party emoji run"
        );
        for cap in [1, 10, 30, 50, 65] {
            let (prefix, _truncated) = truncate_snippet(&text, cap);
            assert!(prefix.chars().count() <= cap + cap / 5);
            assert!(!prefix.is_empty());
        }
    }

    #[test]
    fn multibyte_safety_cjk() {
        let text = format!(
            "{}{}",
            "漢字".repeat(60),
            "more text after the cjk run here"
        );
        for cap in [1, 10, 30, 50, 65] {
            let (prefix, _truncated) = truncate_snippet(&text, cap);
            assert!(prefix.chars().count() <= cap + cap / 5);
            assert!(!prefix.is_empty());
        }
    }

    #[test]
    fn soft_cap_ge_len_is_untruncated() {
        let text = "short text here";
        let total = text.chars().count();
        let (prefix, truncated) = truncate_snippet(text, total);
        assert_eq!(prefix, text);
        assert!(!truncated);

        let (prefix, truncated) = truncate_snippet(text, total + 10);
        assert_eq!(prefix, text);
        assert!(!truncated);
    }

    #[test]
    fn soft_cap_of_one() {
        let text = "hello world this is a longer sentence than one char.";
        let (prefix, truncated) = truncate_snippet(text, 1);
        assert!(truncated);
        assert!(!prefix.is_empty());
    }

    #[test]
    fn empty_input() {
        for cap in [0, 1, 5, 400] {
            let (prefix, truncated) = truncate_snippet("", cap);
            assert_eq!(prefix, "");
            assert!(!truncated);
        }
    }

    #[test]
    fn pseudo_property_guarantees() {
        let fragments = [
            "The quick brown fox jumps over the lazy dog. ",
            "Another sentence follows here, with a comma! ",
            "漢字の文章がここに続きます。これはテストです。",
            "🎉🎊🎈 party time all the time every single day 🎉🎊🎈 ",
            "Mixed café naïve résumé with accents and more words. ",
            "\n\nA fresh paragraph starts here after a break.\n\n",
            "word ",
            "",
            "a",
            "   leading and trailing whitespace text follows here   ",
        ];

        let mut strings: Vec<String> = Vec::new();
        for i in 0..fragments.len() {
            let mut s = String::new();
            for j in 0..=i {
                s.push_str(fragments[j]);
                s.push_str(fragments[(i + j) % fragments.len()]);
            }
            strings.push(s);
        }

        let caps = [1usize, 5, 50, 400];

        for s in &strings {
            let len = s.chars().count();
            let mut test_caps = caps.to_vec();
            if len > 0 {
                test_caps.push(len.saturating_sub(1).max(1));
                test_caps.push(len);
                test_caps.push(len + 1);
            }
            for &cap in &test_caps {
                let (prefix, _truncated) = truncate_snippet(s, cap);
                if !s.is_empty() {
                    assert!(!prefix.is_empty(), "empty prefix for cap={cap} on {s:?}");
                }
                assert!(
                    prefix.chars().count() <= cap + cap / 5,
                    "prefix too long for cap={cap}: {} chars, text={s:?}",
                    prefix.chars().count()
                );
            }
        }
    }
}
