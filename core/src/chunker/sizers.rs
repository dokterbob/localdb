//! Pluggable chunk-size metrics (tokens or chars).

use std::sync::Arc;

/// A pluggable size metric for chunking.
///
/// `CharSizer` counts characters; `TokenSizer` wraps a model tokenizer's
/// token-counting closure. This is *our* trait (not `text-splitter`'s) so the
/// `text-splitter` dependency never leaks through the public API.
pub trait ChunkSizer: Send + Sync {
    /// Return the size of `text` in this metric's units.
    fn size(&self, text: &str) -> usize;
}

/// Sizer that counts Unicode scalar values (characters).
pub struct CharSizer;

impl ChunkSizer for CharSizer {
    fn size(&self, t: &str) -> usize {
        t.chars().count()
    }
}

/// Sizer backed by a token-counting closure (e.g. a model tokenizer).
#[derive(Clone)]
pub struct TokenSizer(Arc<dyn Fn(&str) -> usize + Send + Sync>);

impl TokenSizer {
    /// Build a `TokenSizer` from a token-counting closure.
    pub fn new(f: Arc<dyn Fn(&str) -> usize + Send + Sync>) -> Self {
        Self(f)
    }
}

impl ChunkSizer for TokenSizer {
    fn size(&self, t: &str) -> usize {
        (self.0)(t)
    }
}

/// Internal newtype bridging *our* `ChunkSizer` to `text-splitter`'s trait.
pub(in crate::chunker) struct TsSizer<'a>(pub(in crate::chunker) &'a dyn ChunkSizer);

impl text_splitter::ChunkSizer for TsSizer<'_> {
    fn size(&self, chunk: &str) -> usize {
        self.0.size(chunk)
    }
}
