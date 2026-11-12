//! The ephemeral loopback callback listener `localdb login` opens for the
//! browser-driven OAuth2 authorization-code flow (`login.rs`'s
//! `perform_login`).
//!
//! A single deadline bounds the *whole* exchange, not just the initial
//! `accept()`: anyone on the machine can connect to this loopback port
//! before the real browser redirect arrives, so the first connection cannot
//! be trusted to be the real one. [`wait_for_callback`] keeps accepting
//! connections while handling previously accepted ones concurrently — each
//! gets its own short read deadline rather than a share of whatever time
//! remains overall, so one connection that stays open and silent cannot
//! hold up the accept loop while the real browser callback is still on its
//! way. Anything that doesn't parse, times out on its own deadline, or
//! doesn't carry the expected `state` is discarded and accepting continues;
//! the first connection whose `state` matches wins, and everything else is
//! dropped without affecting the outcome. Each read is also bounded in
//! size (not just time): a connection that never sends a request
//! terminator, or sends an oversized one, is dropped rather than buffered
//! without limit. The number of connections handled at once is bounded too,
//! so a flood of local connections can't grow the task set without limit.

use std::collections::HashMap;

use localdb_core::Error;

/// Upper bound on the request line (`GET /callback?... HTTP/1.1`): generous
/// for any real authorization-code callback, which carries only a handful
/// of short query parameters.
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;

/// Upper bound on a single header line.
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Upper bound on the total bytes spent reading headers, across however
/// many lines a connection sends.
const MAX_HEADER_TOTAL_BYTES: usize = 32 * 1024;

/// Upper bound on the number of header lines read, independent of their
/// total byte count (guards against many short lines).
const MAX_HEADER_LINES: usize = 200;

/// Per-connection deadline for reading one request, independent of how much
/// of the overall deadline is left. A real browser redirect completes in
/// well under a second; this is generous headroom for a loaded machine, not
/// an attempt to accommodate a slow client — a connection that cannot
/// finish sending its request in this window is not worth holding onto.
const PER_CONNECTION_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum number of connections read concurrently. Bounds the task set a
/// burst of local connections can create; once the limit is reached,
/// accepting pauses until an in-flight connection finishes (matches,
/// mismatches, or times out).
const MAX_IN_FLIGHT_CONNECTIONS: usize = 16;

/// Accept connections on `listener`, handling each concurrently with a
/// short deadline of its own, until one carries a `state` parameter equal
/// to `expected_state`, or the overall `deadline` elapses — whichever comes
/// first. Returns the callback's `code` parameter (possibly empty; the
/// caller rejects that). A connection whose request doesn't parse, is
/// oversized, times out, or carries no (or the wrong) `state` is discarded
/// and accepting continues with whatever time remains.
///
/// A callback that *does* match `expected_state` but carries an `error`
/// parameter (the user denied consent, or the daemon rejected the request)
/// is terminal: it is the real callback, so there is nothing to keep
/// waiting for.
pub(super) async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    deadline: std::time::Duration,
) -> Result<String, Error> {
    let deadline_at = tokio::time::Instant::now() + deadline;
    let timed_out = || Error::Unauthorized {
        message: "timed out waiting for the browser to complete login".to_string(),
    };

    let mut in_flight: tokio::task::JoinSet<Result<HashMap<String, String>, Error>> =
        tokio::task::JoinSet::new();

    loop {
        if tokio::time::Instant::now() >= deadline_at {
            return Err(timed_out());
        }

        tokio::select! {
            accepted = listener.accept(), if in_flight.len() < MAX_IN_FLIGHT_CONNECTIONS => {
                let mut stream = match accepted {
                    Ok((stream, _)) => stream,
                    // A transient accept error on this one connection attempt
                    // doesn't exhaust the deadline for the whole exchange.
                    Err(_) => continue,
                };
                let remaining = deadline_at.saturating_duration_since(tokio::time::Instant::now());
                let per_connection_deadline = PER_CONNECTION_READ_TIMEOUT.min(remaining);
                in_flight.spawn(async move {
                    match tokio::time::timeout(
                        per_connection_deadline,
                        read_callback_request(&mut stream),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(Error::InvalidRequest {
                            message: "callback connection did not send a complete request \
                                      within its read deadline"
                                .to_string(),
                        }),
                    }
                });
            }
            joined = in_flight.join_next(), if !in_flight.is_empty() => {
                let params = match joined {
                    Some(Ok(Ok(params))) => params,
                    // Malformed, oversized, too-slow, or panicked: not the
                    // real callback — discard and keep listening.
                    Some(Ok(Err(_))) | Some(Err(_)) | None => continue,
                };
                if params.get("state").map(String::as_str) != Some(expected_state) {
                    continue;
                }
                if let Some(err) = params.get("error") {
                    return Err(Error::Unauthorized {
                        message: format!("authorization failed: {err}"),
                    });
                }
                return Ok(params.get("code").cloned().unwrap_or_default());
            }
            () = tokio::time::sleep_until(deadline_at) => {
                return Err(timed_out());
            }
        }
    }
}

/// Read one HTTP request off `stream` (request line + headers, body
/// ignored — the callback carries everything in the query string), reply
/// with a minimal "you can close this window" page, and return the parsed
/// query parameters.
async fn read_callback_request(
    stream: &mut tokio::net::TcpStream,
) -> Result<HashMap<String, String>, Error> {
    use tokio::io::{AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.split();
    let mut reader = BufReader::new(read_half);

    let request_line = read_bounded_line(&mut reader, MAX_REQUEST_LINE_BYTES).await?;

    let mut header_bytes = 0usize;
    let mut header_lines = 0usize;
    loop {
        let line = read_bounded_line(&mut reader, MAX_HEADER_LINE_BYTES).await?;
        header_bytes += line.len();
        header_lines += 1;
        if header_bytes > MAX_HEADER_TOTAL_BYTES || header_lines > MAX_HEADER_LINES {
            return Err(Error::InvalidRequest {
                message: "callback request headers exceeded the size limit".to_string(),
            });
        }
        if line.is_empty() || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let path_and_query = request_line.split_whitespace().nth(1).unwrap_or("");
    let query = path_and_query.split_once('?').map(|x| x.1).unwrap_or("");
    let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();

    let body = "<html><body>Login complete &mdash; you can close this window.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = write_half.write_all(response.as_bytes()).await;
    let _ = write_half.shutdown().await;

    Ok(params)
}

/// Read a single line (through the terminating `\n`, inclusive), bounded to
/// `max_bytes`. Reads one byte at a time rather than through
/// `AsyncBufReadExt::read_line`, which keeps extending its buffer until a
/// delimiter or EOF — exactly the unbounded behavior this exists to avoid.
/// Returns an empty string on a clean EOF with no bytes read (end of a
/// well-formed header block without a final blank line) and errors once
/// `max_bytes` is exceeded before a terminator is found.
async fn read_bounded_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<String, Error> {
    use tokio::io::AsyncReadExt;

    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let n = reader.read(&mut byte).await.map_err(|e| Error::Internal {
            message: format!("callback read failed: {e}"),
            correlation_id: "login_callback_read".to_string(),
        })?;
        if n == 0 {
            break;
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= max_bytes {
            return Err(Error::InvalidRequest {
                message: "callback request line exceeded the size limit".to_string(),
            });
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

#[cfg(test)]
mod tests;
