//! Policy versioning for indexing configurations.
//!
//! `policy_version = hash(canonical serialization of the store's effective {chunking, embedding})`.
//!
//! Any change to the effective `{chunking, embedding}` policy changes the hash,
//! which triggers a reindex of that store.
//!
//! See specs/03-config.md §2 and specs/04-search-pipeline.md §4.

use crate::config::schema::IndexingPolicyConfig;

/// Compute the policy version hash for a given indexing policy.
///
/// Returns a hex-encoded blake3 hash of the canonical JSON serialization.
/// The hash is stable: same inputs → same hash; different inputs → different hash.
///
/// This is the `policy_version` field stored on every `Chunk`.
pub fn compute_policy_version(policy: &IndexingPolicyConfig) -> String {
    // Canonical serialization: sort_keys via BTreeMap to guarantee stable ordering
    let canonical = canonical_policy_json(policy);
    let hash = blake3::hash(canonical.as_bytes());
    hex::encode(hash.as_bytes())
}

/// Produce a canonical JSON string from the policy.
///
/// Uses sorted keys for determinism regardless of insertion order.
/// All three sub-policies (chunking, embedding, parsers) are included so that
/// a change to any of them changes the hash and triggers a reindex.
fn canonical_policy_json(policy: &IndexingPolicyConfig) -> String {
    use std::collections::BTreeMap;

    // Manually build ordered JSON to ensure canonical form
    let mut chunking_map: BTreeMap<&str, serde_json::Value> = BTreeMap::new();

    // Sort preset_overrides by key for stable serialization
    let mut preset_overrides_sorted: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &policy.chunking.preset_overrides {
        preset_overrides_sorted.insert(k.clone(), v.clone());
    }
    chunking_map.insert(
        "preset_overrides",
        serde_json::to_value(&preset_overrides_sorted).unwrap(),
    );

    let mut embedding_map: BTreeMap<&str, &str> = BTreeMap::new();
    embedding_map.insert("model", &policy.embedding.model);
    embedding_map.insert("provider", &policy.embedding.provider);

    let mut root: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    root.insert("chunking", serde_json::to_value(&chunking_map).unwrap());
    root.insert("embedding", serde_json::to_value(&embedding_map).unwrap());
    // parsers list is order-sensitive (first-match), so preserve insertion order
    root.insert("parsers", serde_json::to_value(&policy.parsers).unwrap());

    serde_json::to_string(&root).expect("canonical policy JSON serialization should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{ChunkingPolicy, EmbeddingPolicy, IndexingPolicyConfig};
    use std::collections::HashMap;

    fn make_default_policy() -> IndexingPolicyConfig {
        IndexingPolicyConfig::default()
    }

    fn make_policy(model: &str, provider: &str) -> IndexingPolicyConfig {
        IndexingPolicyConfig {
            embedding: EmbeddingPolicy {
                model: model.to_string(),
                provider: provider.to_string(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn same_policy_same_hash() {
        let p1 = make_default_policy();
        let p2 = make_default_policy();
        assert_eq!(
            compute_policy_version(&p1),
            compute_policy_version(&p2),
            "identical policies should produce the same hash"
        );
    }

    #[test]
    fn different_embedding_model_different_hash() {
        let p1 = make_policy("model-a", "local-onnx");
        let p2 = make_policy("model-b", "local-onnx");
        assert_ne!(
            compute_policy_version(&p1),
            compute_policy_version(&p2),
            "different embedding models should produce different policy hashes"
        );
    }

    #[test]
    fn different_embedding_provider_different_hash() {
        let p1 = make_policy("model-a", "local-onnx");
        let p2 = make_policy("model-a", "openai-compatible");
        assert_ne!(
            compute_policy_version(&p1),
            compute_policy_version(&p2),
            "different embedding providers should produce different policy hashes"
        );
    }

    #[test]
    fn different_chunking_preset_overrides_different_hash() {
        let mut overrides = HashMap::new();
        overrides.insert("prose".to_string(), "custom".to_string());

        let p1 = IndexingPolicyConfig::default();
        let p2 = IndexingPolicyConfig {
            chunking: ChunkingPolicy {
                preset_overrides: overrides,
            },
            ..Default::default()
        };
        assert_ne!(
            compute_policy_version(&p1),
            compute_policy_version(&p2),
            "different chunking overrides should produce different policy hashes"
        );
    }

    #[test]
    fn hash_is_deterministic_across_calls() {
        let policy = make_default_policy();
        let hash1 = compute_policy_version(&policy);
        let hash2 = compute_policy_version(&policy);
        let hash3 = compute_policy_version(&policy);
        assert_eq!(hash1, hash2);
        assert_eq!(hash2, hash3);
    }

    #[test]
    fn hash_is_hex_encoded_blake3() {
        let policy = make_default_policy();
        let hash = compute_policy_version(&policy);
        // blake3 produces 32 bytes → 64 hex chars
        assert_eq!(hash.len(), 64, "expected 64-char hex string, got: {}", hash);
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "expected all hex digits, got: {}",
            hash
        );
    }

    #[test]
    fn different_parsers_list_different_hash() {
        let p1 = IndexingPolicyConfig {
            parsers: vec!["pdf".to_string(), "html".to_string()],
            ..Default::default()
        };
        let p2 = IndexingPolicyConfig {
            parsers: vec!["markdown".to_string(), "plaintext".to_string()],
            ..Default::default()
        };
        assert_ne!(
            compute_policy_version(&p1),
            compute_policy_version(&p2),
            "different parsers lists should produce different policy hashes"
        );
    }

    #[test]
    fn parsers_order_affects_hash() {
        let p1 = IndexingPolicyConfig {
            parsers: vec!["pdf".to_string(), "html".to_string()],
            ..Default::default()
        };
        let p2 = IndexingPolicyConfig {
            parsers: vec!["html".to_string(), "pdf".to_string()],
            ..Default::default()
        };
        assert_ne!(
            compute_policy_version(&p1),
            compute_policy_version(&p2),
            "parser order is load-bearing and must affect the policy hash"
        );
    }

    #[test]
    fn preset_override_key_order_does_not_affect_hash() {
        // Two policies with the same overrides in different insertion order should hash the same
        let mut overrides1 = HashMap::new();
        overrides1.insert("code".to_string(), "custom-code".to_string());
        overrides1.insert("prose".to_string(), "custom-prose".to_string());

        let mut overrides2 = HashMap::new();
        overrides2.insert("prose".to_string(), "custom-prose".to_string());
        overrides2.insert("code".to_string(), "custom-code".to_string());

        let p1 = IndexingPolicyConfig {
            chunking: ChunkingPolicy {
                preset_overrides: overrides1,
            },
            ..Default::default()
        };
        let p2 = IndexingPolicyConfig {
            chunking: ChunkingPolicy {
                preset_overrides: overrides2,
            },
            ..Default::default()
        };
        assert_eq!(
            compute_policy_version(&p1),
            compute_policy_version(&p2),
            "key insertion order should not affect policy hash"
        );
    }
}
