use super::{parse_date_or_datetime, parse_partial_iso8601};

#[test]
fn bare_year() {
    assert_eq!(parse_partial_iso8601("2026"), Some("2026".to_string()));
}

#[test]
fn year_month() {
    assert_eq!(
        parse_partial_iso8601("2026-06"),
        Some("2026-06".to_string())
    );
}

#[test]
fn full_date() {
    assert_eq!(
        parse_partial_iso8601("2026-06-15"),
        Some("2026-06-15".to_string())
    );
}

#[test]
fn full_rfc3339_datetime_with_z() {
    assert_eq!(
        parse_partial_iso8601("2026-06-15T10:30:00Z"),
        Some("2026-06-15".to_string())
    );
}

#[test]
fn full_rfc3339_datetime_with_offset() {
    assert_eq!(
        parse_partial_iso8601("2026-06-15T10:30:00+02:00"),
        Some("2026-06-15".to_string())
    );
}

#[test]
fn rejects_garbage() {
    assert_eq!(parse_partial_iso8601("not-a-date"), None);
    assert_eq!(parse_partial_iso8601("2026-06-15T"), None);
    assert_eq!(parse_partial_iso8601("2026-06-15T10:30:00"), None); // no offset
    assert_eq!(parse_partial_iso8601("2026-06-15X10:30:00Z"), None); // bad separator
}

#[test]
fn rejects_empty() {
    assert_eq!(parse_partial_iso8601(""), None);
    assert_eq!(parse_partial_iso8601("   "), None);
}

#[test]
fn rejects_month_13() {
    assert_eq!(parse_partial_iso8601("2026-13"), None);
    assert_eq!(parse_partial_iso8601("2026-13-01"), None);
}

#[test]
fn rejects_day_00() {
    assert_eq!(parse_partial_iso8601("2026-06-00"), None);
}

// ---------------------------------------------------------------------------
// Real-world shapes emitted by the three `dc:date` producers.
// ---------------------------------------------------------------------------

/// `extract::pdf::parse_pdf_date` returns a bare "YYYY" for a PDF Info/XMP
/// date that only carries a year (e.g. `D:2026`).
#[test]
fn pdf_year_only_shape() {
    assert_eq!(parse_partial_iso8601("2026"), Some("2026".to_string()));
}

/// `extract::pdf::parse_pdf_date` returns "YYYY-MM" for a PDF date that
/// carries year and month but no day (e.g. `D:202606`).
#[test]
fn pdf_year_month_shape() {
    assert_eq!(
        parse_partial_iso8601("2026-06"),
        Some("2026-06".to_string())
    );
}

/// `extract::pdf::parse_pdf_date` always truncates to "YYYY-MM-DD" even when
/// the source `D:...` string carries a time component (see `pdf.rs`'s
/// `date_str` — HH/mm/ss are validated but never appended).
#[test]
fn pdf_full_date_shape() {
    assert_eq!(
        parse_partial_iso8601("2026-06-15"),
        Some("2026-06-15".to_string())
    );
}

/// `extract::parsers::epub::map_metadata` sets `dc.date` from rbook's
/// `DateTime::date().to_string()`, whose `Display` impl always zero-pads to
/// "YYYY-MM-DD" (rbook defaults an absent month/day to `01`, so EPUB never
/// actually emits a bare year or year-month — verified against
/// `rbook::ebook::metadata::datetime::Date`'s `Display` impl).
#[test]
fn epub_date_shape() {
    assert_eq!(
        parse_partial_iso8601("2023-01-25"),
        Some("2023-01-25".to_string())
    );
}

/// `ingest::feed_ingestor` sets `dc.date` from `chrono::DateTime::
/// to_rfc3339()`, which renders a numeric `+00:00` offset rather than a `Z`
/// literal (confirmed by `feed_ingestor::tests::
/// modified_at_prefers_updated_while_dc_date_prefers_published`, which
/// asserts the exact string `"2026-01-04T00:00:00+00:00"`).
#[test]
fn feed_rfc3339_shape() {
    assert_eq!(
        parse_partial_iso8601("2026-01-04T00:00:00+00:00"),
        Some("2026-01-04".to_string())
    );
}

// ---------------------------------------------------------------------------
// parse_date_or_datetime (issue #247) — search-filter date bound primitive.
// Not wired to any caller yet; a later PR wires it through CLI date-filter
// flags. Partial dates pass through unchanged (asymmetric filter-bound
// comparison needs that); a full datetime normalizes to canonical UTC.
// ---------------------------------------------------------------------------

#[test]
fn partial_date_shapes_pass_through_unchanged() {
    assert_eq!(parse_date_or_datetime("2026"), Some("2026".to_string()));
    assert_eq!(
        parse_date_or_datetime("2026-06"),
        Some("2026-06".to_string())
    );
    assert_eq!(
        parse_date_or_datetime("2026-06-15"),
        Some("2026-06-15".to_string())
    );
}

/// The load-bearing case: a non-UTC offset must come back UTC-shifted and in
/// canonical `Z` form, not returned unchanged. `+02:00` subtracts two hours
/// from the clock time to land on the same instant in UTC.
#[test]
fn full_datetime_with_non_utc_offset_normalizes_to_utc_canonical_form() {
    assert_eq!(
        parse_date_or_datetime("2026-06-15T14:30:00+02:00"),
        Some("2026-06-15T12:30:00Z".to_string())
    );
}

/// Fractional seconds are accepted on input but the canonical output never
/// carries them (`SecondsFormat::Secs`), matching the stored-timestamp
/// contract every other canonical-form producer in this codebase follows.
#[test]
fn full_datetime_with_fractional_seconds_loses_the_fraction() {
    assert_eq!(
        parse_date_or_datetime("2026-06-15T10:30:00.123456Z"),
        Some("2026-06-15T10:30:00Z".to_string())
    );
}

#[test]
fn full_datetime_with_z_normalizes_unchanged_in_value() {
    assert_eq!(
        parse_date_or_datetime("2026-06-15T10:30:00Z"),
        Some("2026-06-15T10:30:00Z".to_string())
    );
}

#[test]
fn full_datetime_lowercase_t_and_z_accepted() {
    assert_eq!(
        parse_date_or_datetime("2026-06-15t10:30:00z"),
        Some("2026-06-15T10:30:00Z".to_string())
    );
}

/// Unlike `parse_partial_iso8601`'s `YYYY-MM-DD` arm (deliberately
/// calendar-lax), the full-datetime arm here needs a real instant to
/// normalize to UTC, so a calendar-invalid date is rejected rather than
/// silently accepted.
#[test]
fn full_datetime_with_calendar_invalid_date_is_rejected() {
    assert_eq!(parse_date_or_datetime("2026-11-31T10:00:00Z"), None);
}

#[test]
fn rejects_datetime_missing_offset() {
    assert_eq!(parse_date_or_datetime("2026-06-15T10:30:00"), None);
}

#[test]
fn rejects_garbage_and_empty() {
    assert_eq!(parse_date_or_datetime(""), None);
    assert_eq!(parse_date_or_datetime("   "), None);
    assert_eq!(parse_date_or_datetime("not-a-date"), None);
    assert_eq!(parse_date_or_datetime("2026-13"), None);
    assert_eq!(parse_date_or_datetime("2026-06-00"), None);
}
