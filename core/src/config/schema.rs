//! YAML config schema types.
//!
//! These types represent the raw user-written YAML configuration.
//! Unknown keys are rejected at parse time (via `deny_unknown_fields`).
//! The schema is versioned: `version: 1` is required.
//!
//! See specs/03-config.md §1, §5.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Top-level raw config (before validation)
// ---------------------------------------------------------------------------

/// Raw YAML config shape — the user's config file.
///
/// `#[serde(deny_unknown_fields)]` enforces strict key rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    /// Schema version; must be 1 in MVP. Required.
    pub version: u32,

    /// HTTP server settings.
    #[serde(default)]
    pub server: ServerConfig,

    /// Platform path overrides.
    #[serde(default)]
    pub paths: PathsConfig,

    /// Global indexing defaults inherited by all stores.
    #[serde(default)]
    pub defaults: DefaultsConfig,

    /// Declarative (YAML-owned) stores.
    #[serde(default)]
    pub stores: Vec<StoreConfig>,

    /// External embedding / LLM providers.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// HTTP server configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind address; loopback-only by default.
    #[serde(default = "default_bind")]
    pub bind: String,

    /// Port to listen on.
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    7700
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Optional platform path overrides.
///
/// `None` means use the platform default from `PlatformPaths`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
    /// Override for data directory (indexes, runtime-state DB, lock, socket).
    #[serde(default)]
    pub data: Option<String>,

    /// Override for model cache directory.
    #[serde(default)]
    pub models: Option<String>,

    /// Override for log directory.
    #[serde(default)]
    pub logs: Option<String>,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Global defaults; stores inherit from here unless they override.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultsConfig {
    /// Default indexing policy for all stores.
    #[serde(default)]
    pub indexing: IndexingPolicyConfig,
}

/// Indexing policy config — chunking + embedding + parsers as one unit.
///
/// A change to any field triggers a reindex (policy_version changes).
/// See specs/03-config.md §2 and specs/04-search-pipeline.md §4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexingPolicyConfig {
    /// Chunking settings.
    #[serde(default)]
    pub chunking: ChunkingPolicy,

    /// Embedding settings.
    #[serde(default)]
    pub embedding: EmbeddingPolicy,

    /// Ordered list of parser IDs to try (first match wins).
    ///
    /// Empty or absent defaults to `["pdf", "office", "html", "markdown", "plaintext"]`.
    /// Unknown IDs are rejected at config validation time.
    /// Order is load-bearing: placing `plaintext` before `html` would cause
    /// HTML files with a `.html` extension to be parsed as plain text.
    #[serde(default = "default_parser_ids")]
    pub parsers: Vec<String>,
}

impl Default for IndexingPolicyConfig {
    fn default() -> Self {
        Self {
            chunking: ChunkingPolicy::default(),
            embedding: EmbeddingPolicy::default(),
            parsers: default_parser_ids(),
        }
    }
}

fn default_parser_ids() -> Vec<String> {
    vec![
        "pdf".to_string(),
        "office".to_string(),
        "html".to_string(),
        "markdown".to_string(),
        "plaintext".to_string(),
    ]
}

/// Chunking policy configuration.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkingPolicy {
    /// Per-source-kind preset overrides (e.g. `prose`, `code`, `messages`).
    #[serde(default)]
    pub preset_overrides: HashMap<String, String>,
}

/// Embedding policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingPolicy {
    /// Model name / path.
    #[serde(default = "default_embedding_model")]
    pub model: String,

    /// Provider kind: "local-onnx", "openai-compatible", "perplexity", "voyage".
    #[serde(default = "default_embedding_provider")]
    pub provider: String,
}

impl Default for EmbeddingPolicy {
    fn default() -> Self {
        Self {
            model: default_embedding_model(),
            provider: default_embedding_provider(),
        }
    }
}

fn default_embedding_model() -> String {
    "pplx-embed-context-v1-0.6b".to_string()
}

fn default_embedding_provider() -> String {
    "local-onnx".to_string()
}

// ---------------------------------------------------------------------------
// Stores
// ---------------------------------------------------------------------------

/// A declarative (YAML-owned) store definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    /// Human-readable store name (unique per instance).
    pub name: String,

    /// Visibility: "private" or "shared". MVP: only private functional.
    #[serde(default = "default_visibility")]
    pub visibility: String,

    /// Backend kind: "lancedb" (default).
    #[serde(default = "default_backend")]
    pub backend: String,

    /// Indexing policy override, or null to inherit defaults.
    #[serde(default)]
    pub indexing: Option<IndexingPolicyConfig>,

    /// Sources attached to this store.
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
}

fn default_visibility() -> String {
    "private".to_string()
}

fn default_backend() -> String {
    "lancedb".to_string()
}

/// A source attached to a store (YAML-declared).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Source kind: "path" or "url".
    pub kind: String,

    // Path source fields
    /// Root path (for `kind: path`).
    #[serde(default)]
    pub root: Option<String>,

    /// Include globs (for `kind: path`).
    #[serde(default)]
    pub include: Vec<String>,

    /// Exclude globs (for `kind: path`).
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Chunking preset: "prose", "code", or "messages" (for `kind: path`).
    #[serde(default = "default_preset")]
    pub preset: String,

    // URL source fields
    /// URL to fetch (for `kind: url`).
    #[serde(default)]
    pub url: Option<String>,

    /// Refresh interval as a human-readable string (e.g. "24h", "30m").
    /// Validated to be a valid duration.
    #[serde(default)]
    pub refresh: Option<String>,
}

fn default_preset() -> String {
    "prose".to_string()
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// External provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Provider name (user-assigned label).
    pub name: String,

    /// Provider kind: "openai-compatible", "perplexity", "voyage".
    pub kind: String,

    /// Base URL for API calls.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Environment variable name that holds the API key. Never inline.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_config_defaults() {
        let cfg: RawConfig = serde_yaml::from_str("version: 1").unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.server.bind, "127.0.0.1");
        assert_eq!(cfg.server.port, 7700);
        assert!(cfg.stores.is_empty());
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn unknown_key_at_root_rejected() {
        let yaml = "version: 1\nunknown_field: foo\n";
        let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown root key should be rejected");
    }

    #[test]
    fn unknown_key_in_server_rejected() {
        let yaml = "version: 1\nserver:\n  bind: 127.0.0.1\n  port: 7700\n  typo_field: bad\n";
        let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown server key should be rejected");
    }

    #[test]
    fn embedding_policy_defaults() {
        let p = EmbeddingPolicy::default();
        assert_eq!(p.model, "pplx-embed-context-v1-0.6b");
        assert_eq!(p.provider, "local-onnx");
    }

    #[test]
    fn server_config_defaults() {
        let s = ServerConfig::default();
        assert_eq!(s.bind, "127.0.0.1");
        assert_eq!(s.port, 7700);
    }
}
