use crate::Error;

/// Validate a feed source's `max_entries` cap.
///
/// Mirrors `config::refresh::validate_refresh_interval`'s shape: `None` means
/// unbounded (a feed source processes every entry the feed exposes) and is
/// always valid. `Some(0)` is rejected — a feed source that indexes zero
/// entries per run is almost certainly a mistake, not an intentional cap.
///
/// Returns the value unchanged (as `Ok`) so callers can use this as a
/// pass-through validation step.
pub fn validate_max_entries(max_entries: Option<u32>) -> Result<Option<u32>, Error> {
    match max_entries {
        Some(0) => Err(Error::InvalidRequest {
            message: "invalid max_entries '0': must be greater than zero, or omitted for unbounded"
                .to_string(),
        }),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_unbounded_and_valid() {
        assert_eq!(validate_max_entries(None).unwrap(), None);
    }

    #[test]
    fn positive_values_are_valid() {
        assert_eq!(validate_max_entries(Some(1)).unwrap(), Some(1));
        assert_eq!(validate_max_entries(Some(50)).unwrap(), Some(50));
        assert_eq!(
            validate_max_entries(Some(u32::MAX)).unwrap(),
            Some(u32::MAX)
        );
    }

    #[test]
    fn zero_is_rejected() {
        assert!(matches!(
            validate_max_entries(Some(0)),
            Err(Error::InvalidRequest { .. })
        ));
    }
}
