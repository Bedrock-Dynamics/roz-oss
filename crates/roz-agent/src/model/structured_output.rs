//! Shared structured-output repair loop (Plan 19-12).
//!
//! All three [`crate::model::Model`] providers (OpenAI-compat, Anthropic,
//! Gemini) implement the same `response_schema` parse-or-repair contract:
//!
//! 1. Try `serde_json::from_str` on the raw assistant output.
//! 2. On failure, try [`roz_core::json_repair::repair`] locally.
//! 3. If still bad, perform ONE provider-specific retry (the caller-supplied
//!    closure) and try parse + local repair on the second attempt.
//! 4. If still bad after the retry, surface
//!    [`AgentError::StructuredOutputParse`].
//!
//! The hard cap of one retry mitigates T-19-10-02 (forced-repair DoS).
//!
//! [`apply_repair_loop`] factors the loop body so each provider only needs to
//! implement its own retry callback (Anthropic re-issues a forced-tool call;
//! Gemini re-issues a `responseSchema`-tagged call; OpenAI-compat appends the
//! malformed assistant turn + a synthetic user repair instruction).

use serde_json::Value;

use crate::error::AgentError;

/// Run the parse → local-repair → one-retry → local-repair pipeline.
///
/// `raw` is the first-attempt assistant text. `schema` is the
/// `response_schema` value (used to build the synthetic repair instruction
/// passed to `retry`). `retry` is an async closure invoked at most once when
/// local repair on the first attempt fails; it receives the original raw text
/// plus the synthetic repair instruction and returns the second-attempt raw
/// assistant text.
///
/// Returns a parsed `Value` on success at any stage. Returns
/// [`AgentError::StructuredOutputParse`] if both the first attempt + local
/// repair AND the second attempt + local repair fail.
///
/// # Errors
///
/// - [`AgentError::StructuredOutputParse`] — both attempts produced
///   un-parseable, un-repairable JSON.
/// - Whatever [`AgentError`] the `retry` closure returns is propagated as-is.
pub async fn apply_repair_loop<F, Fut>(raw: String, schema: &Value, retry: F) -> Result<Value, AgentError>
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<String, AgentError>>,
{
    // 1. Already valid?
    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
        return Ok(v);
    }
    // 2. Local repair on first attempt.
    if let Ok(repaired) = roz_core::json_repair::repair(&raw)
        && let Ok(v) = serde_json::from_str::<Value>(&repaired)
    {
        return Ok(v);
    }
    // 3. Build synthetic repair instruction and invoke the caller's retry.
    let instruction = format!(
        "The previous response was not valid JSON. Return ONLY JSON matching the provided schema. \
         Do not include markdown fences or commentary. Schema: {schema}"
    );
    let second = retry(raw, instruction).await?;
    // 4. Try parse + local repair on second attempt.
    if let Ok(v) = serde_json::from_str::<Value>(&second) {
        return Ok(v);
    }
    if let Ok(repaired) = roz_core::json_repair::repair(&second)
        && let Ok(v) = serde_json::from_str::<Value>(&repaired)
    {
        return Ok(v);
    }
    let parse_err = serde_json::from_str::<Value>(&second)
        .err()
        .map_or_else(|| "malformed after 1 repair retry".to_string(), |e| e.to_string());
    Err(AgentError::StructuredOutputParse {
        raw: truncate_at_char_boundary(second, RAW_CAP),
        err: parse_err,
    })
}

/// Upper bound on `AgentError::StructuredOutputParse::raw` size (WR-03).
///
/// Matches the 2 KB cap [`crate::model::openai`] applies to HTTP error bodies,
/// so malformed-JSON payloads can't blow up error chains / logs when a provider
/// returns a huge un-parseable response.
const RAW_CAP: usize = 2 * 1024;

/// Truncate `s` to at most `cap` bytes, ending on a UTF-8 char boundary, and
/// append a `…[truncated N bytes]` suffix when truncation occurs.
fn truncate_at_char_boundary(s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    // Walk backwards from `cap` to find a char boundary.
    let mut boundary = cap;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let dropped = s.len() - boundary;
    let mut out = String::with_capacity(boundary + 32);
    out.push_str(&s[..boundary]);
    out.push_str(&format!("…[truncated {dropped} bytes]"));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type":"object"})
    }

    #[tokio::test]
    async fn repair_loop_ok_on_valid_json_no_retry() {
        let s = schema();
        let v = apply_repair_loop(r#"{"answer":"42"}"#.to_string(), &s, |_, _| async {
            panic!("retry must not be invoked when first attempt is valid");
        })
        .await
        .expect("should succeed");
        assert_eq!(v["answer"], "42");
    }

    #[tokio::test]
    async fn repair_loop_ok_after_trailing_comma_repair_no_retry() {
        let s = schema();
        let v = apply_repair_loop(r#"{"answer":"42",}"#.to_string(), &s, |_, _| async {
            panic!("retry must not be invoked when local repair succeeds");
        })
        .await
        .expect("should succeed via local repair");
        assert_eq!(v["answer"], "42");
    }

    #[tokio::test]
    async fn repair_loop_calls_retry_on_unrepairable_then_success() {
        let s = schema();
        let v = apply_repair_loop(
            "garbage cannot be repaired locally".to_string(),
            &s,
            |raw, instr| async move {
                assert!(raw.contains("garbage"));
                assert!(instr.contains("Return ONLY JSON"));
                assert!(instr.contains(r#""type":"object""#));
                Ok(r#"{"answer":"42"}"#.to_string())
            },
        )
        .await
        .expect("retry should produce valid JSON");
        assert_eq!(v["answer"], "42");
    }

    #[tokio::test]
    async fn repair_loop_repairs_second_attempt_before_giving_up() {
        let s = schema();
        let v = apply_repair_loop("garbage cannot be repaired locally".to_string(), &s, |_, _| async {
            // Second attempt has a trailing comma — local repair should fix it.
            Ok(r#"{"answer":"42",}"#.to_string())
        })
        .await
        .expect("local repair should succeed on second attempt");
        assert_eq!(v["answer"], "42");
    }

    #[tokio::test]
    async fn repair_loop_surfaces_structured_output_parse_after_retry() {
        let s = schema();
        let err = apply_repair_loop("garbage".to_string(), &s, |_, _| async {
            Ok("still garbage".to_string())
        })
        .await
        .expect_err("should fail");
        match err {
            AgentError::StructuredOutputParse { raw, err } => {
                assert_eq!(raw, "still garbage");
                assert!(!err.is_empty());
            }
            other => panic!("expected StructuredOutputParse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repair_loop_propagates_retry_error() {
        let s = schema();
        let err = apply_repair_loop("garbage".to_string(), &s, |_, _| async {
            Err(AgentError::Model("upstream exploded".into()))
        })
        .await
        .expect_err("retry error should propagate");
        assert!(matches!(err, AgentError::Model(_)));
    }
}
