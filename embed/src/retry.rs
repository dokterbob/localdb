//! Batching and per-request timeout policy for hosted embedding providers.
//!
//! Retry/backoff/jitter policy used to live here too, but it was a second,
//! hand-rolled implementation entirely separate from `fetch`'s outgoing-HTTP
//! path (issue #207) — and a worse one in three specific ways: it never
//! honored a server's `Retry-After` header, it had no jitter (so many
//! concurrently-failing requests to the same host all woke on the same
//! tick), and `backoff_for_attempt` computed its exponential curve by calling
//! `.as_secs()` *before* multiplying, so any sub-second `initial_backoff`
//! silently collapsed to `Duration::ZERO`. Hosted embedding providers now
//! share `fetch::http`'s retry policy (`fetch::http::retry_policy`,
//! `fetch::http::HttpSettings`) via `http_helper::send_with_retry`, the same
//! machinery `fetch::HttpUrlFetcher` uses for document fetches. See
//! `http_helper` for the call site.
//!
//! What's left here is deliberately *not* an HTTP concern: how many chunk
//! strings to pack into one request, and how long to wait for a single
//! request before giving up on it. Both stay local to `embed` because they
//! depend on provider-specific payload shape and latency expectations, not
//! on anything `fetch::http` models.

use std::time::Duration;

/// Batching and per-request timeout policy for hosted embedding requests.
///
/// All fields are public so callers can construct custom policies.
/// Use [`RetryPolicy::default()`] for the sensible defaults.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Per-request timeout. Default: 30 s.
    pub request_timeout: Duration,

    /// Chunk batch size per HTTP request. Default: 32.
    pub batch_size: usize,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            batch_size: 32,
        }
    }
}

impl RetryPolicy {
    /// Create a policy with custom settings.
    pub fn new(request_timeout: Duration, batch_size: usize) -> Self {
        Self {
            request_timeout,
            batch_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_has_expected_values() {
        let p = RetryPolicy::default();
        assert_eq!(p.request_timeout, Duration::from_secs(30));
        assert_eq!(p.batch_size, 32);
    }

    #[test]
    fn custom_policy() {
        let p = RetryPolicy::new(Duration::from_secs(60), 64);
        assert_eq!(p.batch_size, 64);
        assert_eq!(p.request_timeout, Duration::from_secs(60));
    }
}
