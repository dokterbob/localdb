//! YAML config schema types.
//!
//! These types represent the raw user-written YAML configuration.
//! Unknown keys are rejected at parse time (via `deny_unknown_fields`).
//! The schema is versioned: `version: 1` is required.
//!
//! See specs/03-config.md §1, §5.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Top-level raw config (before validation)
// ---------------------------------------------------------------------------

/// Raw YAML config shape — the user's config file.
///
/// `#[serde(deny_unknown_fields)]` enforces strict key rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    /// Schema version; must be 1 in MVP. Required.
    pub version: u32,

    /// Editor schema reference (`$schema:` key), written by the auto-generated
    /// config template; accepted and semantically ignored on load.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,

    /// HTTP server settings.
    #[serde(default)]
    pub server: ServerConfig,

    /// Platform path overrides.
    #[serde(default)]
    pub paths: PathsConfig,

    /// Global indexing defaults inherited by all stores.
    #[serde(default)]
    pub defaults: DefaultsConfig,

    /// External embedding / LLM providers.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,

    /// Outbound HTTP client policy (user agent, retries, per-host rate limiting).
    #[serde(default)]
    pub http: HttpConfig,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            version: 1,
            schema: None,
            server: ServerConfig::default(),
            paths: PathsConfig::default(),
            defaults: DefaultsConfig::default(),
            providers: Vec::new(),
            http: HttpConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// HTTP server configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind address; loopback-only by default.
    #[serde(default = "default_bind")]
    pub bind: String,

    /// Port to listen on.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Number of daemon job-queue workers. Jobs for the same store never run
    /// concurrently regardless of this setting; values greater than 1 enable
    /// cross-store parallelism. Default 1.
    #[serde(default = "default_job_workers")]
    pub job_workers: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            job_workers: default_job_workers(),
        }
    }
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    7700
}

fn default_job_workers() -> usize {
    1
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Optional platform path overrides.
///
/// `None` means use the platform default from `PlatformPaths`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// Empty or absent defaults to `["pdf", "epub", "office", "html", "markdown", "plaintext"]`.
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
        "epub".to_string(),
        "office".to_string(),
        "html".to_string(),
        "markdown".to_string(),
        "plaintext".to_string(),
    ]
}

/// Chunking policy configuration.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkingPolicy {
    /// Per-source-kind preset overrides (e.g. `prose`, `code`, `messages`).
    #[serde(default)]
    pub preset_overrides: HashMap<String, String>,
}

/// Embedding policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingPolicy {
    /// Model name / path.
    #[serde(default = "default_embedding_model")]
    pub model: String,

    /// Provider kind. Local options:
    /// - `"local"` — auto: on macOS with CoreML support use CoreML, else ONNX.
    /// - `"local-coreml"` — force in-process CoreML (macOS/Apple Silicon only).
    /// - `"local-onnx"` — force in-process ONNX inference.
    ///
    /// Hosted options: `"openai-compatible"`, `"perplexity"`, `"voyage"`.
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
    "local".to_string()
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// External provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Outbound HTTP client policy: applies to every request localdb makes to
/// fetch content (file/URL/feed ingestion) — not to the `server:` block
/// above, which configures the *inbound* daemon listener.
///
/// Deliberately top-level rather than nested under `defaults.indexing`: it
/// governs network behavior, not chunk/embedding semantics, so changing it
/// never bumps a store's `policy_version` or triggers a reindex.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// `User-Agent` header sent with every outbound request. `~`/omitted
    /// means `localdb/<version> (+https://github.com/dokterbob/localdb)`.
    #[serde(default)]
    pub user_agent: Option<String>,

    /// Maximum number of retries for a request that fails with a retryable
    /// status (e.g. a rate limit or transient server error) before giving up.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Per-destination-host rate limiting for outbound requests.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            user_agent: default_user_agent(),
            max_retries: default_max_retries(),
            rate_limit: RateLimitConfig::default(),
        }
    }
}

fn default_user_agent() -> Option<String> {
    None
}

fn default_max_retries() -> u32 {
    3
}

/// Rate limit applied per public destination host for outbound HTTP
/// requests. Loopback and LAN destinations are exempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Maximum sustained requests per second to a single public host. Must be
    /// at least 1 — `validate_config` (`core/src/config/loader.rs`) rejects
    /// `0` at load time, so the emitted JSON Schema declares the same floor
    /// (`minimum: 1`, not schemars' derived-from-`u32` `minimum: 0`) rather
    /// than accepting a value the loader will turn around and reject.
    #[schemars(range(min = 1))]
    #[serde(default = "default_requests_per_second")]
    pub requests_per_second: u32,

    /// Maximum burst size above the sustained rate (token bucket capacity).
    /// Must be at least 1, for the same reason as `requests_per_second`
    /// above: `validate_config` rejects `0`, so the schema does too.
    #[schemars(range(min = 1))]
    #[serde(default = "default_burst")]
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_second: default_requests_per_second(),
            burst: default_burst(),
        }
    }
}

fn default_requests_per_second() -> u32 {
    1
}

fn default_burst() -> u32 {
    4
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
        assert!(cfg.providers.is_empty());
        assert_eq!(cfg.schema, None);
    }

    #[test]
    fn raw_config_accepts_dollar_schema_key() {
        let yaml = "version: 1\n$schema: https://example.com/x.json\n";
        let cfg: RawConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.schema, Some("https://example.com/x.json".to_string()));
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
    fn raw_config_defaults_include_http() {
        let cfg: RawConfig = serde_yaml::from_str("version: 1").unwrap();
        assert_eq!(cfg.http, HttpConfig::default());
        assert_eq!(cfg.http.user_agent, None);
        assert_eq!(cfg.http.max_retries, 3);
        assert_eq!(cfg.http.rate_limit.requests_per_second, 1);
        assert_eq!(cfg.http.rate_limit.burst, 4);
    }

    #[test]
    fn http_config_defaults() {
        let h = HttpConfig::default();
        assert_eq!(h.user_agent, None);
        assert_eq!(h.max_retries, 3);
        assert_eq!(h.rate_limit, RateLimitConfig::default());
    }

    #[test]
    fn rate_limit_config_defaults() {
        let r = RateLimitConfig::default();
        assert_eq!(r.requests_per_second, 1);
        assert_eq!(r.burst, 4);
    }

    #[test]
    fn unknown_key_in_http_rejected() {
        let yaml = "version: 1\nhttp:\n  max_retries: 3\n  typo_field: bad\n";
        let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown http key should be rejected");
    }

    #[test]
    fn unknown_key_in_http_rate_limit_rejected() {
        let yaml =
            "version: 1\nhttp:\n  rate_limit:\n    requests_per_second: 1\n    typo_field: bad\n";
        let result: Result<RawConfig, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown http.rate_limit key should be rejected"
        );
    }

    #[test]
    fn raw_config_default_matches_bare_version_1() {
        // Default::default() must agree with parsing a minimal config, since
        // work item 2 relies on `..Default::default()` at every literal
        // construction site standing in for "every field at its platform
        // default" exactly as a bare `version: 1` config would produce.
        let parsed: RawConfig = serde_yaml::from_str("version: 1").unwrap();
        assert_eq!(parsed, RawConfig::default());
    }

    #[test]
    fn embedding_policy_defaults() {
        let p = EmbeddingPolicy::default();
        assert_eq!(p.model, "pplx-embed-context-v1-0.6b");
        assert_eq!(p.provider, "local");
    }

    #[test]
    fn server_config_defaults() {
        let s = ServerConfig::default();
        assert_eq!(s.bind, "127.0.0.1");
        assert_eq!(s.port, 7700);
        assert_eq!(s.job_workers, 1);
    }

    /// This list is duplicated in `extract::registry::default_parser_ids`
    /// (core cannot depend on extract). The two must stay byte-for-byte
    /// identical: the order feeds the policy-version hash and the chain's
    /// first-match priority. Update both lists together.
    #[test]
    fn default_parser_ids_match_extract_registry() {
        assert_eq!(
            default_parser_ids(),
            vec!["pdf", "epub", "office", "html", "markdown", "plaintext"],
            "schema default_parser_ids must match extract::registry::default_parser_ids"
        );
    }
}
