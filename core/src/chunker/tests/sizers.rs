//! ChunkSizer implementation tests.

use std::sync::Arc;

use crate::chunker::sizers::TsSizer;
use crate::chunker::{CharSizer, ChunkSizer, TokenSizer};

#[test]
fn char_sizer_counts_scalar_values_not_bytes() {
    assert_eq!(CharSizer.size("abc"), 3);
    assert_eq!(CharSizer.size("héllo"), 5); // 6 bytes, 5 chars
    assert_eq!(CharSizer.size(""), 0);
}

#[test]
fn token_sizer_delegates_to_closure() {
    let sizer = TokenSizer::new(Arc::new(|t: &str| t.split_whitespace().count()));
    assert_eq!(sizer.size("one two three"), 3);
    assert_eq!(sizer.size(""), 0);
    let clone = sizer.clone();
    assert_eq!(clone.size("just one two"), 3);
}

#[test]
fn ts_sizer_bridges_to_text_splitter_trait() {
    let inner = TokenSizer::new(Arc::new(|t: &str| t.len()));
    let bridge = TsSizer(&inner);
    assert_eq!(text_splitter::ChunkSizer::size(&bridge, "abcd"), 4);
}
