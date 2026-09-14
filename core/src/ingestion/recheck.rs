//! The recheck floor shared by the feed liveness sweep's candidate cutoff
//! and the feed entry recheck gate (specs/04-search-pipeline.md §1 "Recheck
//! gate" → "Floor"; "Aged-out feed entries: the liveness sweep" →
//! "Candidates").
//!
//! Both mechanisms ask the same question — "has it been long enough since
//! `last_checked_at` to check this resource's link again?" — against the
//! same column, so both derive the answer from these two functions rather
//! than each computing it independently. A source's own
//! `refresh_interval_secs`, when configured, only ever *raises* the floor
//! above the bare minimum: dropping the minimum whenever a shorter interval
//! is configured would mean a `refresh: 15m` feed rechecks every entry every
//! 15 minutes — zero savings in exactly the configuration this floor exists
//! to protect.

use chrono::{DateTime, SecondsFormat, Utc};

/// Minimum recheck interval, in seconds: a resource is never re-probed more
/// often than this, however long it has gone unchecked, however short a
/// source's own `refresh_interval_secs` is configured. See the module doc
/// comment above for why the minimum holds even under a short interval.
pub(crate) const FEED_LIVENESS_MIN_RECHECK_SECS: i64 = 24 * 60 * 60;

/// The recheck floor for `refresh_interval_secs`, in seconds:
/// `max(refresh_interval_secs, FEED_LIVENESS_MIN_RECHECK_SECS)`. An
/// unconfigured interval (`None`) — the common case — or one below the bare
/// minimum leaves the floor at the bare minimum; a longer configured
/// interval raises the floor to match it.
pub(crate) fn recheck_floor_secs(refresh_interval_secs: Option<u64>) -> u64 {
    refresh_interval_secs
        .unwrap_or(0)
        .max(FEED_LIVENESS_MIN_RECHECK_SECS as u64)
}

/// The instant before which a `last_checked_at` counts as stale under
/// [`recheck_floor_secs`]: `now` minus the floor, saturating rather than
/// panicking or wrapping at the extremes.
///
/// `refresh_interval_secs` is an unvalidated `u64` from config (no upper
/// bound is enforced in `core::config::refresh::validate_refresh_interval`),
/// so it must not be cast with `as i64`: a value above `i64::MAX` wraps
/// negative, which would push the result into the future and make every
/// resource pass the floor check — the opposite of what the floor is for.
/// Saturate every step rather than only the cast: `chrono::Duration::seconds`
/// itself panics above `i64::MAX / 1_000`, and subtracting from `now` can in
/// principle underflow past the representable range.
pub(crate) fn recheck_floor_start(
    now: DateTime<Utc>,
    refresh_interval_secs: Option<u64>,
) -> DateTime<Utc> {
    let floor_secs = recheck_floor_secs(refresh_interval_secs);
    let floor_secs_i64 = i64::try_from(floor_secs).unwrap_or(i64::MAX);
    let recheck_window =
        chrono::Duration::try_seconds(floor_secs_i64).unwrap_or(chrono::Duration::MAX);
    now.checked_sub_signed(recheck_window)
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// [`recheck_floor_start`], formatted the way `last_checked_at` is stored
/// and compared: RFC 3339, second precision, explicit `Z`. Both the sweep's
/// SQL cutoff and the gate's string comparison against the stored column
/// need this exact format to line up with it.
pub(crate) fn recheck_floor_start_rfc3339(
    now: DateTime<Utc>,
    refresh_interval_secs: Option<u64>,
) -> String {
    recheck_floor_start(now, refresh_interval_secs).to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests;
