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

use std::sync::atomic::{AtomicU64, Ordering};
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

    ExponentialBuilder::default()
        .with_jitter()
        .with_min_delay(min_delay)
        .with_max_delay(max_delay)
        .with_max_times(cfg.max_retries as usize)
        .with_total_delay(Some(total_retry_budget(cfg)))
}

/// The `with_total_delay` budget [`retry_policy`] hands `backon` for `cfg`,
/// exposed separately so a caller's `.adjust()` closure ([`retry_after_
/// adjuster`]) can enforce the *same* number against the sleeps it actually
/// causes.
///
/// This has to be a real function, not a restated constant at each call
/// site: `backon` only ever charges *its own proposed* delay against the
/// budget it was configured with (see [`retry_after_adjuster`]'s doc comment
/// for why that isn't good enough on its own), so a caller enforcing this
/// independently needs the exact value `backon` was configured with — and
/// that value scales with `cfg.min_retry_delay`, which the test suite always
/// overrides to keep wall-clock time down (see `HttpSettings::
/// min_retry_delay`'s doc comment). Hardcoding 30s at each `.adjust()` call
/// site would silently stop tracking the *test* budget once a test shrinks
/// `min_retry_delay`, defeating the point of the independent tracker for
/// exactly the tests that most need it to be exercised.
pub fn total_retry_budget(cfg: &HttpSettings) -> Duration {
    let min_delay = cfg.min_retry_delay;
    min_delay.saturating_mul(TOTAL_DELAY_RATIO).max(min_delay)
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
/// [`retry_policy`]'s own doc comment on `with_max_times`).
///
/// That 30 s sleep half of the bound is only real because
/// [`retry_after_adjuster`] tracks its *own* cumulative honored-`Retry-After`
/// total independently of `backon`. `backon`'s internal `with_total_delay`
/// accounting only ever charges the delay *it* proposed for a given attempt,
/// never the value a caller's `.adjust()` closure substitutes in its place —
/// so a naive adjuster that returns `min(retry_after, INLINE_RETRY_AFTER_CAP)`
/// without its own tracker lets a server that keeps responding
/// `Retry-After: 30` blow straight through this budget: `backon` charges
/// each attempt's own (small, exponentially-growing-from-1s) proposal, sees
/// plenty of apparent headroom, and keeps allowing retries while the loop
/// actually sleeps 30 s each time. Three such retries sleep 90 s against a
/// budget `backon` still believes is barely touched, pushing the real worst
/// case to roughly 210 s (the same 4 attempts × 30 s of timeouts = 120 s,
/// plus 90 s of actual sleep instead of the intended 30 s budget) — this was
/// an observed defect on this branch, not a hypothetical. Once
/// [`retry_after_adjuster`] enforces the budget itself against the sleeps it
/// actually causes, 150 s is a real ceiling again, not just backon's
/// (misinformed) opinion of one. 30 s is generous enough to ride out a
/// short-lived 429 without making that budget dominate. See [`retry_policy`]
/// for why a non-default `min_retry_delay` scales this budget down with it
/// rather than leaving it fixed at 30 s.
const TOTAL_DELAY_RATIO: u32 = 30;

// ---------------------------------------------------------------------------
// Retry-After adjustment
// ---------------------------------------------------------------------------

/// Builds the `.adjust()` closure `backon`'s retry loop calls after every
/// failed, retryable attempt to decide how long to sleep before the next
/// one — shared by both outgoing-HTTP retry loops in the workspace
/// (`fetch::HttpUrlFetcher::fetch` and `embed::http_helper::send_with_retry`)
/// rather than duplicated near-identically at each, which is exactly the
/// kind of drift issue #207 set out to eliminate (qlty separately flagged
/// the sibling duplication between `embed/src/perplexity.rs` and
/// `embed/src/voyage.rs` on this same PR).
///
/// Call this **once per retry loop**, not once per process/client: the
/// returned closure owns a fresh, zeroed cumulative-sleep tracker, and that
/// tracker must be scoped to a single `fetch()`/`send_with_retry()` call —
/// reusing one closure (and its tracker) across multiple documents/requests
/// would make the second one inherit the first one's spent budget.
///
/// `total_budget` should always be [`total_retry_budget`] applied to the
/// same [`HttpSettings`] passed to [`retry_policy`] for this same retry
/// loop, so the independent tracking below (point 4) enforces the exact
/// number `backon` was configured with — including when a test shrinks
/// `HttpSettings::min_retry_delay` to keep wall-clock time down.
///
/// Behavior, in order:
///
/// 1. `dur?` — if `backon` has already decided to stop (its own
///    `max_retries`/`total_delay` accounting exhausted), this closure must
///    never resurrect that decision. Load-bearing, not stylistic: a version
///    of this closure that inspected the error before checking `dur`
///    returned `Some(retry_after)` unconditionally, which resurrects a
///    stopped retry loop on every subsequent poll against a server that
///    keeps sending 429 + `Retry-After` — an infinite loop that was actually
///    observed on this branch, not a hypothetical.
/// 2. No `Retry-After` on this error (a network-error `RetryError::Request`,
///    or a `Status` with no parsed header) → fall through to `dur`
///    unchanged, i.e. `backon`'s own computed exponential-backoff delay.
/// 3. A `Retry-After` over [`INLINE_RETRY_AFTER_CAP`] → `None`, ending the
///    retry loop immediately rather than clamping it down to the cap and
///    sleeping anyway. Waiting that long inline would eat into (or exceed)
///    `total_budget` on its own; the caller maps the resulting terminal
///    error to `RateLimited`/`RetriesExhausted`. This function has no side
///    channel to record a pacing cooldown from the oversized value itself —
///    a caller that wants that (as `fetch::lib`'s `HttpUrlFetcher::fetch`
///    does, via `HostLimiter::note_retry_after`) must record it from the
///    attempt's own response handling, *before* the `Err` carrying it
///    reaches this closure at all, not from here.
/// 4. Otherwise: honor the `Retry-After` value, but only if doing so keeps
///    this loop's *actual* cumulative sleep under `total_budget`, tracked
///    independently of `backon`'s own internal accounting via an `AtomicU64`
///    of milliseconds (an atomic rather than a `Cell`/`RefCell` because the
///    future this closure lives inside is boxed `dyn Future + Send` by
///    `#[async_trait]` on every `Embedder` impl that calls
///    `send_with_retry` — a `&Cell`/`&RefCell` held across an `.await` is
///    not `Send`, since neither type is `Sync`). See [`TOTAL_DELAY_RATIO`]'s
///    doc comment for *why* this independent tracking is necessary at all
///    (`backon` only ever charges its own proposed delay against the
///    budget, never the substituted value returned here). The budget check
///    happens *before* adding the candidate sleep to the tracker, and it is
///    a **pre-add** check — `spent_so_far + retry_after`, not `spent_so_far`
///    alone, is compared against `total_budget`. A post-hoc check (comparing
///    only what was already spent) was tried and rejected: at production
///    settings, where `total_budget` equals [`INLINE_RETRY_AFTER_CAP`]
///    exactly, a single maximal wait happens to consume the whole budget
///    either way, which looks like the two forms "collapse" to the same
///    behavior — but that equivalence only holds for a wait at the cap.
///    For anything smaller — `Retry-After: 29` against a 30 s budget, say —
///    a post-hoc check honors a *second* 29 s wait too (`29 s < 30 s` is
///    still true after the first), sleeping 58 s against a documented 30 s
///    budget. The pre-add form rejects that second wait: `29 s + 29 s = 58
///    s > 30 s budget`. A single honored wait exactly at `total_budget` is
///    still allowed (`0 + total_budget` is not `> total_budget`), so the
///    production one-maximal-wait case above still behaves identically.
pub fn retry_after_adjuster(
    total_budget: Duration,
) -> impl FnMut(&RetryError, Option<Duration>) -> Option<Duration> + Send + Sync + 'static {
    let spent_millis = AtomicU64::new(0);

    move |err: &RetryError, dur: Option<Duration>| {
        dur?;

        let retry_after = match err {
            RetryError::Status {
                retry_after: Some(retry_after),
                ..
            } => *retry_after,
            _ => return dur,
        };

        if retry_after > INLINE_RETRY_AFTER_CAP {
            return None;
        }

        // u64 millis comfortably holds any `Duration` this function ever
        // sees: every `retry_after` reaching here already passed through
        // `parse_retry_after`'s `RETRY_AFTER_PARSE_CAP` (120 s = 120_000 ms).
        let spent_so_far = Duration::from_millis(spent_millis.load(Ordering::Relaxed));
        if spent_so_far + retry_after > total_budget {
            return None;
        }

        spent_millis.fetch_add(retry_after.as_millis() as u64, Ordering::Relaxed);
        Some(retry_after)
    }
}

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
/// Retryable: HTTP 429, HTTP 408, any 5xx, a network-level timeout or
/// connect failure, and a body-transport failure — an origin that closes or
/// resets the connection after sending successful headers but before the
/// body finishes. Everything else is fatal — including every other 4xx, and
/// (the trap this predicate exists to avoid) an SSRF refusal.
///
/// # `is_body() || is_decode()`, and why both are needed
///
/// Both `fetch::lib`'s `classify_response` and `embed::http_helper::
/// send_with_retry` deliberately read the response body *inside* the
/// retried closure specifically so a truncated/reset body is retried like
/// any other transient failure — but until this predicate recognized the
/// error kinds below, that placement bought nothing.
///
/// The obvious guess is `e.is_body()` alone: reqwest's own docs describe it
/// as "related to the request or response body". It is necessary but not
/// sufficient. Both call sites read the body via `Response::bytes()` — never
/// `.json()`, and neither call site's `reqwest` client enables a
/// compression feature (`fetch/Cargo.toml`'s `default-features = false`
/// build has none active) — and reqwest 0.12's `Response::bytes()`
/// (`BodyExt::collect().map_err(crate::error::decode)` in
/// `async_impl/response.rs`) tags *every* failure it sees, transport or
/// not, as `Kind::Decode`, not `Kind::Body`. Confirmed empirically (see
/// `truncated_response_body_is_classified_transient` below): a connection
/// reset mid-body, driven through a raw TCP listener that promises more
/// bytes via `Content-Length` than it sends, surfaces as `reqwest::Error {
/// kind: Decode, source: hyper::Error(Body, Os { kind: ConnectionReset,
/// .. }) }` — `is_decode()` true, `is_body()` false. `is_body()` is kept
/// anyway, both for the (currently unreached, in this codebase) streaming
/// paths (`chunk()`/`bytes_stream()`) where reqwest does use `Kind::Body`,
/// and as cheap insurance against a future reqwest version changing which
/// kind `.bytes()` uses.
///
/// Retrying `is_decode()` unconditionally would normally be the wrong call
/// — a *genuine* decode failure (served bytes are not what the caller asked
/// for, e.g. malformed JSON from `Response::json()`, or a broken gzip
/// stream) is a data problem the origin will reproduce identically on a
/// retry, not a transport hiccup a second attempt might avoid, so retrying
/// it only burns the retry budget on an outcome that cannot change. It is
/// the right call *here* only because neither call site sharing this
/// predicate can currently produce that genuine case: `.bytes()` returns
/// raw, undecoded bytes (any JSON parsing embed's callers do happens later,
/// outside this retry loop, as a `serde_json::Error` that never reaches
/// `RetryError` at all), and no compression feature is enabled to make
/// `Kind::Decode` mean "corrupt gzip stream" either. If either call site
/// ever starts calling `.json()`/`.text()` on the success path, or a
/// compression feature gets enabled, this reasoning needs revisiting —
/// `is_decode()` would then also catch genuine, non-retryable content
/// failures.
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
            e.is_timeout() || e.is_connect() || e.is_body() || e.is_decode()
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

    /// Finding D regression: a body-transport failure — the origin closes
    /// the connection after sending headers but before finishing the body
    /// it promised via `Content-Length` — must be classified transient,
    /// matching `classify_response`'s own stated intent (`fetch::lib`) of
    /// reading the body *inside* the retry closure specifically so a
    /// truncated read gets retried like any other transient failure. Before
    /// this fix, `is_transient` only checked `is_timeout()`/`is_connect()`,
    /// neither of which a body-read failure satisfies, so that placement
    /// bought nothing.
    ///
    /// Also pins the `is_body()` vs `is_decode()` finding documented at
    /// length on [`is_transient`]'s own doc comment: this failure surfaces
    /// with `is_decode() == true` and `is_body() == false`, the opposite of
    /// what the error's plain-English shape ("connection reset mid-body")
    /// would suggest — `Response::bytes()` tags every failure it sees as
    /// `Kind::Decode`, transport or not. A predicate that only added
    /// `is_body()` would compile, look plausible, and still not fix this.
    ///
    /// Driven against a raw `TcpListener`, not wiremock: wiremock has no way
    /// to serve a response that lies about its own `Content-Length`, which
    /// is what is needed to make `Response::bytes()` itself fail rather than
    /// the initial `send()`.
    #[tokio::test]
    async fn truncated_response_body_is_classified_transient() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener must bind");
        let addr = listener
            .local_addr()
            .expect("listener must have a local addr");

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            if let Ok((mut socket, _)) = listener.accept().await {
                // Promises 100 bytes of body, sends 5, then the connection
                // closes (`socket` dropping at the end of this block) —
                // reqwest only discovers the mismatch when the body is
                // actually read, not when headers arrive.
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort")
                    .await;
            }
        });

        let client = reqwest::Client::builder()
            .build()
            .expect("client must build");
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("headers arrive intact; the failure surfaces on body read")
            .bytes()
            .await
            .expect_err("a body shorter than its own Content-Length must fail to read");

        // `Response::bytes()`'s own quirk (see `is_transient`'s doc comment):
        // this is a transport failure, but reqwest 0.12 tags it
        // `Kind::Decode`, not `Kind::Body`. Pinned explicitly so a future
        // reqwest upgrade that changes this is caught here, at the
        // precondition, rather than silently making the assertion below
        // pass for the wrong reason.
        assert!(
            err.is_decode() && !err.is_body(),
            "test precondition: expected this reqwest version's `Response::\
             bytes()` to tag a transport failure as Kind::Decode, not \
             Kind::Body — if this fails, reqwest's behavior changed and \
             `is_transient`'s doc comment needs re-checking against it: {err:?}"
        );
        assert!(
            !err.is_timeout() && !err.is_connect(),
            "test precondition: a truncated body must not itself look like a \
             timeout or connect error, or this test would not distinguish \
             the fix from the pre-fix predicate: {err:?}"
        );
        assert!(
            is_transient(&RetryError::Request(err)),
            "a truncated/reset response body must be classified transient"
        );
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

    // -----------------------------------------------------------------------
    // total_retry_budget
    // -----------------------------------------------------------------------

    #[test]
    fn total_retry_budget_matches_retry_policys_own_total_delay_ratio() {
        // Pins the formula against a literal, not just against itself, so a
        // future edit to `TOTAL_DELAY_RATIO` (or an accidental divergence
        // between this function and `retry_policy`'s inline use of it) shows
        // up here.
        let cfg = HttpSettings {
            min_retry_delay: Duration::from_millis(10),
            ..HttpSettings::default()
        };
        assert_eq!(total_retry_budget(&cfg), Duration::from_millis(300));
    }

    // -----------------------------------------------------------------------
    // retry_after_adjuster — pure logic, no real sleep, no mock server.
    //
    // Exercised again end-to-end through wiremock in `fetch::lib` and
    // `embed::http_helper`'s own test modules; these pin the same contract
    // at the closure level, deterministically and without any wall-clock
    // dependency.
    // -----------------------------------------------------------------------

    fn status_err_with_retry_after(retry_after: Duration) -> RetryError {
        RetryError::Status {
            status: StatusCode::TOO_MANY_REQUESTS,
            retry_after: Some(retry_after),
        }
    }

    #[test]
    fn adjuster_never_resurrects_a_stopped_loop() {
        // Regression for the load-bearing `dur?` short-circuit: backon
        // saying "stop" (`dur = None`) must win even when the error carries
        // an honorable `Retry-After` — an earlier version of this closure
        // that checked the error first produced an observed infinite loop
        // against a server that kept sending 429 + Retry-After.
        let mut adjust = retry_after_adjuster(Duration::from_secs(60));
        let err = status_err_with_retry_after(Duration::from_secs(1));
        assert_eq!(adjust(&err, None), None);
    }

    #[test]
    fn adjuster_falls_back_to_backons_delay_with_no_retry_after_hint() {
        let mut adjust = retry_after_adjuster(Duration::from_secs(60));
        let no_header = RetryError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            retry_after: None,
        };
        let backons_delay = Some(Duration::from_millis(250));
        assert_eq!(adjust(&no_header, backons_delay), backons_delay);

        let network_err = RetryError::Request(
            reqwest::Client::new()
                .get("not a url")
                .build()
                .expect_err("a non-URL request must fail to build"),
        );
        assert_eq!(adjust(&network_err, backons_delay), backons_delay);
    }

    /// Defect 1 regression: a `Retry-After` over the inline cap must end the
    /// loop (`None`), never get clamped down to the cap and slept on anyway.
    #[test]
    fn adjuster_gives_up_on_a_retry_after_over_the_inline_cap() {
        let mut adjust = retry_after_adjuster(Duration::from_secs(60));
        let err = status_err_with_retry_after(INLINE_RETRY_AFTER_CAP + Duration::from_secs(1));
        assert_eq!(adjust(&err, Some(Duration::from_millis(1))), None);
    }

    /// Guard against over-correcting defect 1: a value *at* the cap is still
    /// honored, not treated the same as one over it.
    #[test]
    fn adjuster_honors_a_retry_after_exactly_at_the_inline_cap() {
        let mut adjust = retry_after_adjuster(Duration::from_secs(60));
        let err = status_err_with_retry_after(INLINE_RETRY_AFTER_CAP);
        assert_eq!(
            adjust(&err, Some(Duration::from_millis(1))),
            Some(INLINE_RETRY_AFTER_CAP)
        );
    }

    /// Defect 2 regression, replacing an earlier version of this test
    /// (`adjuster_honors_the_first_wait_even_if_it_alone_covers_the_whole_
    /// budget`) that only proved the *first* honored wait can equal the
    /// whole budget — true under both the buggy post-hoc check this module
    /// used to have (`spent_so_far >= total_budget`) and the pre-add fix
    /// (`spent_so_far + retry_after > total_budget`), since `0 +
    /// total_budget` is not `> total_budget` either way. That
    /// non-distinguishing case was exactly the (wrong) justification
    /// previously accepted for the post-hoc form: "at production settings
    /// one maximal wait already consumes the whole budget, so pre-add and
    /// post-hoc collapse to the same behavior" — true only when the wait is
    /// itself at the cap. This version adds the case that actually
    /// separates the two: once the first wait has consumed the whole
    /// budget, *any* further honored wait — however small — must now be
    /// refused, because `spent_so_far + retry_after` exceeds `total_budget`
    /// even though `spent_so_far` alone does not exceed it until that next
    /// wait is added. The old post-hoc check would have wrongly honored the
    /// second wait here (`100ms >= 100ms budget` is false, so it would have
    /// slept an extra 1ms — small in this scaled-down test, but the same
    /// shape as the real defect: `Retry-After: 29` honored twice against a
    /// 30s budget).
    #[test]
    fn adjuster_honors_a_first_wait_at_the_whole_budget_but_rejects_any_further_wait() {
        let mut adjust = retry_after_adjuster(Duration::from_millis(100));
        let err = status_err_with_retry_after(Duration::from_millis(100));
        assert_eq!(
            adjust(&err, Some(Duration::from_millis(1))),
            Some(Duration::from_millis(100))
        );

        let tiny_err = status_err_with_retry_after(Duration::from_millis(1));
        assert_eq!(adjust(&tiny_err, Some(Duration::from_millis(1))), None);
    }

    /// Defect 2 regression, the core of it: once the running total of
    /// *honored* waits already covers the budget, a further honored wait is
    /// refused — even though `dur` is still `Some` (backon's own, much
    /// smaller, internal accounting still believes there is room). This is
    /// exactly the divergence `TOTAL_DELAY_RATIO`'s doc comment describes:
    /// without this independent tracker, a server that keeps returning
    /// `Retry-After` near the cap can make the loop sleep for many times the
    /// configured budget while backon still says "keep going".
    #[test]
    fn adjuster_bounds_cumulative_honored_waits_to_the_total_budget() {
        let mut adjust = retry_after_adjuster(Duration::from_millis(150));
        let err = status_err_with_retry_after(Duration::from_millis(60));
        let backons_own_tiny_delay = Some(Duration::from_millis(1));

        // First 60ms wait: 0 spent so far, 0+60=60ms <= 150ms budget —
        // honored, running total → 60ms.
        assert_eq!(
            adjust(&err, backons_own_tiny_delay),
            Some(Duration::from_millis(60))
        );
        // Second 60ms wait: 60ms spent so far, 60+60=120ms <= 150ms budget —
        // still honored (the check is against what spending this wait
        // *would bring the total to*, i.e. pre-add, not against what's
        // already spent), bringing the running total to 120ms.
        assert_eq!(
            adjust(&err, backons_own_tiny_delay),
            Some(Duration::from_millis(60))
        );
        // Third wait: 120ms already spent, 120+60=180ms > 150ms budget —
        // refused, even though backon's own (uninflated) accounting would
        // still say yes, and even though 120ms alone is still < 150ms (the
        // post-hoc check this replaces would have wrongly honored this
        // wait too).
        assert_eq!(adjust(&err, backons_own_tiny_delay), None);
    }

    /// Defect 2 regression, the exact scenario reported (issue reproduction:
    /// `Retry-After: 29` honored twice against a documented 30s budget),
    /// scaled down to stay pure-logic and instantaneous: repeated
    /// just-under-cap `Retry-After` values must never let cumulative
    /// honored sleep exceed `total_budget` in aggregate, even though each
    /// individual value alone would fit comfortably under it.
    #[test]
    fn adjuster_rejects_repeated_just_under_cap_waits_once_their_sum_exceeds_the_budget() {
        let mut adjust = retry_after_adjuster(Duration::from_millis(300));
        let err = status_err_with_retry_after(Duration::from_millis(290));
        let backons_own_tiny_delay = Some(Duration::from_millis(1));

        // First 290ms wait: 0 spent so far, 0+290=290ms <= 300ms budget —
        // honored.
        assert_eq!(
            adjust(&err, backons_own_tiny_delay),
            Some(Duration::from_millis(290))
        );
        // Second 290ms wait: 290ms already spent + 290ms candidate =
        // 580ms > 300ms budget — refused. Under the old post-hoc check
        // (`spent_so_far >= total_budget`), 290ms < 300ms was still true
        // here, so this wait was wrongly honored too, sleeping 580ms
        // against a 300ms budget — the exact defect this test pins.
        assert_eq!(adjust(&err, backons_own_tiny_delay), None);
    }
}
