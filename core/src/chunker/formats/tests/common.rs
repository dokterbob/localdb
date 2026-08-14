//! Shared test helpers for chunker format tests.

use crate::chunker::{ChunkOutput, ChunkSizer};

/// Word-count sizer for tests — no model download required.
pub(in crate::chunker) struct WordSizer;
impl ChunkSizer for WordSizer {
    fn size(&self, t: &str) -> usize {
        t.split_whitespace().count()
    }
}

/// Returns the char immediately preceding byte offset `pos` in `s`, if any.
pub(in crate::chunker) fn char_before(s: &str, pos: usize) -> Option<char> {
    s[..pos].chars().next_back()
}

/// Returns the char starting at byte offset `pos` in `s`, if any.
pub(in crate::chunker) fn char_at(s: &str, pos: usize) -> Option<char> {
    s[pos..].chars().next()
}

/// Asserts that no chunk boundary in `chunks` splits a run of alphanumeric
/// characters in `source` (a "mid-word split", #191). A boundary is a
/// mid-word split when the char immediately on one side of it and the
/// char immediately on the other side are both alphanumeric.
///
/// Deliberate scope: only alphanumeric-to-alphanumeric boundaries are flagged.
/// A split at a hyphen or apostrophe ("well-|known", "don|'t") passes silently,
/// since the flanking punctuation is not alphanumeric.
pub(in crate::chunker) fn assert_no_mid_word_splits(source: &str, chunks: &[ChunkOutput]) {
    for c in chunks {
        let start = c.span.start;
        let end = c.span.end;
        if let (Some(prev), Some(first)) = (char_before(source, start), char_at(source, start)) {
            assert!(
                !(prev.is_alphanumeric() && first.is_alphanumeric()),
                "mid-word split at chunk start (byte {start}): preceding char {prev:?}, \
                 chunk's first char {first:?}"
            );
        }
        if let (Some(last), Some(next)) = (char_before(source, end), char_at(source, end)) {
            assert!(
                !(last.is_alphanumeric() && next.is_alphanumeric()),
                "mid-word split at chunk end (byte {end}): chunk's last char {last:?}, \
                 following char {next:?}"
            );
        }
    }
}
