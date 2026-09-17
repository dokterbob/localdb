//! Citation model — the canonical result shape every surface uses.
//!
//! See specs/02-domain-model.md §6.
//!
//! Every search hit, on every surface (HTTP, CLI, MCP), resolves to this structure.

use serde::{Deserialize, Serialize};

use crate::ids::{ContentId, UlidId};
use crate::metadata::Metadata;
use crate::types::Span;

/// A store reference embedded in a citation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitationStore {
    /// Store ID (ULID).
    pub id: UlidId,
    /// Store name.
    pub name: String,
}

/// Per-leg scores for the hybrid search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Score {
    /// Fused RRF score (primary ranking key).
    pub fused: f64,
    /// Dense (vector similarity) leg score.
    #[serde(default)]
    pub dense: Option<f64>,
    /// BM25 leg score.
    #[serde(default)]
    pub bm25: Option<f64>,
}

/// Provenance summary for a citation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitationProvenance {
    /// Acquisition time (RFC 3339 string).
    pub fetched_at: String,
    /// blake3 content hash of normalized text (hex string).
    pub content_hash: String,
}

/// The block a citation's chunk originated from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitationBlock {
    /// Block sequence number within the resource (0-indexed).
    pub seq: u32,
    /// Block kind string (e.g. "text", "heading").
    ///
    /// `None` for chunks indexed before the Resource/Block architecture.
    #[serde(default)]
    pub kind: Option<String>,
    /// 1-indexed page number for paginated source formats (#103, today PDF).
    /// `None` for non-paginated formats and pre-page-plumbing chunks. Omitted
    /// from JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
}

/// The chunk's position within its parent block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkPosition {
    /// Chunk position within the block (0-indexed).
    pub seq_in_block: u32,
}

/// Refined sub-block location for a citation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitationLocation {
    /// Block-relative byte offsets into the parent block's text.
    pub span: Span,
    /// For message-window chunks (#129): all block seqs participating in the
    /// window. Omitted from JSON entirely when empty (single-block chunks).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub window_block_seqs: Vec<u32>,
}

/// The canonical result shape every surface uses.
///
/// Not a stored entity — it is a view over Chunk + Document.
///
/// See specs/02-domain-model.md §6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    /// Chunk ID (content-addressed blake3).
    pub chunk_id: ContentId,

    /// Document ID (content-addressed blake3).
    pub resource_id: ContentId,

    /// Store reference.
    pub store: CitationStore,

    /// Canonical locator (file path as `file://`, or URL) — the user-actionable locator.
    pub uri: String,

    /// Document title.
    #[serde(default)]
    pub title: Option<String>,

    /// Heading path, e.g. `["API", "Auth"]`.
    #[serde(default)]
    pub heading_path: Vec<String>,

    /// The block this chunk originated from.
    pub block: CitationBlock,

    /// The chunk's position within its parent block.
    pub chunk_position: ChunkPosition,

    /// Refined sub-block location (span, plus window block seqs for
    /// message-window chunks).
    pub location: CitationLocation,

    /// Complete representative passage text.
    pub snippet: String,

    /// Search scores.
    pub score: Score,

    /// Provenance summary.
    pub provenance: CitationProvenance,

    /// Resource metadata (Dublin Core plus kind-specific fields), tagged by
    /// resource kind (`"kind":"document"|"conversation"|"transcription"`).
    #[serde(default)]
    pub metadata: Metadata,

    /// Other matching occurrences in ranked order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duplicates: Vec<CitationDuplicate>,
}

/// A direct match against the representative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateReason {
    /// Exactly equal stored passage text.
    ExactText,
    /// Exactly equal eligible stored vector.
    ExactVector,
}

/// Store-qualified reference to an earlier explicit text owner within a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationChunkRef {
    pub store_id: String,
    pub chunk_id: String,
}

/// A matching occurrence with reasons relative to the representative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitationDuplicate {
    pub reasons: Vec<DuplicateReason>,
    pub citation: CitationOccurrence,
}

/// Nonrecursive alternate citation preserving occurrence-specific fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CitationOccurrence {
    /// Chunk ID (content-addressed blake3).
    pub chunk_id: ContentId,

    /// Document ID (content-addressed blake3).
    pub resource_id: ContentId,

    /// Store reference.
    pub store: CitationStore,

    /// Canonical locator (file path as `file://`, or URL) — the user-actionable locator.
    pub uri: String,

    /// Document title.
    #[serde(default)]
    pub title: Option<String>,

    /// Heading path, e.g. `["API", "Auth"]`.
    #[serde(default)]
    pub heading_path: Vec<String>,

    /// The block this chunk originated from.
    pub block: CitationBlock,

    /// The chunk's position within its parent block.
    pub chunk_position: ChunkPosition,

    /// Refined sub-block location (span, plus window block seqs for
    /// message-window chunks).
    pub location: CitationLocation,

    /// Full text when this occurrence owns a differing passage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,

    /// Earlier alternate owning this occurrence's text, if not the representative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet_ref: Option<CitationChunkRef>,

    /// Search scores.
    pub score: Score,

    /// Provenance summary.
    pub provenance: CitationProvenance,

    /// Resource metadata (Dublin Core plus kind-specific fields), tagged by
    /// resource kind (`"kind":"document"|"conversation"|"transcription"`).
    #[serde(default)]
    pub metadata: Metadata,
}

impl From<Citation> for CitationOccurrence {
    fn from(citation: Citation) -> Self {
        Self {
            chunk_id: citation.chunk_id,
            resource_id: citation.resource_id,
            store: citation.store,
            uri: citation.uri,
            title: citation.title,
            heading_path: citation.heading_path,
            block: citation.block,
            chunk_position: citation.chunk_position,
            location: citation.location,
            snippet: Some(citation.snippet),
            snippet_ref: None,
            score: citation.score,
            provenance: citation.provenance,
            metadata: citation.metadata,
        }
    }
}

impl Citation {
    /// Attach independently shaped occurrences, retaining each distinct text once.
    /// References always identify an earlier alternate that directly owns its text.
    pub fn with_duplicates(
        mut self,
        duplicates: impl IntoIterator<Item = (Citation, Vec<DuplicateReason>)>,
    ) -> Self {
        let mut owners = std::collections::HashMap::<String, CitationChunkRef>::new();
        self.duplicates = duplicates
            .into_iter()
            .map(|(citation, reasons)| {
                let mut occurrence = CitationOccurrence::from(citation);
                let text = occurrence
                    .snippet
                    .take()
                    .expect("shaped citation owns its text");
                if text != self.snippet {
                    if let Some(owner) = owners.get(&text) {
                        occurrence.snippet_ref = Some(owner.clone());
                    } else {
                        owners.insert(
                            text.clone(),
                            CitationChunkRef {
                                store_id: occurrence.store.id.clone(),
                                chunk_id: occurrence.chunk_id.clone(),
                            },
                        );
                        occurrence.snippet = Some(text);
                    }
                }
                CitationDuplicate {
                    reasons,
                    citation: occurrence,
                }
            })
            .collect();
        self
    }
}

#[cfg(test)]
mod tests;
