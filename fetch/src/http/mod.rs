//! Shared HTTP client policy: user agent, timeout, retry, and per-host
//! pacing — the pieces that are the same regardless of which destination
//! policy `HttpUrlFetcher` enforces (see `fetch::destination`).
//!
//! Motivation (issue #207): a feed with many entries hosted on the same
//! origin was observed firing on the order of twenty-three requests per
//! second at that origin, which is indistinguishable from a denial-of-service
//! attempt from the far end and earns localdb a 429 (or an IP ban) in
//! response. Two independent mechanisms fix that, and both live here so
//! `fetch::lib`'s `HttpUrlFetcher::fetch` (owned by a later stage) can compose
//! them without re-deriving either:
//!
//!   - [`retry`] classifies which failures are worth retrying at all (a 429
//!     or a transient network error; never a destination the SSRF guard
//!     refused) and how long to back off, honoring a server's own
//!     `Retry-After` guidance where present.
//!   - [`limiter::HostLimiter`] paces *every* request to a host, proactively,
//!     independent of whether any individual request ever fails — this is
//!     what actually stops the 23-requests-per-second pattern, since a token
//!     bucket smooths the burst before the origin ever has cause to reply
//!     with a 429 in the first place.
//!
//! This module intentionally knows nothing about `core`'s YAML config
//! *shape* — only [`HttpSettings`], a plain struct converted from
//! `localdb_core::config::HttpConfig` at the one composition point
//! ([`HttpSettings`]'s `From` impl below). That keeps `fetch` free of
//! config-format concerns (no `serde`/`schemars` derives in this crate) while
//! still letting operators reach every knob here through `config.yaml`.

use std::time::Duration;

use localdb_core::config::HttpConfig;

pub mod limiter;
pub mod retry;

pub use limiter::HostLimiter;
pub use retry::{
    is_transient, parse_retry_after, retry_policy, RetryError, INLINE_RETRY_AFTER_CAP,
};

/// Default `User-Agent` sent when the operator has not overridden it via
/// `http.user_agent` in `config.yaml`.
///
/// Replaces the previous hardcoded `"localdb/0.1"` literal in
/// `HttpUrlFetcher::builder()`, which never actually tracked the workspace
/// version (`0.1.0` at the time it was written, and every version since) —
/// `env!("CARGO_PKG_VERSION")` reads `fetch/Cargo.toml`'s `version.workspace
/// = true`, so this stays correct without hand-editing on every release.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "localdb/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/dokterbob/localdb)"
);

/// The `fetch`-side shape of `core`'s YAML `http:` section: plain runtime
/// settings, no `serde`/`schemars` derives, no knowledge of defaulting-via-
/// `#[serde(default = ...)]`. `core::config::HttpConfig` is still the single
/// source of truth for *default values* (`Default for HttpConfig` in
/// `core/src/config/schema.rs`) — the [`From`] impl below just flattens its
/// nested `rate_limit` block into this struct's fields, since `fetch` has no
/// use for the nesting once the config has been loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSettings {
    /// `User-Agent` header override. `None` means [`DEFAULT_USER_AGENT`].
    pub user_agent: Option<String>,
    /// Maximum number of retries for a request that fails with a retryable
    /// status or a transient network error. See [`retry::retry_policy`].
    pub max_retries: u32,
    /// Maximum sustained requests per second to a single public host. See
    /// [`limiter::HostLimiter`].
    pub requests_per_second: u32,
    /// Token-bucket burst capacity above the sustained rate.
    pub burst: u32,
    /// Floor of the exponential backoff curve [`retry::retry_policy`] builds
    /// — the delay before the *first* retry, absent a `Retry-After` hint.
    ///
    /// Deliberately **not** part of `core::config::HttpConfig` and therefore
    /// not reachable from `config.yaml`: `HttpConfig` is frozen (its JSON
    /// Schema artifact is drift-guarded by `core/tests/config_schema_drift.rs`
    /// per `specs/03-config.md` §8), and there is no operator-facing need for
    /// this knob — production always wants the same conservative floor. It
    /// exists purely as a test seam: `retry_policy`'s curve was originally
    /// hardcoded to a 1s floor regardless of `HttpSettings`, which made every
    /// wiremock test that forces a retry sleep for real (with jitter, up to
    /// 1s per retry) — multiplied across a dozen-odd retry tests, and
    /// catastrophically so for any test that exhausts several retries, that
    /// is many real seconds of dead test time for zero coverage benefit.
    /// [`From<&HttpConfig>`] always sets this to the 1s production default;
    /// only test code (`fetch::lib`'s test module) overrides it, to
    /// millisecond scale, to keep the retry test suite fast without changing
    /// what those tests actually prove (retry *counts* and *classification*,
    /// not real backoff timing — the one test that legitimately wants a real
    /// wall-clock wait, honoring a server's actual `Retry-After` value, gets
    /// that from the header override in `fetch`'s `.adjust` closure, which is
    /// independent of this floor).
    pub min_retry_delay: Duration,
}

impl From<&HttpConfig> for HttpSettings {
    fn from(cfg: &HttpConfig) -> Self {
        Self {
            user_agent: cfg.user_agent.clone(),
            max_retries: cfg.max_retries,
            requests_per_second: cfg.rate_limit.requests_per_second,
            burst: cfg.rate_limit.burst,
            min_retry_delay: DEFAULT_MIN_RETRY_DELAY,
        }
    }
}

/// Production floor for the retry backoff curve. See
/// [`HttpSettings::min_retry_delay`] for why this is a crate constant rather
/// than something `HttpConfig` exposes.
const DEFAULT_MIN_RETRY_DELAY: Duration = Duration::from_secs(1);

impl Default for HttpSettings {
    /// Mirrors `core::config::HttpConfig`'s own `Default` impl exactly —
    /// duplicated rather than routed through `HttpConfig::default()` because
    /// this struct must remain constructible (e.g. in tests, or for a caller
    /// with no config file at all) without depending on `core`'s serde
    /// defaulting machinery.
    fn default() -> Self {
        Self {
            user_agent: None,
            max_retries: 3,
            requests_per_second: 1,
            burst: 4,
            min_retry_delay: DEFAULT_MIN_RETRY_DELAY,
        }
    }
}

/// Settings shared by both `HttpUrlFetcher` constructors: the User-Agent
/// header and the request timeout.
///
/// Deliberately does **not** set a DNS resolver or redirect policy — those
/// two are the destination-guard layers (`destination::GuardedResolver`,
/// `destination::guarded_redirect_policy`), which only `new_public_only()`
/// applies. Folding them in here would make every client public-only-guarded
/// and break `new()`'s contract of reaching operator-configured loopback/LAN
/// addresses.
pub fn client_builder(cfg: &HttpSettings) -> reqwest::ClientBuilder {
    let user_agent = cfg.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT);
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_match_core_config_defaults() {
        let settings = HttpSettings::default();
        assert_eq!(settings.user_agent, None);
        assert_eq!(settings.max_retries, 3);
        assert_eq!(settings.requests_per_second, 1);
        assert_eq!(settings.burst, 4);
    }

    #[test]
    fn from_http_config_flattens_rate_limit() {
        let cfg = HttpConfig {
            user_agent: Some("custom-agent/1.0".to_string()),
            max_retries: 7,
            rate_limit: localdb_core::config::RateLimitConfig {
                requests_per_second: 2,
                burst: 9,
            },
        };
        let settings = HttpSettings::from(&cfg);
        assert_eq!(settings.user_agent.as_deref(), Some("custom-agent/1.0"));
        assert_eq!(settings.max_retries, 7);
        assert_eq!(settings.requests_per_second, 2);
        assert_eq!(settings.burst, 9);
    }

    #[test]
    fn client_builder_uses_default_user_agent_when_unset() {
        let settings = HttpSettings::default();
        // `ClientBuilder` exposes no getters, so the only thing a unit test
        // can prove locally is that construction succeeds; the header value
        // itself is exercised end-to-end once `HttpUrlFetcher` wires this in
        // (a later stage), via wiremock's `header` matcher.
        let built = client_builder(&settings).build();
        assert!(built.is_ok());
    }

    #[test]
    fn client_builder_accepts_a_user_agent_override() {
        let settings = HttpSettings {
            user_agent: Some("override/9.9".to_string()),
            ..HttpSettings::default()
        };
        let built = client_builder(&settings).build();
        assert!(built.is_ok());
    }
}
