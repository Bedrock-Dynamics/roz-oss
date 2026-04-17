//! Error taxonomy for the OpenAI-compatible streaming client (Plan 19-07).
//!
//! Every boundary failure reachable from [`crate::client::OpenAiClient`] surfaces as one of
//! these variants. Variants are coarse to prevent accidental secret leakage via error messages
//! (per threat T-19-07-03 / T-19-07-04) — HTTP response bodies are truncated at the caller
//! site before being placed in [`OpenAiError::Http`], and auth tokens never appear here.

use std::time::Duration;

/// Top-level error for the OpenAI-compatible streaming client.
#[derive(thiserror::Error, Debug)]
pub enum OpenAiError {
    /// Non-2xx HTTP status from the upstream. `body` is truncated to the first 2 KB by the
    /// client before being placed here.
    #[error("http error: status={status}, body={body}")]
    Http { status: u16, body: String },

    /// SSE framing or transport failure surfaced by `eventsource-stream`.
    #[error("sse parse error: {0}")]
    Sse(String),

    /// JSON parse failure on an individual `data:` payload.
    #[error("json parse error: {0}")]
    ParseJson(String),

    /// Authentication failure surfaced by the configured [`crate::auth::AuthProvider`].
    #[error("auth error: {0}")]
    Auth(String),

    /// SSE stream went idle for longer than the configured timeout. `Duration` reports the
    /// threshold that elapsed (not the wait itself).
    #[error("idle timeout after {0:?}")]
    Timeout(Duration),

    /// Upstream returned a 5xx or reported an internal error event.
    #[error("server error: {0}")]
    ServerError(String),

    /// Failed to build the outbound request (header construction, URL join, body serialize).
    #[error("request build error: {0}")]
    RequestBuild(String),
}

impl From<serde_json::Error> for OpenAiError {
    fn from(e: serde_json::Error) -> Self {
        Self::ParseJson(e.to_string())
    }
}

impl From<reqwest::Error> for OpenAiError {
    fn from(e: reqwest::Error) -> Self {
        Self::RequestBuild(e.to_string())
    }
}

impl From<crate::auth::AuthError> for OpenAiError {
    fn from(e: crate::auth::AuthError) -> Self {
        Self::Auth(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAiError;
    use std::time::Duration;

    #[test]
    fn http_error_display_contains_status_and_body() {
        let e = OpenAiError::Http {
            status: 502,
            body: "upstream down".to_string(),
        };
        let s = format!("{e}");
        assert!(s.contains("502"));
        assert!(s.contains("upstream down"));
    }

    #[test]
    fn timeout_error_renders_duration() {
        let e = OpenAiError::Timeout(Duration::from_secs(300));
        let s = format!("{e}");
        assert!(s.contains("300"));
    }

    #[test]
    fn from_serde_json_error_maps_to_parse_json() {
        let bad: serde_json::Error = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
        let mapped: OpenAiError = bad.into();
        assert!(matches!(mapped, OpenAiError::ParseJson(_)));
    }
}
