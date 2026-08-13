//! Per-host outbound pacing (issue #207).
//!
//! Motivation, restated from `super`: a feed with many entries on one origin
//! was observed firing ~23 requests/second at it. `retry` only reacts *after*
//! a request has already failed; this module is the proactive half — a token
//! bucket per destination host that smooths bursts before the origin ever has
//! cause to reply with a 429, plus a per-host cooldown that a 429's own
//! `Retry-After` can extend so the *next* request (to the next feed entry on
//! the same host, not just a retry of this one) inherits the back-off too.
//! That inheritance is what actually stops a burst pattern — reactive retry
//! alone only slows down the one request that already got a 429, while every
//! other request already in flight or about to be sent keeps hammering the
//! same host at the same rate.
//!
//! # Host, not registrable domain
//!
//! Keyed on the lowercased hostname string (`Url::host_str`), not the
//! registrable/eTLD+1 domain. There is no `publicsuffix`-style crate in this
//! workspace, and host-keying can only ever be *more* conservative than
//! domain-keying: `a.example.com` and `b.example.com` get independent
//! buckets rather than sharing one, so at worst this paces some hosts more
//! gently than a "true" per-organization limit would. That is the safe
//! direction to be wrong in for a rate limiter.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use governor::clock::{Clock, DefaultClock, ReasonablyRealtime};
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter as GovernorRateLimiter};
use reqwest::Url;

use crate::destination;
use crate::http::HttpSettings;

/// Governor's own keyed, async-capable rate limiter, spelled out in full.
///
/// There is no `RateLimiter::keyed_with_clock` in governor 0.10 — the only
/// constructor that accepts a non-default clock is the generic
/// `RateLimiter::new(quota, state, clock)`, which requires the middleware
/// type parameter to be named explicitly rather than defaulted. See
/// [`HostLimiter::build`].
type GovernorLimiter<C> = GovernorRateLimiter<
    String,
    DefaultKeyedStateStore<String>,
    C,
    NoOpMiddleware<<C as Clock>::Instant>,
>;

/// Ceiling on a stored cooldown deadline, applied in [`HostLimiter::
/// note_retry_after`] on top of whatever [`retry::parse_retry_after`]
/// already capped the raw header value at.
///
/// Deliberately tighter than [`retry::RETRY_AFTER_PARSE_CAP`] (120 s):
/// a cooldown blocks every *subsequent* request to the host for the rest of
/// the run, not just one document's retry attempts, so a bug or a hostile
/// server sending an enormous `Retry-After` should not be able to stall an
/// entire ingestion run against one host for minutes.
///
/// [`retry::parse_retry_after`]: super::retry::parse_retry_after
/// [`retry::RETRY_AFTER_PARSE_CAP`]: super::retry
const COOLDOWN_CAP: Duration = Duration::from_secs(60);

/// How many [`HostLimiter::acquire`] calls between housekeeping passes.
///
/// Both the cooldown `HashMap` and governor's own per-key state store grow
/// one entry per distinct host ever seen; without periodic pruning, a
/// long-running daemon process that eventually touches many different hosts
/// over its lifetime would accumulate entries for hosts it will never touch
/// again. 100 is arbitrary but cheap: pruning itself is a single lock plus a
/// `HashMap::retain` pass, negligible next to the network I/O `acquire`
/// callers are about to do anyway.
const PRUNE_EVERY_N_ACQUIRES: u64 = 100;

/// Whether `url`'s destination should be paced at all.
///
/// Loopback and private/link-local addresses are operator-owned (a `url`
/// source, or a feed's own URL, pointed at a homelab or LAN service) — there
/// is no shared-origin congestion to protect against, so pacing them only
/// adds latency for no benefit. A hostname is paced unconditionally: this
/// runs *before* DNS resolution (governor keys on the literal host string,
/// not a resolved address), so a name cannot yet be known to resolve
/// somewhere exempt, and defaulting to "pace it" is the conservative choice.
///
/// This also has a pleasant side effect for this crate's existing wiremock
/// suite: every fixture binds `127.0.0.1`, so it is exempt from pacing with
/// no test-only constructor or wiring required — `HttpUrlFetcher::fetch`
/// (a later stage) can call `acquire` unconditionally on every request
/// without any of today's 28 tests slowing down or needing to know pacing
/// exists at all.
fn should_pace(url: &Url) -> bool {
    match destination::ip_literal_host(url) {
        Some(ip) => !destination::is_blocked_destination(ip),
        None => true,
    }
}

fn nonzero_or(value: u32, fallback: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap_or_else(|| {
        NonZeroU32::new(fallback).expect("fallback for nonzero_or must itself be nonzero")
    })
}

struct Inner<C: Clock> {
    limiter: GovernorLimiter<C>,
    /// Per-host cooldown deadlines set by [`HostLimiter::note_retry_after`].
    /// Independent of `limiter`'s own clock `C` on purpose: cooldowns are
    /// wall-clock deadlines derived from a server's `Retry-After` header,
    /// which is itself a wall-clock quantity, so `std::time::Instant` is the
    /// natural unit regardless of which clock the governor token bucket
    /// (real or [`governor::clock::FakeRelativeClock`] in tests) is using.
    cooldowns: Mutex<HashMap<String, Instant>>,
    acquire_count: AtomicU64,
}

/// Paces outbound requests per destination host: a governor token bucket for
/// the sustained/burst rate, plus a cooldown map that a 429's `Retry-After`
/// can extend past what the token bucket alone would compute.
///
/// `Arc`-backed and cheaply `Clone`, matching `HttpUrlFetcher`'s existing
/// contract (`reqwest::Client` is likewise internally `Arc`-backed) — the
/// unrestricted and public-only fetchers share one `HostLimiter` instance
/// rather than each pacing independently against the same hosts.
///
/// Generic over the governor clock so tests can substitute
/// [`governor::clock::FakeRelativeClock`] and control time explicitly rather
/// than sleeping for real; production code always uses the default
/// [`HostLimiter::new`], which fixes `C` to [`DefaultClock`]. See the
/// `with_clock` / `check_ready` test-only items below for exactly how the
/// substitution works and what it does and does not let a test exercise.
pub struct HostLimiter<C: Clock = DefaultClock> {
    inner: Arc<Inner<C>>,
}

impl<C: Clock> Clone for HostLimiter<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C: Clock> HostLimiter<C> {
    fn build(cfg: &HttpSettings, clock: C) -> Self {
        let quota = Quota::per_second(nonzero_or(cfg.requests_per_second, 1))
            .allow_burst(nonzero_or(cfg.burst, 1));
        let limiter: GovernorLimiter<C> =
            GovernorRateLimiter::new(quota, DefaultKeyedStateStore::default(), clock);
        Self {
            inner: Arc::new(Inner {
                limiter,
                cooldowns: Mutex::new(HashMap::new()),
                acquire_count: AtomicU64::new(0),
            }),
        }
    }

    /// Test seam: build a `HostLimiter` on an injected clock (in practice,
    /// always [`governor::clock::FakeRelativeClock`]).
    ///
    /// This is the whole seam — no `#[cfg(test)]` branch inside production
    /// logic, just a second entry point into the same `build`. It is enough
    /// to test the token-bucket *decision* (`check_ready`, below) and the
    /// cooldown map deterministically, but deliberately not enough to call
    /// the real async [`acquire`](Self::acquire): that method is defined in
    /// a separate `impl` block bounded by `C: ReasonablyRealtime`, a bound
    /// `FakeRelativeClock` does not satisfy (only `MonotonicClock`/
    /// `QuantaClock` do — see governor's `clock` module). A test that tried
    /// to call `.acquire()` on a `HostLimiter<FakeRelativeClock>` simply
    /// fails to compile, which is the point: it forces tests that need
    /// deterministic timing onto the synchronous, non-sleeping probes
    /// instead of an approach that would need a real or virtual sleep to
    /// observe.
    #[cfg(test)]
    fn with_clock(cfg: &HttpSettings, clock: C) -> Self {
        Self::build(cfg, clock)
    }

    /// Record that `host` sent a `Retry-After` of `d`.
    ///
    /// The new deadline replaces the stored one only if it is *later* — a
    /// second, shorter `Retry-After` arriving from a different in-flight
    /// request to the same host must not shrink an already-longer cooldown.
    /// `d` is capped at [`COOLDOWN_CAP`] before comparison, so the stored
    /// value is always within that bound regardless of what the caller
    /// passes (callers are expected to have already run the header through
    /// `retry::parse_retry_after`, but this method does not trust that).
    pub fn note_retry_after(&self, host: &str, d: Duration) {
        let deadline = Instant::now() + d.min(COOLDOWN_CAP);
        let mut cooldowns = self.cooldowns_lock();
        cooldowns
            .entry(host.to_lowercase())
            .and_modify(|existing| {
                if deadline > *existing {
                    *existing = deadline;
                }
            })
            .or_insert(deadline);
    }

    fn cooldowns_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.inner
            .cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remaining wait for `host`'s cooldown, or `Duration::ZERO` if none is
    /// set or it has already elapsed. Never negative by construction
    /// (`saturating_duration_since`).
    fn cooldown_wait(&self, host: &str) -> Duration {
        let cooldowns = self.cooldowns_lock();
        cooldowns
            .get(host)
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::ZERO)
    }

    /// Bound the cooldown map and governor's own per-key state store. See
    /// [`PRUNE_EVERY_N_ACQUIRES`] for the trigger.
    fn prune(&self) {
        let now = Instant::now();
        self.cooldowns_lock().retain(|_, deadline| *deadline > now);
        self.inner.limiter.retain_recent();
    }

    /// Synchronous, non-sleeping probe: would a request to `host` be allowed
    /// through the token bucket right now?
    ///
    /// Test-only. This is deliberately how the burst/wait boundary is
    /// tested — governor's own async `until_key_ready` is what a production
    /// `acquire` call sleeps on, and sleeping (even on a fake clock that
    /// never advances on its own) is exactly what the test suite must not
    /// do. `check_key` performs the identical token-bucket accounting
    /// `until_key_ready` does, synchronously, with no sleep: `Ok` means a
    /// token was available and consumed, `Err` means none was and the loop
    /// would have waited.
    #[cfg(test)]
    fn check_ready(&self, host: &str) -> bool {
        self.inner.limiter.check_key(&host.to_lowercase()).is_ok()
    }

    #[cfg(test)]
    fn cooldown_deadline(&self, host: &str) -> Option<Instant> {
        self.cooldowns_lock().get(host).copied()
    }

    #[cfg(test)]
    fn cooldown_len(&self) -> usize {
        self.cooldowns_lock().len()
    }
}

impl HostLimiter<DefaultClock> {
    /// Build a limiter from operator-configured settings, using the real
    /// clock. This is what every non-test caller uses.
    pub fn new(cfg: &HttpSettings) -> Self {
        Self::build(cfg, DefaultClock::default())
    }
}

impl<C: Clock + ReasonablyRealtime> HostLimiter<C> {
    /// Wait until `url`'s destination host may be contacted: first for any
    /// active cooldown set by [`note_retry_after`](Self::note_retry_after),
    /// then for the token bucket. A no-op for exempt hosts — see
    /// [`should_pace`] — and for a URL with no host at all (malformed input
    /// the caller's own request-building will reject shortly afterward
    /// anyway, so there is nothing sensible to pace here).
    ///
    /// Every `PRUNE_EVERY_N_ACQUIRES`th call also runs housekeeping (see
    /// [`prune`](Self::prune)); this happens after the pacing wait, not
    /// before, so it never adds latency to the call that triggers it.
    pub async fn acquire(&self, url: &Url) {
        if !should_pace(url) {
            return;
        }
        let Some(host) = url.host_str().map(str::to_lowercase) else {
            return;
        };

        let wait = self.cooldown_wait(&host);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }

        self.inner.limiter.until_key_ready(&host).await;

        let n = self.inner.acquire_count.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(PRUNE_EVERY_N_ACQUIRES) {
            self.prune();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use governor::clock::FakeRelativeClock;

    fn settings(rps: u32, burst: u32) -> HttpSettings {
        HttpSettings {
            requests_per_second: rps,
            burst,
            ..HttpSettings::default()
        }
    }

    fn fake_limiter(rps: u32, burst: u32) -> (HostLimiter<FakeRelativeClock>, FakeRelativeClock) {
        let clock = FakeRelativeClock::default();
        let limiter = HostLimiter::with_clock(&settings(rps, burst), clock.clone());
        (limiter, clock)
    }

    // -----------------------------------------------------------------------
    // Token bucket: burst, wait, per-host independence
    // -----------------------------------------------------------------------

    #[test]
    fn burst_of_four_passes_immediately_then_the_fifth_is_blocked() {
        let (limiter, _clock) = fake_limiter(1, 4);
        for i in 0..4 {
            assert!(
                limiter.check_ready("example.com"),
                "call {i} within burst must pass"
            );
        }
        assert!(
            !limiter.check_ready("example.com"),
            "the 5th call must exceed the burst and be blocked"
        );
    }

    #[test]
    fn blocked_host_recovers_once_the_clock_advances() {
        let (limiter, clock) = fake_limiter(1, 1);
        assert!(limiter.check_ready("example.com"));
        assert!(!limiter.check_ready("example.com"));
        clock.advance(Duration::from_secs(2));
        assert!(
            limiter.check_ready("example.com"),
            "a token must have replenished after 2s at 1 req/s"
        );
    }

    #[test]
    fn two_hosts_are_independent() {
        let (limiter, _clock) = fake_limiter(1, 1);
        assert!(limiter.check_ready("a.example.com"));
        assert!(
            !limiter.check_ready("a.example.com"),
            "a.example.com must have exhausted its own bucket"
        );
        assert!(
            limiter.check_ready("b.example.com"),
            "b.example.com must have its own independent bucket"
        );
    }

    #[test]
    fn host_key_is_case_insensitive_at_the_check_ready_seam() {
        // `check_ready` itself lowercases, mirroring `acquire`'s handling of
        // `Url::host_str`; this pins that "Example.com" and "example.com"
        // share one bucket rather than being treated as different hosts.
        let (limiter, _clock) = fake_limiter(1, 1);
        assert!(limiter.check_ready("Example.com"));
        assert!(!limiter.check_ready("example.com"));
    }

    // -----------------------------------------------------------------------
    // Cooldown map
    // -----------------------------------------------------------------------

    #[test]
    fn note_retry_after_delays_only_the_named_host() {
        let (limiter, _clock) = fake_limiter(1, 1);
        limiter.note_retry_after("slow.example.com", Duration::from_secs(5));

        let delayed = limiter.cooldown_wait("slow.example.com");
        assert!(
            delayed >= Duration::from_secs(4) && delayed <= Duration::from_secs(5),
            "expected ~5s remaining, got {delayed:?}"
        );

        assert_eq!(
            limiter.cooldown_wait("other.example.com"),
            Duration::ZERO,
            "an unrelated host must not inherit the cooldown"
        );
    }

    #[test]
    fn note_retry_after_never_shrinks_an_existing_longer_cooldown() {
        let (limiter, _clock) = fake_limiter(1, 1);
        limiter.note_retry_after("host", Duration::from_secs(10));
        let after_first = limiter.cooldown_deadline("host").unwrap();

        limiter.note_retry_after("host", Duration::from_secs(2));
        let after_second = limiter.cooldown_deadline("host").unwrap();

        assert_eq!(
            after_first, after_second,
            "a shorter Retry-After must not shrink an already-longer cooldown"
        );
    }

    #[test]
    fn note_retry_after_extends_a_shorter_existing_cooldown() {
        let (limiter, _clock) = fake_limiter(1, 1);
        limiter.note_retry_after("host", Duration::from_secs(2));
        limiter.note_retry_after("host", Duration::from_secs(10));

        let wait = limiter.cooldown_wait("host");
        assert!(
            wait > Duration::from_secs(8),
            "a longer Retry-After must extend the cooldown, got {wait:?}"
        );
    }

    #[test]
    fn note_retry_after_is_capped() {
        let (limiter, _clock) = fake_limiter(1, 1);
        limiter.note_retry_after("host", Duration::from_secs(3600));
        let wait = limiter.cooldown_wait("host");
        assert!(
            wait <= COOLDOWN_CAP,
            "cooldown must be capped at {COOLDOWN_CAP:?}, got {wait:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Exemption
    // -----------------------------------------------------------------------

    #[test]
    fn loopback_and_lan_destinations_are_exempt_from_pacing() {
        for raw in [
            "http://127.0.0.1/x",
            "http://[::1]/x",
            "http://169.254.169.254/x",
            "http://10.0.0.5/x",
            "http://192.168.1.1/x",
        ] {
            let url = Url::parse(raw).expect("test URL must parse");
            assert!(!should_pace(&url), "{raw} must be exempt from pacing");
        }
    }

    #[test]
    fn globally_routable_and_named_hosts_are_paced() {
        for raw in ["http://8.8.8.8/x", "https://example.com/x"] {
            let url = Url::parse(raw).expect("test URL must parse");
            assert!(should_pace(&url), "{raw} must be paced");
        }
    }

    /// The concrete guarantee the whole exemption exists for: the real async
    /// `acquire()`, on the default (real) clock, returns effectively
    /// instantly for a loopback destination no matter how exhausted its
    /// token bucket is — proven here by giving it a bucket that could not
    /// possibly allow ten real requests within the assertion's time budget
    /// if pacing applied.
    #[tokio::test]
    async fn acquire_never_waits_for_a_loopback_destination() {
        let limiter = HostLimiter::new(&settings(1, 1));
        let url = Url::parse("http://127.0.0.1/doc").expect("test URL must parse");

        let start = std::time::Instant::now();
        for _ in 0..10 {
            limiter.acquire(&url).await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "loopback must never be paced, even across many calls"
        );
    }

    // -----------------------------------------------------------------------
    // Housekeeping
    // -----------------------------------------------------------------------

    #[test]
    fn prune_removes_expired_cooldowns_but_keeps_active_ones() {
        let (limiter, _clock) = fake_limiter(1, 1);
        limiter.note_retry_after("expired.example.com", Duration::ZERO);
        limiter.note_retry_after("active.example.com", Duration::from_secs(30));
        assert_eq!(limiter.cooldown_len(), 2);

        limiter.prune();

        assert_eq!(
            limiter.cooldown_len(),
            1,
            "the already-expired entry must have been pruned"
        );
        assert!(limiter.cooldown_deadline("active.example.com").is_some());
    }
}
