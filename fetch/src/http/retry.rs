//! Retry policy and transient-failure classification for outbound HTTP
//! requests (issue #207).
//!
//! Two things live here, and they are independent knobs:
//!
//!   - [`retry_policy`] builds the `backon` schedule (how many attempts, how
//!     long between them).
//!   - [`is_transient`] decides which *outcomes* are worth feeding to that
//!     schedule at all. A 404 is not retried no matter how generous the
//!     schedule is; a 429 is retried even on the very first attempt.
//!
//! # The shape a caller (the fetch loop) should use
//!
//! Drive the retry loop with a closure returning
//! `Result<localdb_core::ingestion::FetchResult, RetryError>` — reusing
//! `FetchResult` directly rather than inventing a parallel outcome type. Its
//! four variants already are exactly the four terminal, non-retryable
//! results a single attempt can produce:
//!
//!   - `Ok(FetchResult::Downloaded { .. })` — success; stop.
//!   - `Ok(FetchResult::NotModified)` — 304; stop (never retried).
//!   - `Ok(FetchResult::Gone)` — 404/410; stop (never retried).
//!   - `Ok(FetchResult::Blocked)` — the SSRF guard refused the destination,
//!     either at the preflight check or by [`is_blocked_error`] recovering it
//!     from a failed `send()`; stop (never retried, regardless of what
//!     [`is_transient`] would say about the underlying network error — the
//!     closure should recognize this case and return `Ok(Blocked)` rather
//!     than `Err(RetryError::Request(..))` in the first place, exactly as
//!     today's `HttpUrlFetcher::fetch` already does before this module
//!     existed).
//!
//! Every other case — a retryable-*candidate* status (429/408/5xx) or a
//! `send()` failure that was not a blocked destination — is `Err`:
//!
//!   - `Err(RetryError::Status { status, retry_after })` for a non-2xx/304
//!     response the closure decided is not `Gone`.
//!   - `Err(RetryError::Request(reqwest::Error))` for a failed `send()` that
//!     [`destination::is_blocked_error`] did *not* recognize.
//!
//! `backon`'s `.when(is_transient)` then decides whether that `Err` is worth
//! retrying; a fatal status (400, 403, ...) or a non-timeout/connect network
//! error reaches `.when()`, is classified `false`, and the loop returns it
//! immediately as the final error for the caller to translate into
//! `Error::ProviderUnavailable`.
//!
//! [`is_blocked_error`]: crate::destination::is_blocked_error

use std::time::Duration;

use backon::ExponentialBuilder;
use reqwest::StatusCode;

use crate::destination;
use crate::http::HttpSettings;

// ---------------------------------------------------------------------------
// Retry-After parsing
// ---------------------------------------------------------------------------

/// Upper bound applied to a parsed `Retry-After` value, before any of the
/// separate inline-sleep or cooldown caps in this module or in
/// [`super::limiter`] are applied on top.
///
/// A server is free to send `Retry-After: 999999999`; without a ceiling here
/// that parses successfully and propagates a multi-decade `Duration`
/// downstream, where every consumer (`min()` against 30 s inline, `min()`
/// against 60 s cooldown) still ends up correct in practice — but capping at
/// the parse boundary means every downstream computation only ever sees a
/// sane range, rather than relying on each call site to defend itself against
/// an adversarial header. 120 s is double the largest cap anything in this
/// module actually uses (the 60 s cooldown in `HostLimiter`), leaving room to
/// distinguish "large but plausible" from "not remotely a real value" if a
/// future caller ever wants to.
const RETRY_AFTER_PARSE_CAP: Duration = Duration::from_secs(120);

/// Parse an HTTP `Retry-After` header value.
///
/// Accepts both forms the spec allows: delta-seconds (`"120"`) and an HTTP
/// date (`"Wed, 21 Oct 2026 07:28:00 GMT"`), tried in that order since
/// delta-seconds is both the common case and unambiguous to detect (a bare
/// non-negative integer). A date in the past — a server telling us to retry
/// "5 minutes ago" — collapses to [`Duration::ZERO`] rather than underflowing
/// or propagating an error: it means "no useful wait", not "give up parsing".
/// A value that is neither parses as neither form returns `None`.
///
/// Every result is capped at [`RETRY_AFTER_PARSE_CAP`].
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();

    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(RETRY_AFTER_PARSE_CAP));
    }

    let when = httpdate::parse_http_date(value).ok()?;
    let wait = when
        .duration_since(std::time::SystemTime::now())
        .unwrap_or(Duration::ZERO);
    Some(wait.min(RETRY_AFTER_PARSE_CAP))
}

// ---------------------------------------------------------------------------
// Retry schedule
// ---------------------------------------------------------------------------

/// Ceiling on the *inline* sleep this crate will do for a single `Retry-After`
/// value before giving up on the current document and moving on.
///
/// A `Retry-After` at or under this cap is slept on directly. A larger one is
/// not slept on inline — waiting that long inside one job would eat into (or
/// exceed) the total retry budget (see [`TOTAL_DELAY_RATIO`]) on its own —
/// but per `HostLimiter::note_retry_after`, the value is still recorded as
/// that host's cooldown (itself capped separately, at 60 s), so the server's
/// guidance still shapes the pacing of *future* requests even when today's
/// document gives up on waiting for it.
pub const INLINE_RETRY_AFTER_CAP: Duration = Duration::from_secs(30);

/// Build the `backon` retry schedule from operator-configured settings.
///
/// - `with_jitter()` — spreads retries from many concurrently-failing
///   documents against the same host instead of having them all wake on the
///   same tick.
/// - `with_min_delay(cfg.min_retry_delay)` / `with_max_delay(..)` — the
///   exponential curve's floor and ceiling. In production
///   `min_retry_delay` is always [`HttpSettings`]'s 1 s default (see its doc
///   comment — it is not YAML-configurable), so `max_delay` is always 30 s in
///   practice, matching [`INLINE_RETRY_AFTER_CAP`] so a single computed
///   backoff is never itself the reason the total budget is blown. Both the
///   ceiling and the total budget below are derived from the floor at a
///   fixed 30:1 ratio rather than restated as their own constants, so that
///   `min_retry_delay`'s one legitimate override site — the test suite,
///   dialing the floor down to millisecond scale to keep retry tests fast —
///   scales the whole curve down with it instead of leaving `max_delay`/
///   `total_delay` stuck at production-sized values that would defeat the
///   point of overriding the floor at all.
/// - `with_max_times(cfg.max_retries)` — retry *count*, not attempt count:
///   `max_retries = 3` (the default) means up to 4 total attempts.
/// - `with_total_delay(Some(..))` — the hard cap described on
///   [`TOTAL_DELAY_RATIO`]; once cumulative sleep exceeds it, `backon` stops
///   retrying and returns the last error, even if `max_times` has not been
///   reached yet.
pub fn retry_policy(cfg: &HttpSettings) -> ExponentialBuilder {
    let min_delay = cfg.min_retry_delay;
    let max_delay = min_delay.saturating_mul(MAX_DELAY_RATIO).max(min_delay);
    let total_delay = min_delay.saturating_mul(TOTAL_DELAY_RATIO).max(min_delay);

    ExponentialBuilder::default()
        .with_jitter()
        .with_min_delay(min_delay)
        .with_max_delay(max_delay)
        .with_max_times(cfg.max_retries as usize)
        .with_total_delay(Some(total_delay))
}

/// `max_delay / min_delay` at production settings (30 s / 1 s). See
/// [`retry_policy`] for why the ratio, not the ceiling itself, is what a
/// non-default `min_retry_delay` preserves.
const MAX_DELAY_RATIO: u32 = 30;

/// `total_delay / min_delay` at production settings: a 30 s cumulative sleep
/// budget divided by the 1 s production floor.
///
/// Load-bearing, not cosmetic: the daemon runs exactly one ingestion job at a
/// time (`server/src/job_exec.rs`, issue #187's single-worker queue), so an
/// unbounded backoff on one document stalls *every other store's* indexing
/// behind it, not just the slow document's own progress. With the
/// pre-existing 30 s per-attempt timeout (`client_builder`), the worst case
/// for one document at production settings is bounded at `4 attempts × 30 s
/// timeout + 30 s total sleep budget ≈ 150 s` — `max_retries = 3` is a retry
/// *count*, not an attempt count, so it is 4 attempts (1 initial + 3
/// retries), not 3, that each pay the 30 s per-attempt timeout (see
/// [`retry_policy`]'s own doc comment on `with_max_times`). Compare roughly
/// 210 s if the sleep budget were unbounded instead (the same 4 attempts ×
/// 30 s of timeouts = 120 s, plus the 3 retries between them backing off at
/// up to 30 s each under the schedule in [`retry_policy`], i.e. up to 90 s of
/// sleep — 120 s + 90 s = 210 s). 30 s is generous enough to ride out a
/// short-lived 429 without making that budget dominate. See [`retry_policy`]
/// for why a non-default `min_retry_delay` scales this budget down with it
/// rather than leaving it fixed at 30 s.
const TOTAL_DELAY_RATIO: u32 = 30;

// ---------------------------------------------------------------------------
// Transient-failure classification
// ---------------------------------------------------------------------------

/// The non-terminal failure of a single fetch attempt — what the retry
/// closure returns as `Err` when the outcome was neither a success nor one of
/// `FetchResult`'s stable terminal variants (`NotModified`/`Gone`/`Blocked`).
///
/// See the module doc for the full shape a caller should drive the retry loop
/// with.
#[derive(Debug)]
pub enum RetryError {
    /// A response came back with a status this crate does not treat as
    /// terminal (not 2xx, not 304, not 404/410). Carries the parsed
    /// `Retry-After` value, if the response had one and it parsed — the
    /// caller uses it for both the inline sleep and
    /// `HostLimiter::note_retry_after`.
    Status {
        status: StatusCode,
        retry_after: Option<Duration>,
    },
    /// `send()` itself failed — connection refused, timed out, TLS error,
    /// DNS failure, or (recovered by [`destination::is_blocked_error`] in
    /// [`is_transient`] below) an SSRF refusal that reached this point rather
    /// than being caught by the caller's preflight check.
    Request(reqwest::Error),
}

/// Whether a failed attempt is worth retrying.
///
/// Retryable: HTTP 429, HTTP 408, any 5xx, and a network-level timeout or
/// connect failure. Everything else is fatal — including every other 4xx,
/// and (the trap this predicate exists to avoid) an SSRF refusal.
///
/// # Ordering is load-bearing
///
/// [`destination::is_blocked_error`] is checked **before** the
/// timeout/connect check, and short-circuits to `false` on a match. The SSRF
/// destination guard (`fetch::destination`) surfaces its refusal as an
/// ordinary-looking connect error — there is no other channel available to
/// it — so a naive `e.is_timeout() || e.is_connect()` would classify a
/// blocked destination as transient and retry it three times. That is a
/// correctness regression (a `Blocked` result should be immediate and
/// stable, never delayed) and a security-relevant one (it turns one refused
/// probe into several, and burns the retry budget on a destination that was
/// never going to succeed). See `is_transient_returns_false_for_blocked_
/// destination_before_checking_network_errors` below, which pins this
/// specific ordering against a real blocked-destination error.
pub fn is_transient(err: &RetryError) -> bool {
    match err {
        RetryError::Status { status, .. } => {
            *status == StatusCode::TOO_MANY_REQUESTS
                || *status == StatusCode::REQUEST_TIMEOUT
                || status.is_server_error()
        }
        RetryError::Request(e) => {
            if destination::is_blocked_error(e) {
                return false;
            }
            e.is_timeout() || e.is_connect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::SystemTime;

    // -----------------------------------------------------------------------
    // parse_retry_after
    // -----------------------------------------------------------------------

    #[test]
    fn parses_delta_seconds() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
    }

    #[test]
    fn parses_an_http_date_in_the_future() {
        let target = SystemTime::now() + Duration::from_secs(10);
        let header = httpdate::fmt_http_date(target);
        let parsed = parse_retry_after(&header).expect("a well-formed future date must parse");
        // httpdate formats to whole-second granularity and some (sub-ms) time
        // passes between building `target` and parsing it back, so assert a
        // tolerant range rather than an exact value.
        assert!(
            parsed >= Duration::from_secs(8) && parsed <= Duration::from_secs(10),
            "expected ~10s, got {parsed:?}"
        );
    }

    #[test]
    fn a_date_in_the_past_is_zero_not_an_error() {
        let parsed = parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(parsed, Some(Duration::ZERO));
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(parse_retry_after("not a retry-after value"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn an_absurd_delta_seconds_value_is_capped() {
        assert_eq!(parse_retry_after("999999999"), Some(RETRY_AFTER_PARSE_CAP));
    }

    #[test]
    fn an_absurd_future_date_is_capped() {
        let target = SystemTime::now() + Duration::from_secs(3600);
        let header = httpdate::fmt_http_date(target);
        assert_eq!(parse_retry_after(&header), Some(RETRY_AFTER_PARSE_CAP));
    }

    // -----------------------------------------------------------------------
    // is_transient — status classification table
    // -----------------------------------------------------------------------

    fn status_err(code: u16) -> RetryError {
        RetryError::Status {
            status: StatusCode::from_u16(code).expect("test status must be valid"),
            retry_after: None,
        }
    }

    #[test]
    fn transient_statuses() {
        for code in [429, 408, 500, 503] {
            assert!(is_transient(&status_err(code)), "{code} must be transient");
        }
    }

    #[test]
    fn fatal_statuses() {
        for code in [400, 403, 404, 410, 304] {
            assert!(
                !is_transient(&status_err(code)),
                "{code} must not be transient"
            );
        }
    }

    // -----------------------------------------------------------------------
    // is_transient — network errors
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn connect_error_is_transient() {
        // Nothing listens on port 1 (a well-known reserved TCP port), so this
        // fails fast and deterministically with a connect error — no real
        // sleep, no mock server.
        let client = reqwest::Client::builder()
            .build()
            .expect("client must build");
        let err = client
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(
            err.is_connect(),
            "test precondition: must be a connect error"
        );
        assert!(is_transient(&RetryError::Request(err)));
    }

    /// Pins the ordering documented on [`is_transient`]: a blocked
    /// destination must be classified `false` even though it reaches this
    /// predicate wrapped in the same "connect failed" shape as an ordinary
    /// transient network error.
    ///
    /// Built the same way `fetch::destination`'s own tests build a real
    /// rejection (see `public_only_refuses_a_name_that_resolves_to_loopback`
    /// in `fetch/src/lib.rs`): a client using `GuardedResolver` alone (no
    /// mock server needed) resolves `"localhost"` to loopback via a real,
    /// fast, local DNS lookup, and the resolver itself refuses it — so
    /// `send()` fails with a genuine `reqwest::Error` whose source chain
    /// contains the guard's marker error, without ever opening a socket.
    #[tokio::test]
    async fn is_transient_returns_false_for_blocked_destination_before_checking_network_errors() {
        let client = reqwest::Client::builder()
            .dns_resolver(Arc::new(destination::GuardedResolver))
            .build()
            .expect("client must build");
        let err = client
            .get("http://localhost:1/")
            .send()
            .await
            .expect_err("the guarded resolver must refuse a name resolving to loopback");
        assert!(
            destination::is_blocked_error(&err),
            "test precondition: this must be a real blocked-destination error, \
             or the test below proves nothing about the ordering it exists to pin"
        );
        assert!(
            !is_transient(&RetryError::Request(err)),
            "a blocked destination must never be classified as transient, \
             even though it surfaces as a connect-error-shaped reqwest::Error"
        );
    }
}
