//! Bounded retry for blocking, synchronous operations.
//!
//! Local model loading and download go through blocking calls
//! (`fastembed::TextEmbedding::try_new`, the HF file downloader) whose error
//! types don't carry a reliable "is this worth retrying" signal from here —
//! `fastembed`/`hf_hub` surface opaque `anyhow` errors, and a single dropped
//! connection during a multi-hundred-MB download looks the same as a broken
//! setup. Rather than parse error strings, a handful of unconditional
//! attempts absorbs one-off connection resets without turning a genuinely
//! broken setup into a long hang — it still fails, just a few seconds later.

use std::time::Duration;

/// Retry a fallible blocking operation up to `attempts` times total, sleeping
/// `delay(attempt)` between attempts (`attempt` is 0-indexed: `delay(0)` is
/// the sleep after the first failed attempt, `delay(1)` after the second,
/// and so on). Returns the first success, or the last error once every
/// attempt has failed. Unconditional: never inspects `E` to decide whether a
/// given failure is worth retrying.
pub(crate) fn retry_blocking<T, E>(
    attempts: usize,
    delay: impl Fn(usize) -> Duration,
    mut op: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    assert!(attempts >= 1, "attempts must be at least 1");
    let mut last_err = None;
    for attempt in 0..attempts {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) => {
                last_err = Some(err);
                if attempt + 1 < attempts {
                    std::thread::sleep(delay(attempt));
                }
            }
        }
    }
    Err(last_err.expect("loop runs at least once when attempts >= 1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A successful first attempt returns immediately and never sleeps —
    /// verified indirectly by the zero-delay closure never being consulted
    /// for a nonzero duration and the call count staying at one.
    #[test]
    fn succeeds_on_first_attempt() {
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_blocking(
            3,
            |_| Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Ok(42)
            },
        );
        assert_eq!(result, Ok(42));
        assert_eq!(calls.get(), 1, "only the first attempt should run");
    }

    /// A transient failure followed by success returns the success and
    /// stops retrying immediately.
    #[test]
    fn succeeds_after_transient_failures() {
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_blocking(
            3,
            |_| Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err("transient")
                } else {
                    Ok(7)
                }
            },
        );
        assert_eq!(result, Ok(7));
        assert_eq!(
            calls.get(),
            3,
            "should stop retrying as soon as it succeeds"
        );
    }

    /// Persistent failure exhausts the attempt budget and surfaces the last
    /// error, having tried exactly `attempts` times.
    #[test]
    fn exhausts_attempts_and_returns_last_error() {
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_blocking(
            3,
            |_| Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err("still failing")
            },
        );
        assert_eq!(result, Err("still failing"));
        assert_eq!(calls.get(), 3, "should try exactly `attempts` times");
    }

    /// `delay` receives the 0-indexed attempt number of the failure that
    /// just happened, not the attempt about to run.
    #[test]
    fn delay_indexes_by_failed_attempt() {
        let seen_attempts = std::cell::RefCell::new(Vec::new());
        let calls = Cell::new(0);
        let result: Result<i32, &str> = retry_blocking(
            3,
            |attempt| {
                seen_attempts.borrow_mut().push(attempt);
                Duration::ZERO
            },
            || {
                calls.set(calls.get() + 1);
                Err("fail")
            },
        );
        assert_eq!(result, Err("fail"));
        assert_eq!(*seen_attempts.borrow(), vec![0, 1]);
    }
}
