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

    /// Chunk text (possibly trimmed).
    pub snippet: String,

    /// Search scores.
    pub score: Score,

    /// Provenance summary.
    pub provenance: CitationProvenance,

    /// Resource metadata (Dublin Core plus kind-specific fields), tagged by
    /// resource kind (`"kind":"document"|"conversation"|"transcription"`).
    #[serde(default)]
    pub metadata: Metadata,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{chunk_id, content_hash, new_ulid, resource_id};

    fn make_citation() -> Citation {
        let doc_id = resource_id("file:///docs/api.md", &content_hash("some content"));
        let snippet = "This is the chunk text.";
        let span = Span::new(100, 123);
        let cid = chunk_id(&doc_id, 0, snippet, 0);

        Citation {
            chunk_id: cid,
            resource_id: doc_id,
            store: CitationStore {
                id: new_ulid(),
                name: "my-store".to_string(),
            },
            uri: "file:///docs/api.md".to_string(),
            title: Some("API Documentation".to_string()),
            heading_path: vec!["API".to_string(), "Authentication".to_string()],
            block: CitationBlock {
                seq: 3,
                kind: Some("paragraph".to_string()),
                page: Some(12),
            },
            chunk_position: ChunkPosition { seq_in_block: 0 },
            location: CitationLocation {
                span,
                window_block_seqs: vec![],
            },
            snippet: snippet.to_string(),
            score: Score {
                fused: 0.85,
                dense: Some(0.92),
                bm25: Some(0.78),
            },
            provenance: CitationProvenance {
                fetched_at: "2026-06-10T12:00:00Z".to_string(),
                content_hash: content_hash("some content"),
            },
            metadata: Metadata::Document(crate::metadata::DocumentMetadata {
                dublin_core: crate::metadata::DublinCoreMetadata {
                    title: Some("API Documentation".to_string()),
                    creator: vec!["Alice Example".to_string()],
                    date: Some("2026-01-15".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            }),
        }
    }

    // --- Serialization tests ---

    #[test]
    fn citation_serializes_roundtrip() {
        let c = make_citation();
        let json = serde_json::to_string(&c).unwrap();
        let c2: Citation = serde_json::from_str(&json).unwrap();
        assert_eq!(c, c2);
    }

    /// Verifies the exact JSON shape described in specs/02-domain-model.md §6.
    #[test]
    fn citation_json_has_exact_shape() {
        let c = make_citation();
        let v: serde_json::Value = serde_json::to_value(&c).unwrap();

        // All required top-level fields present
        assert!(v.get("chunk_id").is_some(), "chunk_id missing");
        assert!(v.get("resource_id").is_some(), "resource_id missing");
        assert!(v.get("store").is_some(), "store missing");
        assert!(v.get("uri").is_some(), "uri missing");
        assert!(v.get("heading_path").is_some(), "heading_path missing");
        assert!(v.get("block").is_some(), "block missing");
        assert!(v.get("chunk_position").is_some(), "chunk_position missing");
        assert!(v.get("location").is_some(), "location missing");
        assert!(v.get("snippet").is_some(), "snippet missing");
        assert!(v.get("score").is_some(), "score missing");
        assert!(v.get("provenance").is_some(), "provenance missing");

        // Store shape
        let store = &v["store"];
        assert!(store.get("id").is_some(), "store.id missing");
        assert!(store.get("name").is_some(), "store.name missing");

        // block: {seq, kind}
        let block = &v["block"];
        assert_eq!(block["seq"], 3);
        assert_eq!(block["kind"], "paragraph");

        // chunk_position: {seq_in_block}
        assert_eq!(v["chunk_position"]["seq_in_block"], 0);

        // location: {span: {start, end}, window_block_seqs?}
        let location = &v["location"];
        let span = &location["span"];
        assert!(span.get("start").is_some(), "span.start missing");
        assert!(span.get("end").is_some(), "span.end missing");
        assert_eq!(span["start"], 100);
        assert_eq!(span["end"], 123);
        // window_block_seqs is empty for this fixture -> omitted from JSON.
        assert!(
            location.get("window_block_seqs").is_none(),
            "window_block_seqs should be omitted when empty"
        );

        // Score shape
        let score = &v["score"];
        assert!(score.get("fused").is_some(), "score.fused missing");
        assert!(score.get("dense").is_some(), "score.dense missing");
        assert!(score.get("bm25").is_some(), "score.bm25 missing");

        // Provenance shape
        let prov = &v["provenance"];
        assert!(
            prov.get("fetched_at").is_some(),
            "provenance.fetched_at missing"
        );
        assert!(
            prov.get("content_hash").is_some(),
            "provenance.content_hash missing"
        );

        // Metadata shape — tagged enum, Dublin Core fields flattened alongside "kind".
        assert!(v.get("metadata").is_some(), "metadata missing");
        let meta = &v["metadata"];
        assert_eq!(meta["kind"].as_str().unwrap(), "document");
        assert_eq!(
            meta["creator"].as_array().unwrap()[0].as_str().unwrap(),
            "Alice Example"
        );
        assert_eq!(meta["date"].as_str().unwrap(), "2026-01-15");
        assert_eq!(meta["title"].as_str().unwrap(), "API Documentation");
    }

    #[test]
    fn citation_store_shape() {
        let store = CitationStore {
            id: "01HN1Y28MYWN6X5DSKZMNE1T5W".to_string(),
            name: "test-store".to_string(),
        };
        let v = serde_json::to_value(&store).unwrap();
        assert_eq!(v["id"], "01HN1Y28MYWN6X5DSKZMNE1T5W");
        assert_eq!(v["name"], "test-store");
    }

    #[test]
    fn score_serializes_with_both_legs() {
        let score = Score {
            fused: 0.9,
            dense: Some(0.95),
            bm25: Some(0.85),
        };
        let v = serde_json::to_value(&score).unwrap();
        assert_eq!(v["fused"], 0.9);
        assert_eq!(v["dense"], 0.95);
        assert_eq!(v["bm25"], 0.85);
    }

    #[test]
    fn score_serializes_single_leg_only() {
        let score_dense_only = Score {
            fused: 0.9,
            dense: Some(0.95),
            bm25: None,
        };
        let v = serde_json::to_value(&score_dense_only).unwrap();
        assert_eq!(v["fused"], 0.9);
        assert_eq!(v["dense"], 0.95);
        // bm25 is null when None
        assert!(v["bm25"].is_null());
    }

    #[test]
    fn citation_title_optional() {
        let mut c = make_citation();
        c.title = None;
        // title should either be absent or null — check that it doesn't cause errors
        let json = serde_json::to_string(&c).unwrap();
        let c2: Citation = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.title, None);
    }

    #[test]
    fn citation_heading_path_can_be_empty() {
        let mut c = make_citation();
        c.heading_path = vec![];
        let json = serde_json::to_string(&c).unwrap();
        let c2: Citation = serde_json::from_str(&json).unwrap();
        assert!(c2.heading_path.is_empty());
    }

    /// `window_block_seqs` is present (non-empty array) for message-window
    /// chunks — the opposite of the default fixture's omitted-when-empty case.
    #[test]
    fn citation_window_block_seqs_present_when_non_empty() {
        let mut c = make_citation();
        c.location.window_block_seqs = vec![3, 4, 5];
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(
            v["location"]["window_block_seqs"],
            serde_json::json!([3, 4, 5])
        );

        // Round trip preserves it.
        let json = serde_json::to_string(&c).unwrap();
        let c2: Citation = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.location.window_block_seqs, vec![3, 4, 5]);
    }

    /// The removed top-level fields (`block_seq`, `block_kind`, `span`) must
    /// not appear in the serialized JSON — superseded by `block`,
    /// `chunk_position`, and `location.span` respectively.
    #[test]
    fn citation_json_has_no_legacy_top_level_fields() {
        let c = make_citation();
        let v = serde_json::to_value(&c).unwrap();
        assert!(v.get("block_seq").is_none(), "block_seq must be removed");
        assert!(v.get("block_kind").is_none(), "block_kind must be removed");
        assert!(
            v.get("span").is_none(),
            "top-level span must be removed (moved to location.span)"
        );
    }

    #[test]
    fn citation_provenance_shape() {
        let prov = CitationProvenance {
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "a".repeat(64),
        };
        let v = serde_json::to_value(&prov).unwrap();
        assert!(v.get("fetched_at").is_some());
        assert!(v.get("content_hash").is_some());
    }
}
