//! JWT claim extraction for ChatGPT-backend OAuth tokens.
//!
//! Ports `parse_chatgpt_jwt_claims` from the codex-rs login crate
//! (`login/src/token_data.rs:72-91`, pinned SHA `da86cedbd439d38fbd7e613e4e88f8f6f138debb`).
//!
//! # Why no signature verification
//!
//! Roz never issues these JWTs — OpenAI does. We extract the `chatgpt_account_id`
//! claim purely so we can echo it back to OpenAI in the `chatgpt-account-id`
//! request header. OpenAI re-validates the JWT server-side; Roz makes no
//! authorization decision based on the claim contents (T-19-05-02 mitigation
//! per Plan 19-05 threat register).
//!
//! # Plan-type
//!
//! `plan_type` is kept as `Option<String>` rather than ported as `codex_protocol::auth::PlanType`
//! to avoid pulling in a foreign enum that codex-rs evolves independently.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

use super::AuthError;

/// Subset of ChatGPT-backend JWT claims that Roz consumes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdTokenInfo {
    pub chatgpt_account_id: Option<String>,
    pub email: Option<String>,
    /// Plan tier reported by OpenAI (e.g. `"plus"`, `"team"`, `"enterprise"`).
    /// Kept as `Option<String>` — we deliberately do NOT port codex-rs's
    /// `PlanType` enum.
    pub plan_type: Option<String>,
}

/// Decode the payload of an ID-token JWT and extract the claims Roz uses.
///
/// The JWT signature is NOT verified — see the module docs for why.
pub fn parse_chatgpt_jwt_claims(jwt: &str) -> Result<IdTokenInfo, AuthError> {
    let mut parts = jwt.split('.');
    let _header = parts
        .next()
        .ok_or_else(|| AuthError::InvalidJwt("missing header segment".into()))?;
    let payload_b64 = parts
        .next()
        .ok_or_else(|| AuthError::InvalidJwt("missing payload segment".into()))?;
    let _sig = parts
        .next()
        .ok_or_else(|| AuthError::InvalidJwt("missing signature segment".into()))?;
    if parts.next().is_some() {
        return Err(AuthError::InvalidJwt("jwt must have exactly 3 segments".into()));
    }

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| AuthError::InvalidJwt(format!("base64 decode failed: {e}")))?;
    let payload: Value = serde_json::from_slice(&payload_bytes)
        .map_err(|e| AuthError::InvalidJwt(format!("payload is not valid JSON: {e}")))?;

    let custom = payload.get("https://api.openai.com/auth").and_then(Value::as_object);
    let chatgpt_account_id = custom
        .and_then(|c| c.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let plan_type = custom
        .and_then(|c| c.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let email = payload.get("email").and_then(Value::as_str).map(str::to_string);

    Ok(IdTokenInfo {
        chatgpt_account_id,
        email,
        plan_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload: &Value) -> String {
        // Minimal forged JWT — header and signature are placeholders since we
        // never verify the signature.
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        let sig = URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{payload_b64}.{sig}")
    }

    #[test]
    fn parse_chatgpt_jwt_extracts_account_id() {
        let payload = serde_json::json!({
            "email": "alice@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-test-123",
                "chatgpt_plan_type": "plus"
            }
        });
        let jwt = make_jwt(&payload);
        let info = parse_chatgpt_jwt_claims(&jwt).expect("parse");
        assert_eq!(info.chatgpt_account_id.as_deref(), Some("acct-test-123"));
        assert_eq!(info.email.as_deref(), Some("alice@example.com"));
        assert_eq!(info.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn parse_chatgpt_jwt_missing_custom_claim_returns_none_account_id() {
        let payload = serde_json::json!({ "email": "bob@example.com" });
        let jwt = make_jwt(&payload);
        let info = parse_chatgpt_jwt_claims(&jwt).expect("parse");
        assert!(info.chatgpt_account_id.is_none());
        assert_eq!(info.email.as_deref(), Some("bob@example.com"));
    }

    #[test]
    fn parse_chatgpt_jwt_malformed_base64_returns_error() {
        let jwt = "header.@@@not-base64@@@.sig";
        let err = parse_chatgpt_jwt_claims(jwt).expect_err("expected error");
        assert!(matches!(err, AuthError::InvalidJwt(_)));
    }

    #[test]
    fn parse_chatgpt_jwt_not_three_segments_returns_error() {
        let err = parse_chatgpt_jwt_claims("only.two").expect_err("expected error");
        assert!(matches!(err, AuthError::InvalidJwt(_)));
        let err = parse_chatgpt_jwt_claims("a.b.c.d").expect_err("expected error");
        assert!(matches!(err, AuthError::InvalidJwt(_)));
    }
}
