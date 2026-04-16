//! JSON repair helper for structured-output recovery (Plan 19-10).
//!
//! LLMs frequently emit nearly-valid JSON with a small set of recoverable
//! defects: markdown code fences, leading/trailing prose, trailing commas,
//! unclosed braces/brackets. This module offers a best-effort [`repair`] that
//! applies a fixed pipeline of string-scan passes and validates the result via
//! `serde_json`.
//!
//! # Out of scope
//!
//! Explicitly NOT handled (would require a real parser):
//! - Duplicate-key resolution.
//! - Unicode escape fixing (`\u` without 4 hex digits, surrogate-pair repair).
//! - Type coercion (e.g. quoted numbers → numbers).
//! - String-literal repair (unescaped quotes, unescaped newlines inside strings).
//!
//! If the pipeline completes but `serde_json` still rejects the result, the
//! helper surfaces [`JsonRepairError::Unrepairable`] with the serde error.

/// Failure to repair a malformed JSON document.
#[derive(thiserror::Error, Debug)]
pub enum JsonRepairError {
    /// The repair pipeline did not produce a `serde_json`-parseable document.
    #[error("repair failed: {0}")]
    Unrepairable(String),
}

/// Strip a leading/trailing markdown code fence, including an optional
/// `json` / `JSON` language tag on the opener.
///
/// ```text
/// ```json
/// {"a":1}
/// ```
/// ```
///
/// becomes `{"a":1}`. If `input` does not begin with a triple-backtick, the
/// input is returned unchanged.
#[must_use]
pub fn strip_code_fence(input: &str) -> &str {
    let trimmed = input.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return input;
    };

    // Drop optional language tag (everything up to the first newline).
    let body = after_open.split_once('\n').map_or(after_open, |(_tag, rest)| rest);

    // Strip matching trailing fence.
    body.trim_end().strip_suffix("```").map_or(body, str::trim).trim()
}

/// Trim prose that precedes the first `{`/`[` and follows the matching
/// closing `}`/`]` at the END of the document.
///
/// Used as a coarse "find the JSON document" heuristic when the model wraps
/// the JSON in explanatory prose. Safe to apply after `strip_code_fence`.
#[must_use]
pub fn trim_leading_trailing_prose(input: &str) -> &str {
    let start_obj = input.find('{');
    let start_arr = input.find('[');
    let start = match (start_obj, start_arr) {
        (Some(o), Some(a)) => o.min(a),
        (Some(o), None) => o,
        (None, Some(a)) => a,
        (None, None) => return input,
    };

    // Match the outer bracket type.
    let open = input.as_bytes()[start];
    let close = if open == b'{' { b'}' } else { b']' };

    let end = input.as_bytes().iter().rposition(|&b| b == close);
    let Some(end) = end else {
        return &input[start..];
    };
    if end < start {
        return &input[start..];
    }
    &input[start..=end]
}

/// Remove trailing commas before `}` or `]` that are OUTSIDE string literals.
///
/// Correctly handles embedded commas inside string values (e.g.
/// `{"a":"x,}"}` is left untouched). Uses a single-pass state machine that
/// tracks `in_string` + backslash-escape.
#[must_use]
pub fn fix_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b',' {
            // Peek ahead through whitespace; drop if followed by `}` or `]`.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                i += 1; // Skip the comma, preserve trailing whitespace on next iter.
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    // Safety: all written bytes are either from the UTF-8 input (which we
    // copy as bytes) or plain ASCII punctuation.
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

/// Append missing closing `}` / `]` at the end of `input` to balance openers.
///
/// Returns [`JsonRepairError::Unrepairable`] if the bracket stack underflows
/// (i.e. more closers than openers, which cannot be fixed by appending).
///
/// String contents are tracked so that `{` inside a string does not increment
/// the stack.
///
/// # Errors
///
/// Returns [`JsonRepairError::Unrepairable`] when the bracket stack cannot be
/// reconciled (e.g. extra closing brace).
pub fn close_dangling_braces(input: &str) -> Result<String, JsonRepairError> {
    let bytes = input.as_bytes();
    let mut stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for &c in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                let expected = stack.pop();
                if expected != Some(c) {
                    return Err(JsonRepairError::Unrepairable(format!(
                        "mismatched bracket: found {} with expected {:?}",
                        c as char, expected
                    )));
                }
            }
            _ => {}
        }
    }
    let mut out = input.to_string();
    if in_string {
        // Close the dangling string literal first.
        out.push('"');
    }
    while let Some(closer) = stack.pop() {
        out.push(closer as char);
    }
    Ok(out)
}

/// Attempt to repair a malformed JSON document emitted by an LLM.
///
/// Pipeline: [`strip_code_fence`] → [`trim_leading_trailing_prose`] →
/// [`fix_trailing_commas`] → [`close_dangling_braces`]. After each step the
/// helper attempts `serde_json::from_str::<Value>` and short-circuits on the
/// first success. If `input` is already valid JSON the helper returns it
/// essentially unchanged (after code-fence + prose trimming).
///
/// # Errors
///
/// Returns [`JsonRepairError::Unrepairable`] when the full pipeline still
/// yields a document `serde_json` rejects.
pub fn repair(input: &str) -> Result<String, JsonRepairError> {
    // Fast path — valid as-is.
    if serde_json::from_str::<serde_json::Value>(input).is_ok() {
        return Ok(input.to_string());
    }

    // Step 1: code fence.
    let s1 = strip_code_fence(input);
    if serde_json::from_str::<serde_json::Value>(s1).is_ok() {
        return Ok(s1.to_string());
    }

    // Step 2: prose trim.
    let s2 = trim_leading_trailing_prose(s1);
    if serde_json::from_str::<serde_json::Value>(s2).is_ok() {
        return Ok(s2.to_string());
    }

    // Step 3: trailing commas.
    let s3 = fix_trailing_commas(s2);
    if serde_json::from_str::<serde_json::Value>(&s3).is_ok() {
        return Ok(s3);
    }

    // Step 4: dangling braces.
    let s4 = close_dangling_braces(&s3)?;
    match serde_json::from_str::<serde_json::Value>(&s4) {
        Ok(_) => Ok(s4),
        Err(e) => Err(JsonRepairError::Unrepairable(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // strip_code_fence
    // -----------------------------------------------------------------------

    #[test]
    fn strip_code_fence_with_json_tag() {
        let s = "```json\n{\"a\":1}\n```";
        assert_eq!(strip_code_fence(s), "{\"a\":1}");
    }

    #[test]
    fn strip_code_fence_without_tag() {
        let s = "```\n{\"a\":1}\n```";
        assert_eq!(strip_code_fence(s), "{\"a\":1}");
    }

    #[test]
    fn strip_code_fence_multiline_body() {
        let s = "```json\n{\n  \"a\": 1,\n  \"b\": 2\n}\n```";
        let out = strip_code_fence(s);
        assert!(out.contains("\"a\": 1"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn strip_code_fence_noop_when_absent() {
        let s = "{\"a\":1}";
        assert_eq!(strip_code_fence(s), s);
    }

    // -----------------------------------------------------------------------
    // trim_leading_trailing_prose
    // -----------------------------------------------------------------------

    #[test]
    fn trim_prose_around_object() {
        let s = "Here's the JSON:\n{\"a\":1}\nThat's it.";
        assert_eq!(trim_leading_trailing_prose(s), "{\"a\":1}");
    }

    #[test]
    fn trim_prose_around_array() {
        let s = "Output: [1,2,3]. Done.";
        assert_eq!(trim_leading_trailing_prose(s), "[1,2,3]");
    }

    #[test]
    fn trim_prose_noop_when_pure() {
        let s = "{\"a\":1}";
        assert_eq!(trim_leading_trailing_prose(s), s);
    }

    // -----------------------------------------------------------------------
    // fix_trailing_commas
    // -----------------------------------------------------------------------

    #[test]
    fn fix_trailing_comma_object() {
        assert_eq!(fix_trailing_commas("{\"a\":1,}"), "{\"a\":1}");
    }

    #[test]
    fn fix_trailing_comma_array() {
        assert_eq!(fix_trailing_commas("[1,2,]"), "[1,2]");
    }

    #[test]
    fn fix_trailing_comma_nested() {
        assert_eq!(fix_trailing_commas("{\"a\":[1,2,],}"), "{\"a\":[1,2]}");
    }

    #[test]
    fn fix_trailing_comma_preserves_string_contents() {
        // Comma inside a string literal MUST be preserved.
        let s = r#"{"a":"x,}","b":2}"#;
        assert_eq!(fix_trailing_commas(s), s);
    }

    #[test]
    fn fix_trailing_comma_with_escaped_quote_in_string() {
        let s = r#"{"a":"x\",","b":2}"#;
        assert_eq!(fix_trailing_commas(s), s);
    }

    // -----------------------------------------------------------------------
    // close_dangling_braces
    // -----------------------------------------------------------------------

    #[test]
    fn close_single_missing_brace() {
        assert_eq!(close_dangling_braces("{\"a\":{\"b\":1").unwrap(), "{\"a\":{\"b\":1}}");
    }

    #[test]
    fn close_multiple_missing_braces() {
        assert_eq!(close_dangling_braces("{\"a\":[1,2,3").unwrap(), "{\"a\":[1,2,3]}");
    }

    #[test]
    fn close_mismatched_bracket_is_unrepairable() {
        let err = close_dangling_braces("{\"a\":1]").unwrap_err();
        let JsonRepairError::Unrepairable(msg) = err;
        assert!(msg.contains("mismatched"));
    }

    #[test]
    fn close_ignores_braces_inside_strings() {
        // Opener-looking char inside a string must NOT increment the stack.
        assert_eq!(
            close_dangling_braces(r#"{"a":"{\"nested\"}"}"#).unwrap(),
            r#"{"a":"{\"nested\"}"}"#
        );
    }

    // -----------------------------------------------------------------------
    // repair (end-to-end)
    // -----------------------------------------------------------------------

    #[test]
    fn repair_valid_json_is_unchanged() {
        let s = r#"{"a":1,"b":[2,3]}"#;
        let out = repair(s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn repair_code_fenced_json() {
        let s = "```json\n{\"a\":1}\n```";
        let out = repair(s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn repair_trailing_comma_in_fenced() {
        let s = "```json\n{\"a\":1,}\n```";
        let out = repair(s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn repair_prose_wrapped_with_trailing_comma() {
        let s = "Sure! Here's the answer:\n{\"name\":\"Roz\",\"count\":3,}\nEnjoy!";
        let out = repair(s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["name"], "Roz");
        assert_eq!(parsed["count"], 3);
    }

    #[test]
    fn repair_dangling_nested_object() {
        let s = "{\"a\":{\"b\":1";
        let out = repair(s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["a"]["b"], 1);
    }

    #[test]
    fn repair_garbage_is_unrepairable() {
        let err = repair("{not json at all").unwrap_err();
        let JsonRepairError::Unrepairable(_) = err;
    }

    #[test]
    fn repair_extra_closer_is_unrepairable() {
        let err = repair("{\"a\":1}}").unwrap_err();
        let JsonRepairError::Unrepairable(_) = err;
    }
}
