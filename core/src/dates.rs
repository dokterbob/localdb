//! Partial-ISO-8601 normalization for `date_parsed` (specs/02-domain-model.md §2).
//!
//! [`parse_partial_iso8601`] accepts the handful of date shapes the three
//! `dc:date` producers actually emit — a bare year (PDF), a year-month (PDF),
//! a full date (PDF, EPUB), or a full RFC 3339 datetime with or without a UTC
//! offset (feed `<published>`/`<updated>`) — and normalizes each to a sortable
//! ISO 8601 string.
//!
//! The `YYYY`, `YYYY-MM`, and `YYYY-MM-DD` shapes stay hand-rolled digit
//! parsing (issue #247): they deliberately accept calendar-invalid values
//! like day 31 in a 30-day month (see "Why the date arms stay hand-rolled"
//! below), which `chrono::NaiveDate` would reject. The full-datetime shape's
//! time-and-offset tail delegates to `chrono::DateTime::parse_from_rfc3339`
//! (empirically verified identical to the hand-rolled grammar it replaced —
//! see [`validate_full_datetime_tail`]'s doc comment), spliced onto a fixed
//! valid calendar date so chrono's own calendar validation never leaks into
//! this function's deliberately-lax date handling.
//!
//! # Why the date arms stay hand-rolled
//!
//! `"YYYY-MM-DD"` day validated `01..=31`; no month-length calendar check,
//! matching `extract::pdf::parse_pdf_date`'s posture. A source that emits
//! `2026-11-31` is accepted and stored as-is today; swapping to
//! `NaiveDate::parse_from_str` would silently start rejecting input this
//! function has always accepted, which is a real behavior change this PR
//! does not make.
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
    let len = partial_date_prefix_len(s)?;
    let date_str = s[..len].to_string();
    if s.len() == len {
        return Some(date_str);
    }

    // ---- T + time + offset (full RFC 3339 datetime) ----
    // Only reachable with `len == 10`: the shorter shapes are returned above
    // because `partial_date_prefix_len` yields them only when they consume
    // the whole string.
    let bytes = s.as_bytes();
    if bytes[len] != b'T' && bytes[len] != b't' {
        return None;
    }
    if validate_full_datetime_tail(&s[len + 1..]) {
        Some(date_str)
    } else {
        None
    }
}

/// Validate the leading partial-date portion of `s`, returning the byte
/// length it occupies: 4 for `YYYY`, 7 for `YYYY-MM`, 10 for `YYYY-MM-DD`.
/// The shorter two are returned only when they span the whole string, so a
/// returned length shorter than `s` always means `len == 10` and the
/// remainder is a datetime tail for the caller to validate.
///
/// Shared by [`parse_partial_iso8601`] and [`parse_date_or_datetime`] so the
/// accepted date grammar lives in exactly one place: the two differ in what
/// they do with the tail and in what they return, never in which dates they
/// accept. Without this, a change to (say) the accepted day range would have
/// to be made twice, and silently wouldn't be.
///
/// Deliberately calendar-lax — day is range-checked `01..=31` with no
/// month-length check; see "Why the date arms stay hand-rolled" above.
///
/// Every component is fixed-width and range-checked here, so the validated
/// prefix `&s[..len]` is already in canonical zero-padded form and callers
/// can slice it directly rather than reformatting it.
fn partial_date_prefix_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();

    // ---- YYYY (mandatory) ----
    if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() == 4 {
        return Some(4);
    }

    // ---- -MM ----
    if bytes.len() < 7 || bytes[4] != b'-' {
        return None;
    }
    digits_in_range(&bytes[5..7], 1, 12)?;
    if bytes.len() == 7 {
        return Some(7);
    }

    // ---- -DD ----
    if bytes.len() < 10 || bytes[7] != b'-' {
        return None;
    }
    digits_in_range(&bytes[8..10], 1, 31)?;
    Some(10)
}

/// Parse a fixed-width numeric component: fails closed (`None`) unless every
/// byte is an ASCII digit, the parsed value fits `u32`, and it falls within
/// `[lo, hi]` inclusive. Shared by every date/time/offset component below —
/// each is the same "all-digits, then parse, then range-check" shape.
///
/// Takes bytes, not `&str`, deliberately. Every component here sits at a
/// fixed byte offset, and slicing a `&str` at one of those offsets panics
/// when the input happens to carry a multi-byte character across it
/// (`"2026-0é"` splits `é` at byte 6). Input reaches this module straight
/// from document metadata and from user-supplied filter bounds, so that has
/// to fail closed like any other malformed value, not abort the process.
/// Byte slicing cannot split a code point, and a non-ASCII byte is not an
/// ASCII digit, so such input falls out as `None` on the first check.
fn digits_in_range(bytes: &[u8], lo: u32, hi: u32) -> Option<u32> {
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Every byte is an ASCII digit, so this is valid UTF-8 by construction.
    let value: u32 = std::str::from_utf8(bytes).ok()?.parse().ok()?;
    (lo..=hi).contains(&value).then_some(value)
}

/// Validate (without extracting) the `HH:MM:SS[.fraction](Z|±HH:MM)` tail of
/// a full RFC 3339 datetime. Discarded on success — `date_parsed` only ever
/// keeps the date prefix — but validated so trailing garbage still fails the
/// whole parse rather than silently truncating it away.
///
/// Delegates to `chrono::DateTime::parse_from_rfc3339` rather than
/// hand-rolling this grammar a second time (issue #247). `rest` is spliced
/// onto the fixed, always-calendar-valid prefix `"2000-01-01T"` before
/// handing it to chrono: this function must validate *only* the tail's
/// grammar, never the (possibly calendar-invalid, and deliberately
/// unchecked — see this module's doc comment) real date prefix the caller
/// already accepted.
///
/// Empirically verified identical to the hand-rolled grammar it replaces, on
/// every point where the two could plausibly diverge (probed against chrono
/// 0.4.45 via a scratch crate; each hand-rolled verdict is unchanged):
///
/// | Input tail (on a valid date prefix) | Hand-rolled | `parse_from_rfc3339` |
/// | ------------------------------------ | ----------- | --------------------- |
/// | lowercase `t` separator¹             | accept      | accept                 |
/// | lowercase `z` offset                 | accept      | accept                 |
/// | leap second (`:60`)                  | accept      | accept                 |
/// | `-00:00` offset                      | accept      | accept                 |
/// | fractional seconds (`.123`)          | accept      | accept                 |
/// | missing offset entirely              | reject      | reject                 |
///
/// ¹ The `T`/`t` separator itself is validated by [`parse_partial_iso8601`]
/// before this function ever sees `rest` (which starts just past it), so
/// this row confirms the *whole-string* probe still matches, even though
/// this function's own splice always re-emits a literal uppercase `T`.
fn validate_full_datetime_tail(rest: &str) -> bool {
    let probe = format!("2000-01-01T{rest}");
    chrono::DateTime::parse_from_rfc3339(&probe).is_ok()
}

/// Does `s` match the canonical stored-timestamp form
/// `YYYY-MM-DDTHH:MM:SSZ` (specs/02-domain-model.md §2)?
///
/// Every stored timestamp and every value compared against one must be in
/// exactly this shape, because the comparisons are plain lexicographic string
/// comparisons. Anything else silently misorders rather than failing: chrono
/// renders a year outside `0000..=9999` with a sign prefix
/// (`+10000-01-01T00:00:00Z`, `-189627-03-09T13:49:29Z`), and both `+` (0x2B)
/// and `-` (0x2D) sort below every ASCII digit — so such a value compares as
/// an extreme against every real row rather than as the date it names.
///
/// One char class per position, so the pattern reads like the shape it
/// checks: `D` is an ASCII digit, anything else is that literal byte.
pub fn is_canonical_timestamp(s: &str) -> bool {
    const PATTERN: &[u8] = b"DDDD-DD-DDTDD:DD:DDZ";
    s.len() == PATTERN.len()
        && s.bytes().zip(PATTERN).all(|(got, want)| match want {
            b'D' => got.is_ascii_digit(),
            literal => got == *literal,
        })
}

/// Normalize a partial-or-full ISO 8601 date/datetime string for use as a
/// search-filter date bound (a later PR wires this through
/// `--added-after`/`--added-before`-style CLI flags; not called yet here —
/// this is a tested library primitive ahead of its first caller).
///
/// Unlike [`parse_partial_iso8601`], a full datetime is **not** truncated to
/// its date portion: [`parse_partial_iso8601`] exists to make `date_parsed`
/// sortable at day precision, but a user-supplied filter bound like
/// `--added-after 2026-06-10T14:30:00Z` explicitly asked for finer
/// precision, and truncating would silently discard it.
///
/// Accepted shapes (shares its grammar with [`parse_partial_iso8601`] via
/// [`digits_in_range`] and [`validate_full_datetime_tail`] rather than
/// re-deriving it):
/// - `"YYYY"` / `"YYYY-MM"` / `"YYYY-MM-DD"` → returned **unchanged**. This
///   pass-through is deliberate and load-bearing for a later PR's
///   asymmetric date-bound comparison — do not normalize partial dates to a
///   full datetime here.
/// - A full RFC 3339 datetime → returned **normalized to canonical UTC**
///   (`to_rfc3339_opts(SecondsFormat::Secs, true)` — the same
///   `YYYY-MM-DDTHH:MM:SSZ` shape every stored timestamp uses). This is a
///   correctness requirement, not a nicety: bounds are compared
///   lexicographically against canonically-stored `…Z` values, and `'+'`
///   (0x2B) sorts below every digit and below `'Z'` (0x5A) — an
///   unnormalized `2026-06-10T14:30:00+02:00` bound would compare
///   meaninglessly against stored UTC timestamps.
/// - Anything else → `None`, same fails-closed posture as
///   [`parse_partial_iso8601`].
///
/// Unlike the partial-date arms (which stay calendar-lax to match
/// [`parse_partial_iso8601`]'s posture), the full-datetime arm delegates
/// entirely to `chrono::DateTime::parse_from_rfc3339` on the real input —
/// producing a canonical UTC instant requires a real, calendar-valid
/// instant to convert, so (unlike `date_parsed`'s day-only precision) a
/// calendar-invalid date such as day 31 of a 30-day month is correctly
/// rejected here rather than silently accepted.
pub fn parse_date_or_datetime(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let len = partial_date_prefix_len(s)?;
    if s.len() == len {
        return Some(s[..len].to_string());
    }

    // ---- T + time + offset (full RFC 3339 datetime) ----
    // Unlike the partial arms above, this one hands the *real* input to
    // chrono rather than a spliced probe: producing a canonical UTC instant
    // requires a genuinely calendar-valid date, so day 31 of a 30-day month
    // is rejected here even though the partial arms accept it.
    let bytes = s.as_bytes();
    if bytes[len] != b'T' && bytes[len] != b't' {
        return None;
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    let normalized = parsed
        .with_timezone(&chrono::Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Shifting to UTC can carry a four-digit year across its own boundary —
    // `9999-12-31T23:30:00-01:00` becomes year 10000, `0000-01-01T00:30:00+01:00`
    // becomes year -1 — and chrono renders those with a sign prefix
    // (`+10000-…`, `-0001-…`). Both sort below every ordinary timestamp, so
    // returning one would hand back a bound that silently matches the wrong
    // rows rather than the date it names. Fail closed instead, like every
    // other value this function cannot represent.
    is_canonical_timestamp(&normalized).then_some(normalized)
}

/// Widen a date/datetime bound to the latest instant consistent with its own
/// precision, for use as the upper bound of a `DateAxis::Document`
/// `MetadataFilter::DateBefore` comparison (issue #247).
///
/// In fixed-width ISO 8601 a proper prefix always sorts *less than* the
/// string it prefixes (`"2026-06-10" < "2026-06-10T09:00:00Z"`), which makes
/// a plain `<=` comparison correct for `DateAfter` but backwards for
/// `DateBefore`: `--document-before 2026-06-10` would otherwise exclude every
/// timestamp later that same day. Widening the bound to the latest instant
/// its own precision could mean fixes the direction without needing real
/// calendar math.
///
/// Deliberately calendar-**unaware**: `"YYYY-MM"` always widens to day `31`
/// regardless of the real month length, because the literal string `"31"`
/// still string-compares `>=` any real two-digit day, and SQLite cannot run
/// per-row calendar math without a registered custom function.
///
/// **Keep this in lockstep with the SQL-side `CASE`** in
/// `store-libsql/src/tenant/sql.rs`'s `build_filter_clauses` — both widen the
/// exact same way, keyed on exactly the same string lengths (4 / 7 / 10). A
/// future edit here that isn't mirrored there (or vice versa) will make the
/// Rust-side (`FakeStore`) and libsql-side results disagree on the same
/// filter.
///
/// Keyed on string length, not parsing — every input `date_parsed` can hold
/// is already validated and canonical (see `parse_partial_iso8601`), so a
/// length check is sufficient and avoids a second parse:
/// - length 4 (`"YYYY"`) → append `"-12-31T23:59:59Z"`.
/// - length 7 (`"YYYY-MM"`) → append `"-31T23:59:59Z"`.
/// - length 10 (`"YYYY-MM-DD"`) → append `"T23:59:59Z"`.
/// - anything else (already a full-width datetime, or an unrecognized shape)
///   → returned unchanged.
pub fn widen_date_upper_bound(value: &str) -> String {
    match value.len() {
        4 => format!("{value}-12-31T23:59:59Z"),
        7 => format!("{value}-31T23:59:59Z"),
        10 => format!("{value}T23:59:59Z"),
        _ => value.to_string(),
    }
}
