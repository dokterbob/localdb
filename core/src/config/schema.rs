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
use std::net::IpAddr;

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

    /// Auth enforcement mode (specs/03-config.md §1, specs/05-surfaces.md §3):
    /// `auto` (default) enforces auth iff the daemon's actually-bound address
    /// is non-loopback; `required` always enforces; `off` never enforces and
    /// is a hard startup error (`invalid_config`) combined with a
    /// non-loopback bind.
    #[serde(default)]
    pub auth: ServerAuthMode,

    /// Optional client-reachable public base URL (specs/03-config.md §1).
    /// Only meaningful behind a TLS-terminating reverse proxy; used as the
    /// OAuth issuer/resource identifier in discovery responses and consent
    /// links. Surfaced to admins via `GET /v1/config`.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Number of daemon job-queue workers. Jobs for the same store never run
    /// concurrently regardless of this setting; values greater than 1 enable
    /// cross-store parallelism. Default 1. Must be at least 1 —
    /// `validate_config` (`core/src/config/loader.rs`) rejects `0` at load
    /// time, so the emitted JSON Schema declares the same floor
    /// (`minimum: 1`, not schemars' derived-from-`usize` `minimum: 0`)
    /// rather than accepting a value the loader will turn around and
    /// reject — same fix as `RateLimitConfig`'s `requests_per_second`/
    /// `burst` below.
    #[schemars(range(min = 1))]
    #[serde(default = "default_job_workers")]
    pub job_workers: usize,

    /// Abuse limits on the daemon's public, unauthenticated auth routes
    /// (specs/05-surfaces.md §3.1): the dynamic-client-registration cap/TTL
    /// and the per-client-IP rate limit.
    #[serde(default)]
    pub abuse_limits: AbuseLimitsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            auth: ServerAuthMode::default(),
            public_url: None,
            job_workers: default_job_workers(),
            abuse_limits: AbuseLimitsConfig::default(),
        }
    }
}

/// Abuse limits for the daemon's public, unauthenticated auth routes
/// (specs/05-surfaces.md §3.1): `/authorize`, `/join`, `/token`, `/revoke`,
/// `/register`, `/v1/invites/redeem`, `/v1/invites/requests/{id}/poll`. The
/// `/.well-known/*` discovery documents are never limited — they are static,
/// cheap to serve, and a client needs them before it can even attempt any
/// of the routes above.
///
/// Mirrors `HttpConfig`/`RateLimitConfig`'s naming for the outbound
/// per-host limiter (`requests_per_second`/`burst`), since both are the
/// same token-bucket shape applied in the opposite direction (inbound here,
/// keyed on client IP instead of destination host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AbuseLimitsConfig {
    /// Master switch for every limit in this section. A direct loopback TCP
    /// peer that is not itself listed in `trusted_proxies` is exempt from
    /// the rate limit regardless of this setting (a local daemon must never
    /// throttle its own user); a client address taken from
    /// `X-Forwarded-For` is always rate-limited even when it is itself a
    /// loopback address, since nothing authenticates the header's content,
    /// and a trusted proxy whose header yields no usable client address is
    /// rate-limited under the proxy's own address — this switch only
    /// affects those non-exempt callers and the registration cap/TTL.
    #[serde(default = "default_abuse_limits_enable")]
    pub enable: bool,

    /// Maximum number of dynamically registered OAuth2 clients
    /// (`POST /register`, RFC 7591) kept at once. The built-in `localdb-cli`
    /// client does not count toward this and is never pruned. Must be at
    /// least 1 — `validate_config` rejects `0`.
    #[schemars(range(min = 1))]
    #[serde(default = "default_max_dynamic_clients")]
    pub max_dynamic_clients: u64,

    /// How long (seconds) a dynamically registered client may sit unused —
    /// no authorization code ever issued to it — before a later
    /// registration attempt may prune it to make room under
    /// `max_dynamic_clients`. Must be between 1 and 315360000 (10 years) —
    /// `validate_config` rejects `0` and anything past the 10-year ceiling,
    /// since an unbounded value here reaches `chrono` duration arithmetic
    /// that would otherwise overflow.
    #[schemars(range(min = 1, max = 315_360_000))]
    #[serde(default = "default_unused_client_ttl_secs")]
    pub unused_client_ttl_secs: i64,

    /// Maximum sustained requests per second from a single client IP across
    /// the limited routes above. Must be at least 1 — `validate_config`
    /// rejects `0`.
    #[schemars(range(min = 1))]
    #[serde(default = "default_abuse_requests_per_second")]
    pub requests_per_second: u32,

    /// Maximum burst size above the sustained rate (token bucket capacity).
    /// Must be at least 1 — `validate_config` rejects `0`.
    #[schemars(range(min = 1))]
    #[serde(default = "default_abuse_burst")]
    pub burst: u32,

    /// Peer addresses this daemon forwards from, each a single IPv4/IPv6
    /// address or that address with a CIDR prefix length (`203.0.113.5`,
    /// `203.0.113.0/24`, `::1`, `2001:db8::/32`). Default empty: no peer is
    /// trusted, so every request is limited and exempted by its own TCP
    /// peer address and `X-Forwarded-For` is never consulted. When the TCP
    /// peer address falls inside one of these ranges, the client address
    /// used for the rate limit is instead taken from `X-Forwarded-For`
    /// (walked right to left, skipping entries that are themselves trusted
    /// proxies) — never exempt as loopback, even when the resulting address
    /// is itself loopback, and never the proxy's own address unless the
    /// header yields nothing usable, in which case the request is
    /// rate-limited under that address instead — see
    /// [05-surfaces.md §3.1](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md#31-authentication).
    /// Listing a proxy here that does not itself overwrite or correctly
    /// append to `X-Forwarded-For` lets any client spoof its own address —
    /// only list proxies you control and that set the header correctly.
    /// `validate_config` rejects any entry that does not parse as an
    /// address or CIDR range, and rejects an IPv4-mapped IPv6 entry
    /// (`::ffff:a.b.c.d`, or a CIDR range whose address is written in that
    /// form) outright — write the plain IPv4 form instead, since the
    /// resolved peer address is itself normalized to IPv4 before matching,
    /// so the mapped form could never match anyway.
    ///
    /// An IPv4 and an IPv6 range never match each other's address family:
    /// `::/0` does not cover an IPv4 peer, and `0.0.0.0/0` does not cover an
    /// IPv6 one. An operator who means "trust every peer" must list both
    /// `0.0.0.0/0` and `::/0` rather than either alone; one alone silently
    /// leaves every peer of the other family untrusted (and, if it is a
    /// direct loopback peer, not exempt from the rate limit either).
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

/// Parse a `server.abuse_limits.trusted_proxies` entry: a single IPv4/IPv6
/// address, or that address with a `/<prefix-len>` suffix. No suffix means
/// the address's own full width (`/32` for IPv4, `/128` for IPv6). Rejects an
/// IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) outright, naming the plain
/// IPv4 form to use instead: `server::auth::trusted_proxy::unmap_ipv4`
/// normalizes the resolved peer address to IPv4 before matching it against
/// this list, so a mapped entry here could never match that normalized
/// address — accepting it would silently configure a proxy entry that can
/// never take effect. Used both by `validate_config` (to reject a malformed
/// entry at load time) and by `server`'s trusted-proxy matching (to build
/// its runtime CIDR list from the same, already-validated strings).
pub fn parse_ip_cidr(input: &str) -> Result<(IpAddr, u8), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("must not be empty".to_string());
    }
    let (addr_part, prefix_part) = match trimmed.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (trimmed, None),
    };
    let addr: IpAddr = addr_part
        .parse()
        .map_err(|_| format!("'{trimmed}' is not a valid IP address or CIDR range"))?;
    if let IpAddr::V6(v6) = addr {
        if let Some(mapped) = v6.to_ipv4_mapped() {
            return Err(format!(
                "'{trimmed}' is an IPv4-mapped IPv6 address; use the IPv4 form instead (e.g. \
                 \"{mapped}\")"
            ));
        }
    }
    let max_prefix: u8 = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    let prefix_len = match prefix_part {
        None => max_prefix,
        Some(raw) => {
            let parsed: u8 = raw
                .parse()
                .map_err(|_| format!("'{trimmed}' has an invalid CIDR prefix length"))?;
            if parsed > max_prefix {
                return Err(format!(
                    "'{trimmed}' has a CIDR prefix length greater than {max_prefix}"
                ));
            }
            parsed
        }
    };
    Ok((addr, prefix_len))
}

impl Default for AbuseLimitsConfig {
    fn default() -> Self {
        Self {
            enable: default_abuse_limits_enable(),
            max_dynamic_clients: default_max_dynamic_clients(),
            unused_client_ttl_secs: default_unused_client_ttl_secs(),
            requests_per_second: default_abuse_requests_per_second(),
            burst: default_abuse_burst(),
            trusted_proxies: Vec::new(),
        }
    }
}

fn default_abuse_limits_enable() -> bool {
    true
}

fn default_max_dynamic_clients() -> u64 {
    1000
}

fn default_unused_client_ttl_secs() -> i64 {
    24 * 60 * 60
}

/// Upper bound on `server.abuse_limits.unused_client_ttl_secs`
/// (`core::config::loader::validate_config`): 10 years is ample for any
/// real deployment, and keeps the negated delta `AuthService::
/// register_client` feeds to `rfc3339_from_now` comfortably inside the
/// range that function's own clamp accepts without ever engaging it.
pub const MAX_UNUSED_CLIENT_TTL_SECS: i64 = 10 * 365 * 24 * 60 * 60;

/// 5 req/s sustained comfortably clears the CLI invite-approval poll's 1s
/// interval (`cli::cmds::login::poll_until_decided`) with headroom for
/// several concurrent logins sharing one IP (e.g. behind NAT).
fn default_abuse_requests_per_second() -> u32 {
    5
}

fn default_abuse_burst() -> u32 {
    20
}

/// `server.auth` values (specs/05-surfaces.md §3). Unknown values are
/// rejected at parse time like any other invalid enum content, consistent
/// with the strict unknown-key policy (specs/03-config.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServerAuthMode {
    /// Enforce auth iff the actually-bound address is non-loopback.
    #[default]
    Auto,
    /// Always enforce auth, including on loopback binds.
    Required,
    /// Never enforce auth; refuse to start on a non-loopback bind.
    Off,
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
mod tests;
