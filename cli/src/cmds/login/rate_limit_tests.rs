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

// -----------------------------------------------------------------------
// `poll_loop` — the loop itself, driven by a scripted response sequence and
// a recording sleeper instead of a socket and real time. These exercise
// exactly what the pure `next_poll_step` tests above cannot: that the
// consecutive-429 counter actually carries across iterations of the real
// loop, resets after a non-429 response, and that the loop asks for and
// sleeps the number of times the decision logic implies.
// -----------------------------------------------------------------------

fn ok_body(body: serde_json::Value) -> PollResponse {
    PollResponse {
        status: reqwest::StatusCode::OK,
        retry_after: None,
        body,
    }
}

fn pending_response() -> PollResponse {
    ok_body(serde_json::json!({ "state": "pending" }))
}

fn approved_response() -> PollResponse {
    ok_body(serde_json::json!({ "state": "approved", "api_key": "ldb_minted" }))
}

fn rate_limited_response(retry_after: Option<std::time::Duration>) -> PollResponse {
    PollResponse {
        status: reqwest::StatusCode::TOO_MANY_REQUESTS,
        retry_after,
        body: serde_json::Value::Null,
    }
}

/// Run `poll_loop` against a fixed, ordered script of responses (panicking
/// if it asks for more than the script provides) and a sleeper that
/// records every wait instead of actually sleeping. Returns the loop's
/// result, how many poll requests it made, and the recorded sleep
/// durations in order.
async fn run_scripted(
    poll_interval: std::time::Duration,
    script: Vec<PollResponse>,
) -> (Result<String, Error>, usize, Vec<std::time::Duration>) {
    let script = std::cell::RefCell::new(script.into_iter());
    let calls = std::cell::Cell::new(0usize);
    let waits = std::cell::RefCell::new(Vec::new());

    let result = poll_loop(
        poll_interval,
        || {
            calls.set(calls.get() + 1);
            let resp = script
                .borrow_mut()
                .next()
                .expect("poll_loop asked for more responses than the script provides");
            async move { Ok(resp) }
        },
        |wait| {
            waits.borrow_mut().push(wait);
            async move {}
        },
    )
    .await;

    (result, calls.get(), waits.into_inner())
}

#[tokio::test]
async fn nineteen_consecutive_429s_then_pending_then_one_more_429_still_succeeds() {
    // 19 consecutive 429s never reaches the give-up threshold, so the loop
    // must still be polling; the non-429 "pending" response must reset the
    // counter, so the single 429 right after it is only the "1st"
    // consecutive one, not the "20th" — proven here by the loop succeeding
    // rather than giving up.
    let poll_interval = std::time::Duration::from_millis(50);
    let mut script = Vec::new();
    for _ in 0..19 {
        script.push(rate_limited_response(None));
    }
    script.push(pending_response());
    script.push(rate_limited_response(None));
    script.push(approved_response());

    let (result, calls, waits) = run_scripted(poll_interval, script).await;

    assert_eq!(
        result.unwrap(),
        "ldb_minted",
        "the counter reset by the pending response must keep the loop from giving up"
    );
    assert_eq!(calls, 22, "19 + 1 (pending) + 1 (429) + 1 (approved)");
    // 19 waits for the first 19 429s, 1 for the pending state, 1 for the
    // 429 right after it — all sleeps are the poll interval here since no
    // response carries a `Retry-After` header.
    assert_eq!(waits.len(), 21);
    assert!(waits.iter().all(|&w| w == poll_interval));
}

#[tokio::test]
async fn twenty_consecutive_429s_gives_up_with_exactly_twenty_requests() {
    let poll_interval = std::time::Duration::from_millis(50);
    let script: Vec<_> = (0..20).map(|_| rate_limited_response(None)).collect();

    let (result, calls, waits) = run_scripted(poll_interval, script).await;

    let error = result.unwrap_err();
    assert!(
        matches!(error, Error::RateLimited { .. }),
        "the 20th consecutive 429 must give up with Error::RateLimited: {error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "rate limited: the daemon rate-limited this poll {MAX_CONSECUTIVE_RATE_LIMITS} times \
             in a row; retry login later"
        )
    );
    assert_eq!(calls, 20, "the loop must stop asking after the 20th 429");
    // The 1st through 19th 429 each sleep before retrying; the 20th gives
    // up immediately instead of sleeping a 20th time — `next_poll_step`
    // decides GiveUp before `poll_loop` ever calls the sleep effect for it.
    assert_eq!(
        waits.len(),
        19,
        "19 sleeps precede the 20th request; the 20th itself never sleeps"
    );
}

#[tokio::test]
async fn retry_after_is_clamped_through_the_loop_not_just_in_next_poll_step() {
    let poll_interval = std::time::Duration::from_millis(50);

    let (_, _, huge) = run_scripted(
        poll_interval,
        vec![
            rate_limited_response(Some(std::time::Duration::from_secs(315_360_000))),
            approved_response(),
        ],
    )
    .await;
    assert_eq!(
        huge,
        vec![MAX_RATE_LIMIT_WAIT],
        "a huge Retry-After must be clamped down to MAX_RATE_LIMIT_WAIT before poll_loop sleeps it"
    );

    let (_, _, zero) = run_scripted(
        poll_interval,
        vec![
            rate_limited_response(Some(std::time::Duration::ZERO)),
            approved_response(),
        ],
    )
    .await;
    assert_eq!(
        zero,
        vec![poll_interval],
        "Retry-After: 0 must be clamped up to the poll interval before poll_loop sleeps it"
    );

    let (_, _, missing) = run_scripted(
        poll_interval,
        vec![rate_limited_response(None), approved_response()],
    )
    .await;
    assert_eq!(
        missing,
        vec![poll_interval],
        "a missing Retry-After must fall back to the poll interval before poll_loop sleeps it"
    );
}
