use serde::{Deserialize, Serialize};

use crate::metadata::Metadata;
use crate::uri::Uri;

/// Kind of resource, determining block ordering semantics.
///
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    /// Logical reading order (files, web pages, Notion pages).
    Document,
    /// Chronological message order (chat, email threads).
    Conversation,
    /// Transcript time order (SRT, VTT, Whisper).
    Transcription,
}

/// A logical content unit produced by an ingestor.
///
/// Replaces the former `Document` entity. A resource carries ordered blocks
/// and metadata.
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    /// Content-addressed ID: `blake3(uri || content_hash)`.
    pub id: String,

    /// Owning store ID.
    pub store_id: String,

    /// Source ID.
    pub source_id: String,

    /// Which ingestor produced this resource.
    pub ingestor_kind: IngestorKind,

    /// Resource kind (determines block ordering semantics).
    pub resource_kind: ResourceKind,

    /// Canonical locator.
    pub uri: Uri,

    /// Arbitrary source-system ID (Notion page ID, message ID, etc.).
    pub external_id: Option<String>,

    /// Change detection token from the source system.
    pub external_etag: Option<String>,

    /// blake3 of ordered block canonical texts concatenated.
    pub content_hash: String,

    /// Title from extraction.
    pub title: Option<String>,

    /// MIME type.
    pub mime: Option<String>,

    /// Full metadata (Dublin Core base + kind-specific).
    pub metadata: Metadata,

    /// When first indexed (RFC 3339).
    pub added_at: String,

    /// When content last changed (RFC 3339).
    pub modified_at: String,

    /// Conversation thread identifier (conversation resources only).
    pub thread_id: Option<String>,

    /// Channel/folder/chat name (conversation resources only).
    pub channel: Option<String>,

    /// Participant names/IDs (conversation resources only).
    #[serde(default)]
    pub participants: Vec<String>,

    /// Provenance: origin store, source ref, content hash, share path.
    pub origin_store: String,

    /// Hash of the indexing policy.
    pub policy_version: String,

    /// Share path for federation (reserved, empty in MVP).
    #[serde(default)]
    pub share_path: Option<String>,

    /// Version of the parser/ingestor that produced the blocks.
    pub extractor_version: String,

    /// Ordered blocks.
    pub blocks: Vec<Block>,
}

/// Ingestor kind — which ingestor produced a resource or drives a source.
///
/// Lives in `core` as part of the contract; concrete ingestor implementations
/// live outside `core`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestorKind {
    File,
    Url,
    Notion,
    Telegram,
    Signal,
    HackMd,
    Email,
    Transcription,
    Feed,
}

impl IngestorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            IngestorKind::File => "file",
            IngestorKind::Url => "url",
            IngestorKind::Notion => "notion",
            IngestorKind::Telegram => "telegram",
            IngestorKind::Signal => "signal",
            IngestorKind::HackMd => "hackmd",
            IngestorKind::Email => "email",
            IngestorKind::Transcription => "transcription",
            IngestorKind::Feed => "feed",
        }
    }
}

impl std::fmt::Display for IngestorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A typed, ordered unit of content within a resource.
///
/// See specs/02-domain-model.md §2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Ordering within the resource (0-indexed).
    pub seq: u32,

    /// Block kind (determines chunking behavior).
    pub kind: BlockKind,

    /// Canonical text content.
    pub text: String,

    /// Optional source-location data for citation/navigation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<BlockLocation>,
}

/// Block kind — typed variants for different content shapes.
///
/// See specs/02-domain-model.md §2a.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Heading {
        level: u8,
    },
    Paragraph,
    Code {
        language: Option<String>,
    },
    Quote,
    List {
        ordered: bool,
    },
    Table {
        headers: Vec<String>,
        rows: usize,
    },
    Message {
        sender: String,
        timestamp: Option<String>,
        message_id: Option<String>,
        reply_to: Option<String>,
    },
    Segment {
        speaker: Option<String>,
        start_ms: u64,
        end_ms: u64,
    },
    Reference {
        target: String,
        label: Option<String>,
        ref_type: Option<String>,
    },
    Attachment {
        filename: String,
        mime: Option<String>,
        size_bytes: Option<u64>,
    },
    Frontmatter {
        format: String,
    },
    Image {
        alt: Option<String>,
        src: Option<String>,
    },
}

impl BlockKind {
    /// The kind name as a string for storage/display.
    pub fn kind_str(&self) -> &'static str {
        match self {
            BlockKind::Heading { .. } => "heading",
            BlockKind::Paragraph => "paragraph",
            BlockKind::Code { .. } => "code",
            BlockKind::Quote => "quote",
            BlockKind::List { .. } => "list",
            BlockKind::Table { .. } => "table",
            BlockKind::Message { .. } => "message",
            BlockKind::Segment { .. } => "segment",
            BlockKind::Reference { .. } => "reference",
            BlockKind::Attachment { .. } => "attachment",
            BlockKind::Frontmatter { .. } => "frontmatter",
            BlockKind::Image { .. } => "image",
        }
    }
}

/// Source-location metadata on a block, for citation and navigation.
///
/// Not all fields apply to every block kind.
/// See specs/02-domain-model.md §2b.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlockLocation {
    /// Page number (1-indexed, for PDFs and paginated documents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,

    /// Bounding box for PDFs with layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BoundingBox>,

    /// Section identifier or path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<Vec<String>>,

    /// Line range in source file (for code and plain text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,

    /// URI fragment (e.g. `#heading-id` for HTML).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri_fragment: Option<String>,
}

/// Bounding box for PDF layout positions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Chunk location within the block tree, for citation resolution.
///
/// Used on chunks to record their position within the resource's block structure.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChunkLocation {
    /// Sequence number of the block within the resource.
    pub block_seq: u32,

    /// Chunk position within the block (0-indexed).
    pub seq_in_block: u32,

    /// For message-window chunks: all block seqs participating in the window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub window_block_seqs: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DocumentMetadata, DublinCoreMetadata};

    #[test]
    fn block_kind_heading_roundtrip() {
        let kind = BlockKind::Heading { level: 2 };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"type\":\"heading\""));
        let kind2: BlockKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, kind2);
        assert_eq!(kind.kind_str(), "heading");
    }

    #[test]
    fn block_kind_message_roundtrip() {
        let kind = BlockKind::Message {
            sender: "Alice".to_string(),
            timestamp: Some("2026-06-30T12:00:00Z".to_string()),
            message_id: Some("msg-123".to_string()),
            reply_to: None,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let kind2: BlockKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, kind2);
        assert_eq!(kind.kind_str(), "message");
    }

    #[test]
    fn block_kind_segment_roundtrip() {
        let kind = BlockKind::Segment {
            speaker: Some("Bob".to_string()),
            start_ms: 1000,
            end_ms: 5000,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let kind2: BlockKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, kind2);
    }

    #[test]
    fn block_kind_all_variants_serialize() {
        let variants: Vec<BlockKind> = vec![
            BlockKind::Heading { level: 1 },
            BlockKind::Paragraph,
            BlockKind::Code {
                language: Some("rust".to_string()),
            },
            BlockKind::Quote,
            BlockKind::List { ordered: true },
            BlockKind::Table {
                headers: vec!["A".to_string()],
                rows: 5,
            },
            BlockKind::Message {
                sender: "x".to_string(),
                timestamp: None,
                message_id: None,
                reply_to: None,
            },
            BlockKind::Segment {
                speaker: None,
                start_ms: 0,
                end_ms: 100,
            },
            BlockKind::Reference {
                target: "url".to_string(),
                label: None,
                ref_type: None,
            },
            BlockKind::Attachment {
                filename: "f.pdf".to_string(),
                mime: None,
                size_bytes: None,
            },
            BlockKind::Frontmatter {
                format: "yaml".to_string(),
            },
            BlockKind::Image {
                alt: None,
                src: None,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let v2: BlockKind = serde_json::from_str(&json).unwrap();
            assert_eq!(v, &v2);
        }
    }

    #[test]
    fn block_location_roundtrip() {
        let loc = BlockLocation {
            page: Some(3),
            bbox: Some(BoundingBox {
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 50.0,
            }),
            section: Some(vec!["Chapter 1".to_string(), "Intro".to_string()]),
            line_start: None,
            line_end: None,
            uri_fragment: None,
        };
        let json = serde_json::to_string(&loc).unwrap();
        let loc2: BlockLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(loc, loc2);
    }

    #[test]
    fn block_location_empty_roundtrip() {
        let loc = BlockLocation::default();
        let json = serde_json::to_string(&loc).unwrap();
        assert_eq!(json, "{}");
        let loc2: BlockLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(loc, loc2);
    }

    #[test]
    fn chunk_location_roundtrip() {
        let loc = ChunkLocation {
            block_seq: 5,
            seq_in_block: 2,
            window_block_seqs: vec![],
        };
        let json = serde_json::to_string(&loc).unwrap();
        let loc2: ChunkLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(loc, loc2);
    }

    #[test]
    fn chunk_location_with_window() {
        let loc = ChunkLocation {
            block_seq: 3,
            seq_in_block: 0,
            window_block_seqs: vec![3, 4, 5, 6, 7, 8],
        };
        let json = serde_json::to_string(&loc).unwrap();
        assert!(json.contains("window_block_seqs"));
        let loc2: ChunkLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(loc, loc2);
    }

    #[test]
    fn block_roundtrip() {
        let block = Block {
            seq: 0,
            kind: BlockKind::Paragraph,
            text: "Hello, world!".to_string(),
            location: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        let block2: Block = serde_json::from_str(&json).unwrap();
        assert_eq!(block, block2);
    }

    #[test]
    fn resource_kind_roundtrip() {
        for kind in [
            ResourceKind::Document,
            ResourceKind::Conversation,
            ResourceKind::Transcription,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let kind2: ResourceKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, kind2);
        }
    }

    #[test]
    fn ingestor_kind_roundtrip() {
        for kind in [
            IngestorKind::File,
            IngestorKind::Url,
            IngestorKind::Notion,
            IngestorKind::Telegram,
            IngestorKind::Signal,
            IngestorKind::HackMd,
            IngestorKind::Email,
            IngestorKind::Transcription,
            IngestorKind::Feed,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let kind2: IngestorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, kind2);
            assert!(!kind.as_str().is_empty());
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn resource_minimal_roundtrip() {
        let resource = Resource {
            id: "abc123".to_string(),
            store_id: "store1".to_string(),
            source_id: "src1".to_string(),
            ingestor_kind: IngestorKind::File,
            resource_kind: ResourceKind::Document,
            uri: Uri::parse("file:///test.md").unwrap(),
            external_id: None,
            external_etag: None,
            content_hash: "hash123".to_string(),
            title: Some("Test".to_string()),
            mime: Some("text/markdown".to_string()),
            metadata: Metadata::Document(DocumentMetadata {
                dublin_core: DublinCoreMetadata {
                    title: Some("Test".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            }),
            added_at: "2026-06-30T00:00:00Z".to_string(),
            modified_at: "2026-06-30T00:00:00Z".to_string(),
            thread_id: None,
            channel: None,
            participants: vec![],
            origin_store: "store1".to_string(),
            policy_version: "v1".to_string(),
            share_path: None,
            extractor_version: "1.0".to_string(),
            blocks: vec![Block {
                seq: 0,
                kind: BlockKind::Heading { level: 1 },
                text: "Test".to_string(),
                location: None,
            }],
        };
        let json = serde_json::to_string(&resource).unwrap();
        let resource2: Resource = serde_json::from_str(&json).unwrap();
        assert_eq!(resource, resource2);
    }
}
