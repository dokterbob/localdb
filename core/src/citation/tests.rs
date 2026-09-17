use super::*;
use crate::ids::{chunk_id, content_hash, new_ulid, resource_id};

fn make_citation() -> Citation {
    let doc_id = resource_id("file:///docs/api.md", &content_hash("some content"));
    let snippet = "This is the chunk text.";
    let span = Span::new(100, 123);
    let cid = chunk_id(&doc_id, 0, snippet, 0);

    Citation {
        duplicates: vec![],
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

#[test]
fn compact_occurrences_preserve_fields_and_own_each_text_once() {
    let representative = make_citation();
    let mut alternates = Vec::new();
    for (i, text) in [
        "differing text",
        "differing text",
        representative.snippet.as_str(),
    ]
    .into_iter()
    .enumerate()
    {
        let mut citation = make_citation();
        citation.chunk_id = "same-chunk".into();
        citation.store.id = format!("store-{i}");
        citation.resource_id = format!("resource-{i}");
        citation.uri = format!("file:///copy-{i}.pdf");
        citation.snippet = text.into();
        citation.block.page = Some(i as u32 + 1);
        citation.location.window_block_seqs = vec![1, 2, 3];
        citation.chunk_position.seq_in_block = i as u32;
        alternates.push(citation);
    }
    let group = representative.clone().with_duplicates(
        alternates
            .clone()
            .into_iter()
            .map(|c| (c, vec![DuplicateReason::ExactVector])),
    );
    let json = serde_json::to_value(&group).unwrap();
    assert_eq!(
        json["duplicates"][0]["citation"]["snippet"],
        "differing text"
    );
    assert!(json["duplicates"][0]["citation"]
        .get("snippet_ref")
        .is_none());
    assert!(json["duplicates"][1]["citation"].get("snippet").is_none());
    assert_eq!(
        json["duplicates"][1]["citation"]["snippet_ref"],
        serde_json::json!({"store_id":"store-0", "chunk_id":"same-chunk"})
    );
    assert!(json["duplicates"][2]["citation"].get("snippet").is_none());
    assert!(json["duplicates"][2]["citation"]
        .get("snippet_ref")
        .is_none());
    for (mut duplicate, expected) in group.duplicates.into_iter().zip(alternates) {
        duplicate.citation.snippet = Some(expected.snippet.clone());
        duplicate.citation.snippet_ref = None;
        assert_eq!(duplicate.citation, CitationOccurrence::from(expected));
    }
    assert_eq!(group.score, representative.score);
}

#[test]
fn compact_empty_text_is_explicit_owned_value_and_both_reasons_omit_equal_text() {
    let representative = make_citation();
    let mut empty = representative.clone();
    empty.snippet.clear();
    empty.chunk_id = "empty".into();
    let group = representative.clone().with_duplicates([
        (empty.clone(), vec![DuplicateReason::ExactVector]),
        (empty, vec![DuplicateReason::ExactVector]),
        (
            representative,
            vec![DuplicateReason::ExactText, DuplicateReason::ExactVector],
        ),
    ]);
    assert_eq!(group.duplicates[0].citation.snippet.as_deref(), Some(""));
    assert_eq!(
        group.duplicates[1]
            .citation
            .snippet_ref
            .as_ref()
            .unwrap()
            .chunk_id,
        "empty"
    );
    assert_eq!(group.duplicates[2].citation.snippet, None);
    assert_eq!(group.duplicates[2].citation.snippet_ref, None);
}

#[test]
fn legacy_citation_without_duplicates_deserializes() {
    let mut value = serde_json::to_value(make_citation()).unwrap();
    value.as_object_mut().unwrap().remove("duplicates");
    let parsed: Citation = serde_json::from_value(value.clone()).unwrap();
    assert!(parsed.duplicates.is_empty());
    assert_eq!(serde_json::to_value(parsed).unwrap(), value);
}

#[test]
fn documented_compact_groups_match_shaping_and_roundtrip() {
    let examples: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/search-groups.json")).unwrap();
    for name in ["exact_text", "differing_text"] {
        let expected = &examples[name]["citations"][0];
        let group: Citation = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(&group).unwrap(), *expected);
        let mut owners = std::collections::HashMap::new();
        let independent: Vec<_> = group
            .duplicates
            .iter()
            .map(|duplicate| {
                let occurrence = &duplicate.citation;
                let text = if let Some(text) = &occurrence.snippet {
                    owners.insert(
                        (occurrence.store.id.clone(), occurrence.chunk_id.clone()),
                        text.clone(),
                    );
                    text.clone()
                } else if let Some(reference) = &occurrence.snippet_ref {
                    owners
                        .get(&(reference.store_id.clone(), reference.chunk_id.clone()))
                        .unwrap()
                        .clone()
                } else {
                    group.snippet.clone()
                };
                let mut value = serde_json::to_value(occurrence).unwrap();
                value.as_object_mut().unwrap().remove("snippet_ref");
                value["snippet"] = text.into();
                (
                    serde_json::from_value::<Citation>(value).unwrap(),
                    duplicate.reasons.clone(),
                )
            })
            .collect();
        let shaped = group.with_duplicates(independent);
        assert_eq!(serde_json::to_value(shaped).unwrap(), *expected);
    }
    assert_eq!(
        examples["empty_http"],
        serde_json::json!({"citations":[],"total_candidates":0,"total_results":0,"next_cursor":null})
    );
}
