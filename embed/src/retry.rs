//! Retry, timeout, and batching policy for hosted embedding providers.
//!
//! # Defaults (sensible, documented here)
//!
//! - **Batch size**: 32 chunk strings per HTTP request
//! - **Request timeout**: 30 seconds
//! - **Max retries**: 3 attempts
//! - **Back-off**: exponential starting at 1 s (1 s, 2 s, 4 s)
//! - **Retry on**: network errors, HTTP 429, HTTP 5xx

use std::time::Duration;

/// Policy for retry, timeout, and batching of hosted embedding requests.
///
/// All fields are public so callers can construct custom policies.
/// Use [`RetryPolicy::default()`] for the sensible defaults.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first). Default: 3.
    pub max_attempts: u32,

    /// Initial back-off duration before the first retry. Default: 1 s.
    /// Each subsequent retry doubles this (exponential back-off).
    pub initial_backoff: Duration,

    /// Per-request timeout. Default: 30 s.
    pub request_timeout: Duration,

    /// Chunk batch size per HTTP request. Default: 32.
    pub batch_size: usize,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_secs(1),
            request_timeout: Duration::from_secs(30),
            batch_size: 32,
        }
    }
}

impl RetryPolicy {
    /// Create a policy with custom settings.
    pub fn new(
        max_attempts: u32,
        initial_backoff: Duration,
        request_timeout: Duration,
        batch_size: usize,
    ) -> Self {
        Self {
            max_attempts,
            initial_backoff,
            request_timeout,
            batch_size,
        }
    }

    /// Returns true if the HTTP status code should be retried.
    pub fn should_retry_status(&self, status: u16) -> bool {
        status == 429 || status >= 500
    }

    /// Compute the back-off duration for attempt `n` (0-indexed).
    ///
    /// Uses exponential back-off: `initial_backoff * 2^n`.
    /// Capped at 30 seconds to avoid excessive delays.
    pub fn backoff_for_attempt(&self, n: u32) -> Duration {
        let multiplier = 1u64.checked_shl(n).unwrap_or(u64::MAX);
        let secs = self
            .initial_backoff
            .as_secs()
            .saturating_mul(multiplier)
            .min(30);
        Duration::from_secs(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_has_expected_values() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.initial_backoff, Duration::from_secs(1));
        assert_eq!(p.request_timeout, Duration::from_secs(30));
        assert_eq!(p.batch_size, 32);
    }

    #[test]
    fn should_retry_on_429() {
        let p = RetryPolicy::default();
        assert!(p.should_retry_status(429), "429 must be retried");
    }

    #[test]
    fn should_retry_on_5xx() {
        let p = RetryPolicy::default();
        assert!(p.should_retry_status(500));
        assert!(p.should_retry_status(502));
        assert!(p.should_retry_status(503));
        assert!(p.should_retry_status(504));
    }

    #[test]
    fn should_not_retry_on_4xx_except_429() {
        let p = RetryPolicy::default();
        assert!(!p.should_retry_status(400));
        assert!(!p.should_retry_status(401));
        assert!(!p.should_retry_status(403));
        assert!(!p.should_retry_status(404));
        assert!(!p.should_retry_status(422));
    }

    #[test]
    fn backoff_doubles() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff_for_attempt(0), Duration::from_secs(1));
        assert_eq!(p.backoff_for_attempt(1), Duration::from_secs(2));
        assert_eq!(p.backoff_for_attempt(2), Duration::from_secs(4));
        assert_eq!(p.backoff_for_attempt(3), Duration::from_secs(8));
    }

    #[test]
    fn backoff_capped_at_30s() {
        let p = RetryPolicy::default();
        // Attempt 10 would be 1024 s without cap
        assert_eq!(p.backoff_for_attempt(10), Duration::from_secs(30));
    }

    #[test]
    fn custom_policy() {
        let p = RetryPolicy::new(5, Duration::from_millis(100), Duration::from_secs(60), 64);
        assert_eq!(p.max_attempts, 5);
        assert_eq!(p.batch_size, 64);
        assert_eq!(p.request_timeout, Duration::from_secs(60));
    }
}
