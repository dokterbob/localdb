use super::*;
use crate::embedder::VectorEncoding;
use crate::search::tests::make_chunk;
use crate::store::StoredEmbeddingIdentity;
use std::sync::Arc;

fn entry(id: &str, text: &str, bytes: &[u8]) -> FusedChunkEntry {
    FusedChunkEntry {
        chunk: make_chunk(id, id, "store", text, vec![], id, vec![]),
        fused_score: 1.0,
        dense_score: Some(0.5),
        bm25_score: None,
        embedding_identity: Some(StoredEmbeddingIdentity {
            format: "test".into(),
            encoding: VectorEncoding::Float32,
            dimensions: 1,
            bytes: Arc::from(bytes),
        }),
    }
}

#[test]
fn grouping_is_direct_and_reasons_are_against_chosen_representative() {
    let groups = group_candidates(
        vec![
            entry("a", "X", &[1]),
            entry("b", "X", &[2]),
            entry("c", "Y", &[2]),
        ],
        SearchDedup::TextAndVector,
    );
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].duplicates[0].1, vec![DuplicateReason::ExactText]);
    assert_eq!(groups[1].representative.chunk.id, "c");
    let groups = group_candidates(
        vec![
            entry("a", "X", &[1]),
            entry("b", "Y", &[2]),
            entry("c", "Y", &[1]),
        ],
        SearchDedup::TextAndVector,
    );
    assert_eq!(groups[0].duplicates[0].0.chunk.id, "c");
    assert_eq!(
        groups[0].duplicates[0].1,
        vec![DuplicateReason::ExactVector]
    );
    assert!(groups[1].duplicates.is_empty());
}

#[test]
fn modes_and_nonadjacent_matches_preserve_scores_and_order() {
    let candidates = vec![
        entry("a", "X", &[1]),
        entry("b", "Y", &[2]),
        entry("c", "X", &[1]),
        entry("d", "Z", &[1]),
    ];
    assert_eq!(
        group_candidates(candidates.clone(), SearchDedup::Off).len(),
        4
    );
    let text = group_candidates(candidates.clone(), SearchDedup::Text);
    assert_eq!(text.len(), 3);
    assert_eq!(text[0].duplicates[0].1, vec![DuplicateReason::ExactText]);
    let both = group_candidates(candidates, SearchDedup::TextAndVector);
    assert_eq!(both.len(), 2);
    assert_eq!(both[0].representative.fused_score, 1.0);
    assert_eq!(
        both[0]
            .duplicates
            .iter()
            .map(|(e, _)| e.chunk.id.as_str())
            .collect::<Vec<_>>(),
        ["c", "d"]
    );
    assert_eq!(
        both[0].duplicates[0].1,
        vec![DuplicateReason::ExactText, DuplicateReason::ExactVector]
    );
}

#[test]
fn vector_identity_requires_all_fields_and_nonempty_policy() {
    let original = entry("a", "X", &[1]);
    let mut variants = Vec::new();
    let mut v = entry("b", "Y", &[1]);
    v.embedding_identity = None;
    variants.push(v);
    let mut v = entry("b", "Y", &[1]);
    v.embedding_identity.as_mut().unwrap().format = "other".into();
    variants.push(v);
    let mut v = entry("b", "Y", &[1]);
    v.embedding_identity.as_mut().unwrap().encoding = VectorEncoding::Binary;
    variants.push(v);
    let mut v = entry("b", "Y", &[1]);
    v.embedding_identity.as_mut().unwrap().dimensions = 2;
    variants.push(v);
    let mut v = entry("b", "Y", &[1]);
    v.embedding_identity.as_mut().unwrap().dimensions = 0;
    variants.push(v);
    variants.push(entry("b", "Y", &[]));
    variants.push(entry("b", "Y", &[2]));
    let mut v = entry("b", "Y", &[1]);
    v.chunk.policy_version = "other".into();
    variants.push(v);
    let mut v = entry("b", "Y", &[1]);
    v.chunk.policy_version.clear();
    variants.push(v);
    for mut v in variants {
        if v.embedding_identity
            .as_ref()
            .is_none_or(|identity| identity.dimensions == 0 || identity.bytes.is_empty())
        {
            let mut same_invalid_identity = v.clone();
            same_invalid_identity.chunk.id = "invalid-copy".into();
            same_invalid_identity.chunk.text = "different text".into();
            assert_eq!(
                group_candidates(
                    vec![v.clone(), same_invalid_identity],
                    SearchDedup::TextAndVector
                )
                .len(),
                2
            );
        }
        assert_eq!(
            group_candidates(
                vec![original.clone(), v.clone()],
                SearchDedup::TextAndVector
            )
            .len(),
            2
        );
        v.chunk.text = "X".into();
        assert_eq!(
            group_candidates(vec![original.clone(), v], SearchDedup::TextAndVector).len(),
            1
        );
    }
    let mut a = original;
    a.chunk.policy_version.clear();
    let mut b = entry("b", "Y", &[1]);
    b.chunk.policy_version.clear();
    assert_eq!(
        group_candidates(vec![a, b], SearchDedup::TextAndVector).len(),
        2
    );
}
