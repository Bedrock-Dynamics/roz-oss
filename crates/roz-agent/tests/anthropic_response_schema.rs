//! Integration tests for `AnthropicProvider::complete` response_schema path
//! (Plan 19-12).
//!
//! Exercises the synthetic `respond` tool-forcing path against a wiremock-
//! served Anthropic Messages API:
//!
//! - request body asserts: `tools[]` includes a `respond` tool with the
//!   caller's schema as `input_schema`, and `tool_choice = {type: "tool",
//!   name: "respond"}`.
//! - happy path: `tool_use.input` extracted, returned as canonical JSON.
//! - repair path: malformed `tool_use.input` repaired locally without retry.
//! - regression: with `response_schema = None`, NO `respond` tool injected and
//!   `tool_choice` absent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use roz_agent::error::AgentError;
use roz_agent::model::anthropic::{AnthropicConfig, AnthropicProvider};
use roz_agent::model::types::{CompletionRequest, Message, Model};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROXY_PROVIDER: &str = "anthropic";

fn build_provider(server: &MockServer) -> AnthropicProvider {
    AnthropicProvider::new(AnthropicConfig {
        gateway_url: server.uri(),
        api_key: "paig-test".to_string(),
        model: "claude-sonnet-4-5".to_string(),
        thinking: None,
        timeout: Duration::from_secs(30),
        proxy_provider: PROXY_PROVIDER.to_string(),
        direct_api_key: None,
    })
}

fn anthropic_tool_use_response(input: &Value) -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5",
        "content": [{
            "type": "tool_use",
            "id": "toolu_respond",
            "name": "respond",
            "input": input,
        }],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 10, "output_tokens": 7 },
    })
}

fn anthropic_text_response(text: &str) -> Value {
    json!({
        "id": "msg_text",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5",
        "content": [{ "type": "text", "text": text }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 5, "output_tokens": 3 },
    })
}

#[tokio::test]
async fn anthropic_response_schema_forces_respond_tool_in_request() {
    let server = MockServer::start().await;
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}}});

    Mock::given(method("POST"))
        .and(path(format!("/proxy/{PROXY_PROVIDER}/v1/messages")))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_tool_use_response(&json!({"answer":"42"}))))
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

    // Inspect outbound body.
    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    let tools = body["tools"].as_array().expect("tools array present");
    let respond = tools
        .iter()
        .find(|t| t["name"] == "respond")
        .expect("respond tool present");
    assert_eq!(respond["input_schema"]["type"], "object");
    assert_eq!(respond["input_schema"]["properties"]["answer"]["type"], "string");
    assert_eq!(body["tool_choice"]["type"], "tool");
    assert_eq!(body["tool_choice"]["name"], "respond");
}

#[tokio::test]
async fn anthropic_response_schema_extracts_from_tool_use_input() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/proxy/{PROXY_PROVIDER}/v1/messages")))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_tool_use_response(&json!({"x":1,"y":2}))))
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
    assert_eq!(parsed["x"], 1);
    assert_eq!(parsed["y"], 2);
}

#[tokio::test]
async fn anthropic_response_schema_errors_when_no_respond_tool_use() {
    // Server returns plain text instead of forced tool_use — provider must
    // surface StructuredOutputParse with the diagnostic raw marker.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/proxy/{PROXY_PROVIDER}/v1/messages")))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_text_response("ignored chatter")))
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
    let err = provider.complete(&req).await.expect_err("must fail");
    let ae = err.downcast_ref::<AgentError>().expect("AgentError");
    match ae {
        AgentError::StructuredOutputParse { raw, err } => {
            assert!(raw.contains("no forced tool_use"), "raw marker: {raw}");
            assert!(err.contains("respond tool"), "err mentions respond: {err}");
        }
        other => panic!("expected StructuredOutputParse, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_without_response_schema_does_not_inject_respond_tool() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/proxy/{PROXY_PROVIDER}/v1/messages")))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_text_response("hi there")))
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
    assert_eq!(resp.text().as_deref(), Some("hi there"));

    let received = server.received_requests().await.expect("requests");
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    // Regression: no respond tool, no tool_choice forcing.
    assert!(
        body.get("tools").is_none() || body["tools"].as_array().is_some_and(Vec::is_empty),
        "no tools without response_schema: got {body}"
    );
    assert!(
        body.get("tool_choice").is_none(),
        "no tool_choice without response_schema: got {body}"
    );
}
