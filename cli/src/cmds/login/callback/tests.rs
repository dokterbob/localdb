use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

async fn bound_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, format!("127.0.0.1:{}", addr.port()))
}

/// A connection that never sends anything must not hang the whole exchange
/// forever — the overall deadline still has to fire.
#[tokio::test]
async fn connect_and_send_nothing_times_out_on_the_overall_deadline() {
    let (listener, addr) = bound_listener().await;
    let _silent_client = TcpStream::connect(&addr).await.unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        wait_for_callback(
            listener,
            "expected-state",
            std::time::Duration::from_millis(100),
        ),
    )
    .await
    .expect("wait_for_callback must return once its own deadline passes, not hang");

    assert!(
        result.is_err(),
        "a silent connection must not succeed the login"
    );
}

/// Nothing connecting at all must also time out via the overall deadline,
/// not the per-`accept()` call alone.
#[tokio::test]
async fn no_connection_at_all_times_out() {
    let (listener, _addr) = bound_listener().await;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        wait_for_callback(
            listener,
            "expected-state",
            std::time::Duration::from_millis(100),
        ),
    )
    .await
    .expect("wait_for_callback must return once its own deadline passes");

    assert!(result.is_err());
}

/// A bogus first connection (wrong `state`) must not decide the outcome —
/// the listener keeps accepting and a subsequent, correct callback still
/// succeeds.
#[tokio::test]
async fn a_bogus_first_connection_does_not_prevent_the_real_callback_from_succeeding() {
    let (listener, addr) = bound_listener().await;

    let waiter = tokio::spawn(wait_for_callback(
        listener,
        "expected-state",
        std::time::Duration::from_secs(5),
    ));

    let mut bogus = TcpStream::connect(&addr).await.unwrap();
    bogus
        .write_all(b"GET /callback?code=wrong&state=not-the-state HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    drop(bogus);

    // Give the listener a moment to process (and discard) the bogus
    // connection before sending the real one.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut real = TcpStream::connect(&addr).await.unwrap();
    real.write_all(b"GET /callback?code=the-real-code&state=expected-state HTTP/1.1\r\n\r\n")
        .await
        .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("wait_for_callback must return promptly once the real callback arrives")
        .expect("task must not panic");

    assert_eq!(result.unwrap(), "the-real-code");
}

/// An oversized request line (no terminator within the byte cap) must be
/// discarded rather than buffered without limit — and must not abort the
/// whole exchange, so a later well-formed callback can still succeed.
#[tokio::test]
async fn an_oversized_request_line_is_discarded_and_listening_continues() {
    let (listener, addr) = bound_listener().await;

    let waiter = tokio::spawn(wait_for_callback(
        listener,
        "expected-state",
        std::time::Duration::from_secs(5),
    ));

    let mut oversized = TcpStream::connect(&addr).await.unwrap();
    let junk = vec![b'a'; MAX_REQUEST_LINE_BYTES * 2];
    // No trailing newline: this line alone must trip the size cap.
    let _ = oversized.write_all(&junk).await;
    drop(oversized);

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut real = TcpStream::connect(&addr).await.unwrap();
    real.write_all(b"GET /callback?code=good&state=expected-state HTTP/1.1\r\n\r\n")
        .await
        .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("an oversized connection must not stall the exchange")
        .expect("task must not panic");

    assert_eq!(result.unwrap(), "good");
}

/// A callback that *does* carry the expected `state` but reports `error`
/// (e.g. the user denied consent) is terminal — it must fail immediately
/// rather than be discarded as a non-matching connection.
#[tokio::test]
async fn a_matching_callback_reporting_error_fails_immediately() {
    let (listener, addr) = bound_listener().await;

    let waiter = tokio::spawn(wait_for_callback(
        listener,
        "expected-state",
        std::time::Duration::from_secs(5),
    ));

    let mut denied = TcpStream::connect(&addr).await.unwrap();
    denied
        .write_all(b"GET /callback?error=access_denied&state=expected-state HTTP/1.1\r\n\r\n")
        .await
        .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .expect("a denied, matching callback must fail promptly, not wait out the deadline")
        .expect("task must not panic");

    let error = result.expect_err("a denied callback must be an error");
    assert!(error.to_string().contains("access_denied"), "{error}");
}
