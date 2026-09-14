use super::*;

// -----------------------------------------------------------------
// recheck_floor_secs
// -----------------------------------------------------------------

#[test]
fn recheck_floor_secs_table() {
    const DAY: u64 = 24 * 60 * 60;

    let cases: &[(Option<u64>, u64)] = &[
        // Unconfigured — the common case — floors at the bare 24h minimum.
        (None, DAY),
        // A configured interval of zero must not drop below the minimum.
        (Some(0), DAY),
        // Well below the minimum: still floored at 24h.
        (Some(60), DAY),
        // Exactly the minimum: unchanged.
        (Some(DAY), DAY),
        // Above the minimum: the configured interval wins.
        (Some(7 * DAY), 7 * DAY),
        // Must not overflow converting toward the internal `i64` window —
        // the raw `u64` floor itself is unaffected by that cast, only
        // `recheck_floor_start` is.
        (Some(u64::MAX), u64::MAX),
    ];

    for (refresh_interval_secs, expected) in cases.iter().copied() {
        assert_eq!(
            recheck_floor_secs(refresh_interval_secs),
            expected,
            "recheck_floor_secs({refresh_interval_secs:?}) must be {expected}"
        );
    }
}

// -----------------------------------------------------------------
// recheck_floor_start / recheck_floor_start_rfc3339
// -----------------------------------------------------------------

#[test]
fn floor_start_subtracts_the_floor_from_now() {
    let now = DateTime::parse_from_rfc3339("2026-06-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Unconfigured: bare 24h floor.
    assert_eq!(
        recheck_floor_start(now, None),
        DateTime::parse_from_rfc3339("2026-06-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    );

    // A configured interval above the bare floor raises it: a resource
    // checked 25h ago is still within a configured 30-day floor.
    let thirty_days = 30 * 24 * 60 * 60;
    assert_eq!(
        recheck_floor_start(now, Some(thirty_days)),
        DateTime::parse_from_rfc3339("2026-05-16T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    );
}

#[test]
fn floor_start_with_u64_max_refresh_interval_saturates_to_min_utc_not_the_future() {
    // The `as i64` cast this derivation must not use would wrap a value
    // above `i64::MAX` negative, pushing the result into the future and
    // making every resource pass the floor check — the opposite of the
    // floor's purpose. The saturating derivation must instead clamp to the
    // earliest representable instant, which is trivially never in the
    // future of `now`.
    let now = Utc::now();

    assert_eq!(
        recheck_floor_start(now, Some(u64::MAX)),
        DateTime::<Utc>::MIN_UTC,
        "an overflowing refresh_interval_secs must saturate to MIN_UTC, never wrap into \
         the future"
    );
}

#[test]
fn floor_start_rfc3339_matches_the_datetime_form() {
    let now = DateTime::parse_from_rfc3339("2026-06-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    assert_eq!(
        recheck_floor_start_rfc3339(now, None),
        "2026-06-14T12:00:00Z",
        "the string form must format the same instant the DateTime form returns, in the \
         second-precision RFC 3339 shape `last_checked_at` is stored and compared in"
    );
}
