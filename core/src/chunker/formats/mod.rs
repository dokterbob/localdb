//! Per-source-format chunking implementations, dispatched from `chunk_blocks`.

pub(in crate::chunker) mod code;
pub(in crate::chunker) mod messages;
pub(in crate::chunker) mod prose;
pub(in crate::chunker) mod table;
