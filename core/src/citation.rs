//! Citation model — the canonical result shape every surface uses.
//!
//! See specs/02-domain-model.md §6.
//!
//! Every search hit, on every surface (HTTP, CLI, MCP), resolves to this structure.

use serde::{Deserialize, Serialize};

use crate::ids::{ContentId, UlidId};
use crate::parser::DocumentMetadata;
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
    pub document_id: ContentId,

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

    /// Range in the normalized document text.
    pub span: Span,

    /// Chunk text (possibly trimmed).
    pub snippet: String,

    /// Search scores.
    pub score: Score,

    /// Provenance summary.
    pub provenance: CitationProvenance,

    /// Document metadata (Dublin Core); empty/`None` fields when none was extracted.
    #[serde(default)]
    pub metadata: DocumentMetadata,

    /// Block sequence number where this chunk originated.
    ///
    /// `None` for citations produced before the Resource/Block architecture.
    #[serde(default)]
    pub block_seq: Option<u32>,

    /// Block kind string (e.g. "paragraph", "heading").
    ///
    /// `None` for citations produced before the Resource/Block architecture.
    #[serde(default)]
    pub block_kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{chunk_id, content_hash, document_id, new_ulid};

    fn make_citation() -> Citation {
        let doc_id = document_id("file:///docs/api.md", &content_hash("some content"));
        let snippet = "This is the chunk text.";
        let span = Span::new(100, 123);
        let cid = chunk_id(&doc_id, snippet, span.start, span.end, 0);

        Citation {
            chunk_id: cid,
            document_id: doc_id,
            store: CitationStore {
                id: new_ulid(),
                name: "my-store".to_string(),
            },
            uri: "file:///docs/api.md".to_string(),
            title: Some("API Documentation".to_string()),
            heading_path: vec!["API".to_string(), "Authentication".to_string()],
            span,
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
            metadata: DocumentMetadata {
                title: Some("API Documentation".to_string()),
                creator: vec!["Alice Example".to_string()],
                date: Some("2026-01-15".to_string()),
                ..Default::default()
            },
            block_seq: None,
            block_kind: None,
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
        assert!(v.get("document_id").is_some(), "document_id missing");
        assert!(v.get("store").is_some(), "store missing");
        assert!(v.get("uri").is_some(), "uri missing");
        assert!(v.get("heading_path").is_some(), "heading_path missing");
        assert!(v.get("span").is_some(), "span missing");
        assert!(v.get("snippet").is_some(), "snippet missing");
        assert!(v.get("score").is_some(), "score missing");
        assert!(v.get("provenance").is_some(), "provenance missing");

        // Store shape
        let store = &v["store"];
        assert!(store.get("id").is_some(), "store.id missing");
        assert!(store.get("name").is_some(), "store.name missing");

        // Span shape
        let span = &v["span"];
        assert!(span.get("start").is_some(), "span.start missing");
        assert!(span.get("end").is_some(), "span.end missing");

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

        // Metadata shape
        assert!(v.get("metadata").is_some(), "metadata missing");
        let meta = &v["metadata"];
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
