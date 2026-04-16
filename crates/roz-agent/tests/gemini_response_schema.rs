//! Integration tests for `GeminiProvider::complete` response_schema path
//! (Plan 19-12).
//!
//! Exercises the `generationConfig.responseSchema` + `responseMimeType`
//! native structured-output path against a wiremock-served Gemini
//! generateContent endpoint:
//!
//! - request body: `generationConfig.responseSchema` + `responseMimeType =
//!   "application/json"`.
//! - happy path: assistant text already valid JSON.
//! - local repair: trailing-comma JSON repaired without retry.
//! - retry path: unrepairable text triggers second generateContent call.
//! - regression: with `response_schema = None`, neither field present.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use roz_agent::model::gemini::{GeminiConfig, GeminiProvider};
use roz_agent::model::types::{CompletionRequest, Message, Model};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn build_provider(server: &MockServer) -> GeminiProvider {
    GeminiProvider::new(GeminiConfig {
        gateway_url: server.uri(),
        api_key: "paig-test".to_string(),
        model: "gemini-2.5-flash".to_string(),
        timeout: Duration::from_secs(30),
    })
}

fn gemini_text_response(text: &str) -> Value {
    json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{ "text": text }],
            },
            "finishReason": "STOP",
        }],
        "usageMetadata": {
            "promptTokenCount": 11,
            "candidatesTokenCount": 7,
            "totalTokenCount": 18,
        },
    })
}

fn generate_path() -> &'static str {
    // The provider builds:
    // {gateway}/proxy/google-vertex/v1beta1/models/{model}:generateContent
    r"^/proxy/google-vertex/v1beta1/models/.+:generateContent$"
}

#[tokio::test]
async fn gemini_response_schema_sets_generation_config_response_schema_and_mime_type() {
    let server = MockServer::start().await;
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}}});

    Mock::given(method("POST"))
        .and(path_regex(generate_path()))
        .and(header("authorization", "Bearer paig-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_text_response(r#"{"answer":"42"}"#)))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("q")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: Some(schema.clone()),
    };
    let resp = provider.complete(&req).await.expect("complete ok");
    let parsed: Value = serde_json::from_str(resp.text().unwrap().as_str()).unwrap();
    assert_eq!(parsed["answer"], "42");

    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["generationConfig"]["responseMimeType"], "application/json");
    assert_eq!(
        body["generationConfig"]["responseSchema"], schema,
        "responseSchema must match input schema"
    );
}

#[tokio::test]
async fn gemini_response_schema_repairs_trailing_comma_no_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(generate_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_text_response(r#"{"answer":"42",}"#)))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("q")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: Some(json!({"type":"object"})),
    };
    let resp = provider.complete(&req).await.expect("complete ok");
    let parsed: Value = serde_json::from_str(resp.text().unwrap().as_str()).unwrap();
    assert_eq!(parsed["answer"], "42");
}

#[tokio::test]
async fn gemini_response_schema_retries_on_unrepairable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(generate_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_text_response("garbage cannot be repaired")))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(generate_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_text_response(r#"{"answer":"42"}"#)))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("q")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: Some(json!({"type":"object"})),
    };
    let resp = provider.complete(&req).await.expect("complete ok");
    let parsed: Value = serde_json::from_str(resp.text().unwrap().as_str()).unwrap();
    assert_eq!(parsed["answer"], "42");

    // Both calls happened; second body must contain the repair instruction.
    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 2, "expected one retry");
    let second_body: Value = serde_json::from_slice(&received[1].body).unwrap();
    let contents = second_body["contents"].as_array().unwrap();
    let last = &contents[contents.len() - 1];
    assert_eq!(last["role"], "user");
    let last_text = last["parts"][0]["text"].as_str().unwrap_or("");
    assert!(
        last_text.contains("Return ONLY JSON"),
        "repair instruction in second body: {last_text}"
    );
}

#[tokio::test]
async fn gemini_without_response_schema_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(generate_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_text_response("plain text")))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: None,
    };
    let resp = provider.complete(&req).await.expect("complete ok");
    assert_eq!(resp.text().as_deref(), Some("plain text"));

    let received = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert!(
        body["generationConfig"].get("responseSchema").is_none(),
        "no responseSchema without response_schema: got {body}"
    );
    assert!(
        body["generationConfig"].get("responseMimeType").is_none(),
        "no responseMimeType without response_schema: got {body}"
    );
}
