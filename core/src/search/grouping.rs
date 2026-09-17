//! Direct, ranked passage grouping over the complete retrieved candidate pool.

use std::collections::HashMap;

use crate::citation::DuplicateReason;
use crate::store::StoredEmbeddingIdentity;

use super::{FusedChunkEntry, SearchDedup};

pub(super) struct PassageGroup {
    pub representative: FusedChunkEntry,
    pub duplicates: Vec<(FusedChunkEntry, Vec<DuplicateReason>)>,
}

fn vector_key(entry: &FusedChunkEntry) -> Option<(String, StoredEmbeddingIdentity)> {
    let identity = entry.embedding_identity.as_ref()?;
    if entry.chunk.policy_version.is_empty()
        || identity.bytes.is_empty()
        || identity.dimensions == 0
    {
        return None;
    }
    Some((entry.chunk.policy_version.clone(), identity.clone()))
}

/// Only representative keys are registered: members cannot bridge two groups.
/// Full-pool maps are required because fusion and reranking can separate copies.
pub(super) fn group_candidates(
    candidates: Vec<FusedChunkEntry>,
    mode: SearchDedup,
) -> Vec<PassageGroup> {
    let mut groups: Vec<PassageGroup> = Vec::new();
    let mut texts: HashMap<String, usize> = HashMap::new();
    let mut vectors: HashMap<(String, StoredEmbeddingIdentity), usize> = HashMap::new();
    for candidate in candidates {
        let text_match = (mode != SearchDedup::Off)
            .then(|| texts.get(&candidate.chunk.text).copied())
            .flatten();
        let vector = (mode == SearchDedup::TextAndVector)
            .then(|| vector_key(&candidate))
            .flatten();
        let vector_match = vector.as_ref().and_then(|key| vectors.get(key).copied());
        let chosen = text_match.into_iter().chain(vector_match).min();
        if let Some(index) = chosen {
            let mut reasons = Vec::with_capacity(2);
            if text_match == Some(index) {
                reasons.push(DuplicateReason::ExactText);
            }
            if vector_match == Some(index) {
                reasons.push(DuplicateReason::ExactVector);
            }
            groups[index].duplicates.push((candidate, reasons));
        } else {
            let index = groups.len();
            if mode != SearchDedup::Off {
                texts.insert(candidate.chunk.text.clone(), index);
            }
            if let Some(key) = vector {
                vectors.insert(key, index);
            }
            groups.push(PassageGroup {
                representative: candidate,
                duplicates: Vec::new(),
            });
        }
    }
    groups
}

#[cfg(test)]
mod tests;
