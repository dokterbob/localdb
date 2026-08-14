//! Source-kind unit test modules.

// `pub(in crate::source)` (here and on kinds/mod.rs's `mod tests;`) rather than
// private: `source/tests/dispatch.rs` lives in a sibling subtree and reaches
// `common`'s helpers only if every hop in the path is visible to `crate::source`.
pub(in crate::source) mod common;
mod feed;
mod path;
