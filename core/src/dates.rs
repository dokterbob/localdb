//! Partial-ISO-8601 normalization for `date_parsed` (specs/02-domain-model.md §2).
//!
//! [`parse_partial_iso8601`] accepts the handful of date shapes the three
//! `dc:date` producers actually emit — a bare year (PDF), a year-month (PDF),
//! a full date (PDF, EPUB), or a full RFC 3339 datetime with or without a UTC
//! offset (feed `<published>`/`<updated>`) — and normalizes each to a sortable
//! ISO 8601 string. Hand-rolled digit parsing, no chrono/jiff dependency,
//! mirroring `ingest::support::format_unix_secs`'s precedent: the accepted
//! grammar is narrow and fully specified, so a general-purpose date crate buys
//! nothing here.
//!
//! # Why datetimes truncate to a date
//!
//! `date_parsed` exists purely to make document dates sortable, and nothing
//! about that axis needs finer than day precision. Keeping the time component
//! would mean either fabricating a timezone-normalization step (comparing a
//! UTC-offset datetime against a bare date is not well-defined) just to throw
//! the precision away at read time, or leaving two differently-shaped values
//! on the same column. Truncating a full datetime to its date portion up
//! front avoids both — partial-precision inputs (`"2026"`, `"2026-06"`) keep
//! their own precision as-is; only a full datetime is truncated.

#[cfg(test)]
mod tests;

/// Normalize a partial-or-full ISO 8601 date/datetime string to its sortable
/// ISO 8601 date form.
///
/// Accepted shapes (surrounding whitespace is trimmed first):
/// - `"YYYY"` → returned as-is.
/// - `"YYYY-MM"` → returned as-is (month validated `01..=12`).
/// - `"YYYY-MM-DD"` → returned as-is (day validated `01..=31`; no
///   month-length calendar check, matching `extract::pdf::parse_pdf_date`'s
///   posture).
/// - `"YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)"` → truncated to
///   `"YYYY-MM-DD"`. The time-of-day and offset are validated (not merely
///   skipped) so that garbage after a plausible date prefix is still
///   rejected, but their values are otherwise discarded.
///
/// Anything else — empty/whitespace-only input, out-of-range components, a
/// malformed separator, trailing garbage, a datetime missing its UTC offset —
/// fails closed with `None`.
pub fn parse_partial_iso8601(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();

    // ---- YYYY (mandatory) ----
    if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year = &s[..4];
    if bytes.len() == 4 {
        return Some(year.to_string());
    }

    // ---- -MM ----
    if bytes.len() < 7 || bytes[4] != b'-' || !bytes[5..7].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let month: u32 = s[5..7].parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    if bytes.len() == 7 {
        return Some(format!("{year}-{month:02}"));
    }

    // ---- -DD ----
    if bytes.len() < 10 || bytes[7] != b'-' || !bytes[8..10].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let day: u32 = s[8..10].parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let date_str = format!("{year}-{month:02}-{day:02}");
    if bytes.len() == 10 {
        return Some(date_str);
    }

    // ---- T + time + offset (full RFC 3339 datetime) ----
    if bytes[10] != b'T' && bytes[10] != b't' {
        return None;
    }
    if validate_time_and_offset(&s[11..]) {
        Some(date_str)
    } else {
        None
    }
}

/// Validate (without extracting) the `HH:MM:SS[.fraction](Z|±HH:MM)` tail of
/// a full RFC 3339 datetime. Discarded on success — `date_parsed` only ever
/// keeps the date prefix — but validated so trailing garbage still fails the
/// whole parse rather than silently truncating it away.
fn validate_time_and_offset(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    if bytes.len() < 8 {
        return false;
    }
    if !bytes[0..2].iter().all(u8::is_ascii_digit) || bytes[2] != b':' {
        return false;
    }
    if !bytes[3..5].iter().all(u8::is_ascii_digit) || bytes[5] != b':' {
        return false;
    }
    if !bytes[6..8].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let Ok(hour) = rest[0..2].parse::<u32>() else {
        return false;
    };
    let Ok(minute) = rest[3..5].parse::<u32>() else {
        return false;
    };
    // 60 tolerates a leap second; RFC 3339 permits it.
    let Ok(second) = rest[6..8].parse::<u32>() else {
        return false;
    };
    if hour > 23 || minute > 59 || second > 60 {
        return false;
    }

    let mut idx = 8;
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == frac_start {
            return false; // '.' with no fractional digits
        }
    }
    if idx >= bytes.len() {
        return false; // no offset at all — not a complete RFC 3339 datetime
    }

    match bytes[idx] {
        b'Z' | b'z' => idx + 1 == bytes.len(),
        b'+' | b'-' => {
            let off = &rest[idx + 1..];
            let ob = off.as_bytes();
            if ob.len() != 5
                || !ob[0..2].iter().all(u8::is_ascii_digit)
                || ob[2] != b':'
                || !ob[3..5].iter().all(u8::is_ascii_digit)
            {
                return false;
            }
            let Ok(off_hour) = off[0..2].parse::<u32>() else {
                return false;
            };
            let Ok(off_minute) = off[3..5].parse::<u32>() else {
                return false;
            };
            off_hour <= 23 && off_minute <= 59
        }
        _ => false,
    }
}
