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
    // Chunking algorithm identifier: bump to force a reindex when the chunking
    // implementation changes in a way that alters chunk boundaries.
    chunking_map.insert(
        "algorithm",
        serde_json::Value::String("textsplitter-md-v6".into()),
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

    // -- http: config never feeds the policy hash ---------------------------
    //
    // Issue #207 adversarial review, finding 3: the previous version of this
    // test compared `compute_policy_version(&base.defaults.indexing)`
    // against `compute_policy_version(&changed.defaults.indexing)` for two
    // `RawConfig`s that differed *only* in `http:`. That comparison is a
    // tautology, not a regression test: `RawConfig { http: ..,
    // ..Default::default() }` leaves every other field — including
    // `defaults.indexing` — byte-identical to `RawConfig::default()`, so
    // `base.defaults.indexing` and `changed.defaults.indexing` were always
    // equal *before* either one ever reached `compute_policy_version`. The
    // assertion would keep passing even if `compute_policy_version` were
    // broken in some unrelated way (e.g. always returning a constant), which
    // is exactly the kind of "safety net" that gives false confidence.
    //
    // The invariant this test module is named for — "`http:` can never
    // influence `policy_version`" — is real, but it is a **compile-time**
    // fact, not a runtime one, and no runtime test can strengthen it further
    // than the type signature already does: `compute_policy_version` takes
    // `&IndexingPolicyConfig` (see above), and `HttpConfig` is a *sibling*
    // field on `RawConfig` (`core/src/config/schema.rs`), never nested
    // inside `IndexingPolicyConfig`. There is no `http` field anywhere in
    // `IndexingPolicyConfig`'s definition for a value to flow through, so a
    // `RawConfig` differing only in `http:` is *structurally* incapable of
    // changing what `compute_policy_version` sees — not "happens not to" in
    // today's implementation, but "cannot, short of first changing the
    // struct definitions themselves." Every real caller confirms this is
    // also how the invariant is actually consumed: `server/src/job_exec.rs`,
    // `server/src/state.rs`, and `cli/src/app_db.rs` all call
    // `compute_policy_version(&yaml.defaults.indexing)` (or an equivalent
    // `&IndexingPolicyConfig` local), never `&yaml` or `&yaml.http` — there
    // is no production call site this test could exercise "the real entry
    // point" through that isn't already just `&IndexingPolicyConfig` itself.
    //
    // What a runtime test *can* still usefully pin down, and what the
    // deleted test did not, is a negative control: proof that this
    // particular comparison pattern (`compute_policy_version` on two
    // `IndexingPolicyConfig`s derived from otherwise-equal `RawConfig`s) is
    // actually capable of detecting a hash difference at all, so a change
    // that legitimately *should* bump `policy_version` doesn't slip past
    // silently. `indexing_policy_change_under_shared_http_config_changes_
    // hash` below is that negative control, sharing this test's exact
    // `RawConfig`-level construction style but varying `defaults.indexing`
    // (specifically `embedding.model`) instead of `http`.

    #[test]
    fn indexing_policy_change_under_shared_http_config_changes_hash() {
        // Negative control for the structural argument above: two
        // `RawConfig`s that share the exact same non-default `http:` block
        // but differ in `defaults.indexing.embedding.model` must produce
        // *different* policy_version hashes. This is the mirror image of
        // "http never affects the hash" — it proves a real indexing-relevant
        // field, routed through the same `RawConfig -> .defaults.indexing ->
        // compute_policy_version` path a production caller uses, is not
        // silently ignored by that path. Without this, a broken
        // `compute_policy_version` that ignored its argument entirely would
        // have nothing in this test module to catch it.
        use crate::config::schema::{HttpConfig, RateLimitConfig, RawConfig};

        let shared_http = HttpConfig {
            user_agent: Some("custom-agent/1.0".to_string()),
            max_retries: 99,
            rate_limit: RateLimitConfig {
                requests_per_second: 42,
                burst: 100,
            },
        };

        let mut base = RawConfig {
            http: shared_http.clone(),
            ..Default::default()
        };
        base.defaults.indexing.embedding.model = "model-a".to_string();

        let mut changed = RawConfig {
            http: shared_http,
            ..Default::default()
        };
        changed.defaults.indexing.embedding.model = "model-b".to_string();

        assert_eq!(
            base.http, changed.http,
            "test fixture sanity: the two configs must share the same http block"
        );
        assert_ne!(
            compute_policy_version(&base.defaults.indexing),
            compute_policy_version(&changed.defaults.indexing),
            "a changed embedding.model must change the policy_version hash, \
             even under an unchanged (and non-default) shared http: block"
        );
    }
}
