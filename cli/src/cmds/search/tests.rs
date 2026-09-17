use super::citation_headline;
use localdb_core::citation::{
    ChunkPosition, Citation, CitationBlock, CitationLocation, CitationProvenance, CitationStore,
    Score,
};
use localdb_core::types::Span;

fn citation_with(page: Option<u32>, heading: Vec<String>) -> Citation {
    Citation {
        chunk_id: "chunk".to_string(),
        resource_id: "res".to_string(),
        store: CitationStore {
            id: "01HN1Y28MYWN6X5DSKZMNE1T5W".to_string(),
            name: "s".to_string(),
        },
        uri: "file:///docs/paper.pdf".to_string(),
        title: None,
        heading_path: heading,
        block: CitationBlock {
            seq: 0,
            kind: Some("text".to_string()),
            page,
        },
        chunk_position: ChunkPosition { seq_in_block: 0 },
        location: CitationLocation {
            span: Span::new(0, 4),
            window_block_seqs: vec![],
        },
        snippet: "text".to_string(),
        score: Score {
            fused: 1.0,
            dense: None,
            bm25: None,
        },
        provenance: CitationProvenance {
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc".to_string(),
        },
        metadata: Default::default(),
    }
}

#[test]
fn headline_appends_page_when_present() {
    let line = citation_headline(&citation_with(Some(12), vec![]));
    assert_eq!(line, "file:///docs/paper.pdf (p.12)");
}

#[test]
fn headline_omits_page_when_absent() {
    let line = citation_headline(&citation_with(None, vec![]));
    assert_eq!(line, "file:///docs/paper.pdf");
}

#[test]
fn headline_combines_heading_path_and_page() {
    let line = citation_headline(&citation_with(
        Some(3),
        vec!["Intro".to_string(), "Setup".to_string()],
    ));
    assert_eq!(line, "file:///docs/paper.pdf > Intro > Setup (p.3)");
}
