//! `ChunkerConfig` — configuration for the chunking operation.

use crate::Error;

/// Configuration for the chunking operation.
#[derive(Debug, Clone)]
pub struct ChunkerConfig {
    /// Preset name: "prose", "code", or "messages".
    pub preset: String,
    /// Target chunk size in tokens. `None` = preset default.
    ///
    /// For the `prose` preset this is interpreted by the active `ChunkSizer`
    /// (tokens for `TokenSizer`, chars for `CharSizer`). For the `code` preset
    /// it is still interpreted as a character budget (interim line packer).
    pub target_tokens: Option<usize>,
    /// Overlap in tokens. `None` = preset default.
    pub overlap_tokens: Option<usize>,
    /// Number of message turns per sliding window (messages preset). `None` = default (6).
    pub window_turns: Option<usize>,
    /// Stride in turns between windows (messages preset). `None` = default (3).
    pub stride_turns: Option<usize>,
}

impl ChunkerConfig {
    /// Create a config for the `prose` preset with the spec defaults.
    ///
    /// Default target ≈ 256 tokens, overlap ≈ 0 tokens. These match the
    /// contextual training regime of `pplx-embed-context-v1` (256-token chunks,
    /// no intra-document overlap — late chunking supplies cross-chunk context).
    /// See specs/04-search-pipeline.md §3.
    pub fn prose() -> Self {
        Self {
            preset: "prose".to_string(),
            target_tokens: Some(256),
            overlap_tokens: Some(0),
            window_turns: None,
            stride_turns: None,
        }
    }

    /// Create a config for the `code` preset with the spec defaults.
    ///
    /// Target ≈ 60 lines (≈3000 chars assuming ~50 chars/line average). The
    /// code path interprets these values as character counts.
    pub fn code() -> Self {
        Self {
            preset: "code".to_string(),
            target_tokens: Some(3000),
            overlap_tokens: Some(0),
            window_turns: None,
            stride_turns: None,
        }
    }

    /// Create a config for the `messages` preset with the spec defaults.
    ///
    /// Default window = 6 turns, stride = 3 turns.
    /// Token budget uses `target_tokens` (default 512) to cap windows.
    /// See specs/04-search-pipeline.md §3.
    pub fn messages() -> Self {
        Self {
            preset: "messages".to_string(),
            target_tokens: Some(512),
            overlap_tokens: Some(0),
            window_turns: Some(6),
            stride_turns: Some(3),
        }
    }

    /// Create a `ChunkerConfig` from a preset name string.
    ///
    /// Returns `Error::InvalidRequest` for unknown presets.
    pub fn from_preset(preset: &str) -> Result<Self, Error> {
        match preset {
            "prose" => Ok(Self::prose()),
            "code" => Ok(Self::code()),
            "messages" => Ok(Self::messages()),
            other => Err(Error::InvalidRequest {
                message: format!(
                    "unknown chunking preset '{}'; valid values: prose, code, messages",
                    other
                ),
            }),
        }
    }

    /// Resolved target tokens (uses preset default if not overridden).
    pub fn resolved_target_tokens(&self) -> usize {
        self.target_tokens.unwrap_or(256)
    }

    /// Resolved overlap tokens (uses preset default if not overridden).
    pub fn resolved_overlap_tokens(&self) -> usize {
        self.overlap_tokens.unwrap_or(0)
    }

    /// Resolved window turns for the messages preset (default 6).
    pub fn resolved_window_turns(&self) -> usize {
        self.window_turns.unwrap_or(6)
    }

    /// Resolved stride turns for the messages preset (default 3).
    pub fn resolved_stride_turns(&self) -> usize {
        self.stride_turns.unwrap_or(3)
    }
}
