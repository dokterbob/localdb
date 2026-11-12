//! Table-driven unit tests for `next_poll_step` and `retry_after_duration`
//! — the pure decision logic `poll_until_decided` uses to handle a `429`
//! while polling for invite approval. These run with no socket and no
//! clock, so they are deterministic regardless of machine load or how busy
//! the test binary's runtime is.

use super::*;

#[test]
fn retry_after_duration_parses_a_plain_integer_header() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        reqwest::header::HeaderValue::from_static("7"),
    );
    assert_eq!(
        retry_after_duration(&headers),
        Some(std::time::Duration::from_secs(7))
    );
}

#[test]
fn retry_after_duration_is_none_without_the_header() {
    let headers = reqwest::header::HeaderMap::new();
    assert_eq!(retry_after_duration(&headers), None);
}

#[test]
fn retry_after_duration_is_none_for_an_unparsable_value() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2099 07:28:00 GMT"),
    );
    assert_eq!(retry_after_duration(&headers), None);
}

#[test]
fn a_huge_retry_after_is_clamped_to_the_maximum_wait() {
    let poll_interval = std::time::Duration::from_millis(100);
    let step = next_poll_step(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        Some(std::time::Duration::from_secs(315_360_000)),
        poll_interval,
        0,
    );
    assert_eq!(
        step,
        PollStep::WaitAndRetry {
            wait: MAX_RATE_LIMIT_WAIT,
            new_count: 1,
        },
        "a Retry-After far beyond MAX_RATE_LIMIT_WAIT must be clamped down to it, not honored"
    );
}

#[test]
fn a_zero_retry_after_is_raised_to_the_poll_interval() {
    let poll_interval = std::time::Duration::from_millis(100);
    let step = next_poll_step(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        Some(std::time::Duration::ZERO),
        poll_interval,
        0,
    );
    assert_eq!(
        step,
        PollStep::WaitAndRetry {
            wait: poll_interval,
            new_count: 1,
        },
        "Retry-After: 0 must be clamped up to the poll interval, not hot-loop"
    );
}

#[test]
fn a_missing_retry_after_falls_back_to_the_poll_interval() {
    let poll_interval = std::time::Duration::from_millis(100);
    let step = next_poll_step(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        None,
        poll_interval,
        0,
    );
    assert_eq!(
        step,
        PollStep::WaitAndRetry {
            wait: poll_interval,
            new_count: 1,
        },
        "a missing Retry-After must fall back to the poll interval"
    );
}

#[test]
fn an_unparsable_retry_after_falls_back_to_the_poll_interval() {
    // `retry_after_duration` already turns an unparsable header into `None`
    // (covered above); from `next_poll_step`'s side that is indistinguishable
    // from a missing header, so it takes the same fallback path.
    let poll_interval = std::time::Duration::from_millis(100);
    let step = next_poll_step(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        None,
        poll_interval,
        0,
    );
    assert_eq!(
        step,
        PollStep::WaitAndRetry {
            wait: poll_interval,
            new_count: 1,
        }
    );
}

#[test]
fn the_nth_consecutive_429_waits_while_below_the_threshold() {
    let poll_interval = std::time::Duration::from_millis(100);
    let step = next_poll_step(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        None,
        poll_interval,
        MAX_CONSECUTIVE_RATE_LIMITS - 2,
    );
    assert_eq!(
        step,
        PollStep::WaitAndRetry {
            wait: poll_interval,
            new_count: MAX_CONSECUTIVE_RATE_LIMITS - 1,
        },
        "the (MAX_CONSECUTIVE_RATE_LIMITS - 1)th consecutive 429 must still wait and retry"
    );
}

#[test]
fn the_max_consecutive_429_gives_up() {
    let poll_interval = std::time::Duration::from_millis(100);
    let step = next_poll_step(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        None,
        poll_interval,
        MAX_CONSECUTIVE_RATE_LIMITS - 1,
    );
    assert_eq!(
        step,
        PollStep::GiveUp,
        "the MAX_CONSECUTIVE_RATE_LIMITSth consecutive 429 must give up, not wait again"
    );
}

#[test]
fn a_non_429_response_proceeds_regardless_of_the_prior_streak() {
    let poll_interval = std::time::Duration::from_millis(100);
    for status in [
        reqwest::StatusCode::OK,
        reqwest::StatusCode::NOT_FOUND,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let step = next_poll_step(status, None, poll_interval, MAX_CONSECUTIVE_RATE_LIMITS - 1);
        assert_eq!(
            step,
            PollStep::Proceed,
            "a non-429 status ({status}) must proceed to interpret the body, even after a long \
             429 streak — the caller resets its counter on Proceed"
        );
    }
}
