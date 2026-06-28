use crate::Error;

/// Validate and parse a human-readable refresh interval.
///
/// Returns `Ok(Some(secs))` for a valid interval ("24h", "30m", "3600s"),
/// `Ok(None)` for an empty string, `Err(InvalidRequest)` for anything else.
pub fn validate_refresh_interval(s: &str) -> Result<Option<u64>, Error> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let secs = if let Some(h) = s.strip_suffix('h') {
        h.parse::<u64>().ok().and_then(|n| n.checked_mul(3600))
    } else if let Some(m) = s.strip_suffix('m') {
        m.parse::<u64>().ok().and_then(|n| n.checked_mul(60))
    } else if let Some(sec) = s.strip_suffix('s') {
        sec.parse::<u64>().ok()
    } else {
        s.parse::<u64>().ok()
    };
    secs.map(Some).ok_or_else(|| Error::InvalidRequest {
        message: format!(
            "invalid refresh interval '{s}': expected a duration like '24h', '30m', or '3600s'"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_hours() {
        assert_eq!(validate_refresh_interval("1h").unwrap(), Some(3600));
        assert_eq!(validate_refresh_interval("24h").unwrap(), Some(86400));
    }

    #[test]
    fn valid_minutes() {
        assert_eq!(validate_refresh_interval("30m").unwrap(), Some(1800));
    }

    #[test]
    fn valid_seconds() {
        assert_eq!(validate_refresh_interval("3600s").unwrap(), Some(3600));
    }

    #[test]
    fn valid_plain_number() {
        assert_eq!(validate_refresh_interval("7200").unwrap(), Some(7200));
    }

    #[test]
    fn empty_returns_none() {
        assert_eq!(validate_refresh_interval("").unwrap(), None);
        assert_eq!(validate_refresh_interval("   ").unwrap(), None);
    }

    #[test]
    fn invalid_returns_err() {
        assert!(matches!(
            validate_refresh_interval("badvalue"),
            Err(Error::InvalidRequest { .. })
        ));
        assert!(matches!(
            validate_refresh_interval("1x"),
            Err(Error::InvalidRequest { .. })
        ));
    }
}
