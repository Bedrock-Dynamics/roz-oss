//! PKCE callback HTTP server (localhost) for the ChatGPT OAuth flow.
//!
//! **Source:** Lifted from `crates/roz-cli/src/commands/auth.rs:160-214`
//! (callback server) and `:257-291` (request-line parser + HTTP response
//! helper). Total ≈96 LOC of working code.
//!
//! **Why not codex-rs's `login/src/server.rs`?** Upstream's server is
//! 1238 LOC and bundles device-code + multi-transport flows that Roz does
//! not use. Our existing localhost PKCE flow is exactly what we need; lifting
//! the codex-rs version would import unwanted complexity (and divergent
//! routing) for zero benefit. This decision is part of Plan 19-05's
//! must-haves.
//!
//! # Defaults & timeout
//!
//! - Default bind: `127.0.0.1:1455` (ChatGPT OAuth registered redirect).
//! - 60-second overall timeout via `tokio::time::timeout` (T-19-05-04
//!   mitigation: prevents a hung callback from holding the thread forever).

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::AuthError;

/// Maximum number of bytes accepted on a single HTTP line (request-line or
/// header). 8 KB matches Apache / nginx defaults and is comfortably larger
/// than any legitimate OAuth callback URL while bounding memory under a
/// hostile or malformed local client (WR-02).
const MAX_LINE_BYTES: u64 = 8 * 1024;

/// Maximum number of header lines accepted before the empty terminator. A
/// well-behaved browser sends well under 50; this caps memory under a peer
/// that streams headers indefinitely without ever sending the blank line.
const MAX_HEADER_LINES: usize = 100;

/// Read a single \n-terminated line from `reader`, capped at [`MAX_LINE_BYTES`].
///
/// Returns the line as a `String` (lossy UTF-8). If the cap is reached without
/// finding a newline, returns [`AuthError::CallbackError`] so the caller can
/// abort with a 400 instead of buffering unbounded bytes (WR-02).
async fn read_capped_line<R>(reader: &mut R, label: &str) -> Result<String, AuthError>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut buf = Vec::new();
    let mut taken = reader.take(MAX_LINE_BYTES);
    let n = taken
        .read_until(b'\n', &mut buf)
        .await
        .map_err(|e| AuthError::Io(format!("read {label}: {e}")))?;
    // If we hit the cap and the last byte isn't a newline, the line is
    // oversized — refuse rather than silently truncating.
    if n as u64 == MAX_LINE_BYTES && !buf.ends_with(b"\n") {
        return Err(AuthError::CallbackError(format!(
            "{label} exceeded {MAX_LINE_BYTES}-byte cap"
        )));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Result returned by the callback server when the OAuth provider redirects
/// the browser back with `?code=...&state=...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub state: String,
}

/// Bind a one-shot HTTP listener on `bind_addr` and wait for the OAuth
/// provider to redirect the browser with a `code` + `state` query.
///
/// Validates that the returned `state` matches `expected_state`; mismatch
/// yields [`AuthError::CallbackError`] (CSRF defense, T-19-05-01).
pub async fn run_pkce_callback_server(expected_state: &str, bind_addr: &str) -> Result<CallbackResult, AuthError> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(|e| AuthError::Io(format!("bind {bind_addr}: {e}")))?;

    let (stream, _) = tokio::time::timeout(Duration::from_secs(60), listener.accept())
        .await
        .map_err(|_| AuthError::Timeout)?
        .map_err(|e| AuthError::Io(format!("accept: {e}")))?;

    let mut reader = BufReader::new(stream);
    // WR-02: cap request-line size to prevent memory exhaustion from a hostile
    // or malformed local client.
    let request_line = read_capped_line(&mut reader, "request-line").await?;

    let (code, received_state) = parse_callback_query(&request_line);

    // Drain remaining headers so the browser doesn't stall. Cap both per-line
    // size AND header count (WR-02) so a peer cannot stream headers forever.
    for _ in 0..MAX_HEADER_LINES {
        let line = read_capped_line(&mut reader, "header").await?;
        // A bare `\r\n` or empty line signals end-of-headers.
        if line.trim().is_empty() {
            break;
        }
    }

    let mut stream = reader.into_inner();

    if received_state != expected_state {
        send_http_response_async(&mut stream, 400, "CSRF state mismatch.").await;
        return Err(AuthError::CallbackError("state mismatch".into()));
    }

    if code.is_empty() {
        send_http_response_async(&mut stream, 400, "Missing authorization code.").await;
        return Err(AuthError::CallbackError("missing code".into()));
    }

    send_http_response_async(
        &mut stream,
        200,
        "<!DOCTYPE html><html><body><h1>Authorized!</h1><p>You can close this window.</p></body></html>",
    )
    .await;

    Ok(CallbackResult {
        code,
        state: received_state,
    })
}

/// Parse `code` and `state` from a single HTTP request-line of the form
/// `GET /auth/callback?code=...&state=... HTTP/1.1`.
fn parse_callback_query(request_line: &str) -> (String, String) {
    let mut code = String::new();
    let mut state = String::new();

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return (code, state);
    }

    if let Some(query_start) = parts[1].find('?') {
        let query = &parts[1][query_start + 1..];
        for param in query.split('&') {
            if let Some((key, value)) = param.split_once('=') {
                match key {
                    "code" => code = value.to_string(),
                    "state" => state = value.to_string(),
                    _ => {}
                }
            }
        }
    }

    (code, state)
}

async fn send_http_response_async(stream: &mut TcpStream, status: u16, body: &str) {
    let status_text = if status == 200 { "OK" } else { "Bad Request" };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_query_extracts_code_and_state() {
        let line = "GET /auth/callback?code=abc123&state=xyz HTTP/1.1";
        let (code, state) = parse_callback_query(line);
        assert_eq!(code, "abc123");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_callback_query_handles_extra_params() {
        let line = "GET /auth/callback?foo=1&code=abc&bar=2&state=xyz&baz=3 HTTP/1.1";
        let (code, state) = parse_callback_query(line);
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_callback_query_returns_empty_for_no_query() {
        let line = "GET / HTTP/1.1";
        let (code, state) = parse_callback_query(line);
        assert!(code.is_empty());
        assert!(state.is_empty());
    }

    #[tokio::test]
    async fn run_pkce_callback_server_rejects_state_mismatch() {
        // Bind to ephemeral port so the test does not collide with port 1455.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // release so run_pkce_callback_server can re-bind

        let server = tokio::spawn(async move { run_pkce_callback_server("expected-state", &addr.to_string()).await });

        // Briefly retry the connect because there is a tiny race between drop
        // and re-bind.
        let mut stream = None;
        for _ in 0..20 {
            if let Ok(s) = TcpStream::connect(addr).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut stream = stream.expect("connect");
        let request = "GET /auth/callback?code=abc&state=wrong-state HTTP/1.1\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let result = server.await.unwrap();
        assert!(matches!(result, Err(AuthError::CallbackError(ref m)) if m == "state mismatch"));
    }

    #[tokio::test]
    async fn run_pkce_callback_server_succeeds_on_matching_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = tokio::spawn(async move { run_pkce_callback_server("good-state", &addr.to_string()).await });

        let mut stream = None;
        for _ in 0..20 {
            if let Ok(s) = TcpStream::connect(addr).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut stream = stream.expect("connect");
        let request = "GET /auth/callback?code=auth-code-123&state=good-state HTTP/1.1\r\n\r\n";
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let result = server.await.unwrap().expect("server should succeed");
        assert_eq!(result.code, "auth-code-123");
        assert_eq!(result.state, "good-state");
    }
}
