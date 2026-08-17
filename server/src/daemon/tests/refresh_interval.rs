//! `parse_refresh_interval` tests.

use crate::daemon::parse_refresh_interval;

// --- parse_refresh_interval ---

#[test]
fn parse_refresh_interval_parses_hours() {
    assert_eq!(parse_refresh_interval("1h"), Some(3600));
    assert_eq!(parse_refresh_interval("24h"), Some(86400));
    assert_eq!(parse_refresh_interval("0h"), Some(0));
}

#[test]
fn parse_refresh_interval_parses_minutes() {
    assert_eq!(parse_refresh_interval("1m"), Some(60));
    assert_eq!(parse_refresh_interval("30m"), Some(1800));
}

#[test]
fn parse_refresh_interval_parses_seconds() {
    assert_eq!(parse_refresh_interval("3600s"), Some(3600));
    assert_eq!(parse_refresh_interval("0s"), Some(0));
}

#[test]
fn parse_refresh_interval_parses_plain_number() {
    assert_eq!(parse_refresh_interval("7200"), Some(7200));
}

#[test]
fn parse_refresh_interval_empty_returns_none() {
    assert_eq!(parse_refresh_interval(""), None);
    assert_eq!(parse_refresh_interval("   "), None);
}

#[test]
fn parse_refresh_interval_invalid_returns_none() {
    assert_eq!(parse_refresh_interval("abc"), None);
    assert_eq!(parse_refresh_interval("1x"), None);
}

/// F6: overflow guard — very large hour values must not wrap around.
/// `u64::MAX / 3600 + 1` hours would overflow; checked_mul returns None.
#[test]
fn parse_refresh_interval_overflow_returns_none() {
    // u64::MAX is 18_446_744_073_709_551_615.
    // 18_446_744_073_709_551_615 / 3600 = 5_124_095_576_030_431, remainder ≠ 0.
    // So 5_124_095_576_030_432h would overflow.
    let overflow_h = format!("{}h", u64::MAX / 3600 + 1);
    assert_eq!(
        parse_refresh_interval(&overflow_h),
        None,
        "hours overflow should return None, not wrap"
    );

    let overflow_m = format!("{}m", u64::MAX / 60 + 1);
    assert_eq!(
        parse_refresh_interval(&overflow_m),
        None,
        "minutes overflow should return None, not wrap"
    );
}
