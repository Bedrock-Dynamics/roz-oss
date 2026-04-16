//! Integration tests for `roz_agent::model::openai::OpenAiProvider` (Plan 19-10).
//!
//! Exercises the adapter end-to-end against wiremock-served SSE fixtures:
//! - `response_schema` wire-shape assertion (JsonSchema strict-mode on Chat wire).
//! - Trailing-comma repair without a model retry.
//! - Unrepairable → one repair-turn retry → success with summed usage.
//! - Unrepairable → retry still bad → `AgentError::StructuredOutputParse`.
//! - Stream-mode ResponseEvent → StreamChunk translation.
//! - OWM-04 / SC2 cross-turn strip invocation in outbound body.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use futures::StreamExt;
use roz_agent::error::AgentError;
use roz_agent::model::openai::{OpenAiProvider, WireApi};
use roz_agent::model::types::{
    CompletionRequest, ContentPart, Message, MessageRole, Model, StreamChunk, ToolChoiceStrategy,
};
use roz_openai::auth::api_key::ApiKeyAuth;
use roz_openai::client::OpenAiClient;
use secrecy::SecretString;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn build_provider(server: &MockServer) -> OpenAiProvider {
    let client = Arc::new(OpenAiClient::new(
        format!("{}/v1", server.uri()),
        Arc::new(ApiKeyAuth::new(SecretString::from("sk-test".to_string()))),
        reqwest::Client::new(),
    ));
    OpenAiProvider::new(client, "gpt-5-mini".to_string(), WireApi::Chat)
}

fn chat_sse_one_shot(text: &str) -> String {
    // A minimal Chat-Completions SSE response carrying one text delta + finish.
    let chunk1 = serde_json::json!({
        "choices": [{ "delta": { "content": text }, "finish_reason": null }],
    });
    let chunk_final = serde_json::json!({
        "choices": [{ "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 7, "completion_tokens": 13 },
    });
    format!(
        "data: {chunk1}\n\ndata: {chunk_final}\n\ndata: [DONE]\n\n",
        chunk1 = chunk1,
        chunk_final = chunk_final,
    )
}

fn chat_sse_tool_calls() -> String {
    // Multi-chunk SSE: text delta + tool_call
    let chunk1 = serde_json::json!({
        "choices": [{ "delta": { "content": "ok" }, "finish_reason": null }],
    });
    let chunk_tool = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_123",
                    "function": { "name": "get_weather", "arguments": "{\"city\":" }
                }]
            },
            "finish_reason": null
        }],
    });
    let chunk_tool_args = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": "\"Paris\"}" }
                }]
            },
            "finish_reason": null
        }],
    });
    let chunk_final = serde_json::json!({
        "choices": [{ "delta": {}, "finish_reason": "tool_calls" }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 5 },
    });
    format!(
        "data: {chunk1}\n\ndata: {chunk_tool}\n\ndata: {chunk_tool_args}\n\ndata: {chunk_final}\n\ndata: [DONE]\n\n"
    )
}

fn make_sse_response(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body.into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_provider_complete_returns_text_from_chat_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_one_shot("hello world")))
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
    assert_eq!(resp.text().as_deref(), Some("hello world"));
    assert_eq!(resp.usage.input_tokens, 7);
    assert_eq!(resp.usage.output_tokens, 13);
}

#[tokio::test]
async fn openai_provider_complete_with_response_schema_sets_json_schema_on_chat() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_one_shot(r#"{"answer":"42"}"#)))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let schema = serde_json::json!({"type":"object","properties":{"answer":{"type":"string"}}});
    let req = CompletionRequest {
        messages: vec![Message::user("q")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: Some(schema.clone()),
    };
    let resp = provider.complete(&req).await.expect("complete ok");
    let text = resp.text().unwrap();
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["answer"], "42");

    // Inspect the outbound request body for response_format shape.
    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(body["response_format"]["json_schema"]["name"], "roz_output_schema");
    assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
}

#[tokio::test]
async fn openai_provider_complete_repairs_trailing_comma_without_retry() {
    let server = MockServer::start().await;
    // Only ONE upstream call expected — the local json_repair fixes the comma.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_one_shot(r#"{"answer":"42",}"#)))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("q")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: Some(serde_json::json!({"type":"object"})),
    };
    let resp = provider.complete(&req).await.expect("complete ok");
    let text = resp.text().unwrap();
    // Repaired string must parse cleanly.
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["answer"], "42");
}

#[tokio::test]
async fn openai_provider_complete_retries_on_unrepairable_json() {
    let server = MockServer::start().await;

    // Two mocks: first upstream serves garbage; second serves valid JSON.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_one_shot(
            "not json at all nothing to repair",
        )))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_one_shot(r#"{"answer":"42"}"#)))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("q")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: Some(serde_json::json!({"type":"object"})),
    };
    let resp = provider.complete(&req).await.expect("complete ok");
    let text = resp.text().unwrap();
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["answer"], "42");

    // Total usage is SUM across both calls.
    assert_eq!(resp.usage.input_tokens, 14, "summed input tokens (7+7)");
    assert_eq!(resp.usage.output_tokens, 26, "summed output tokens (13+13)");

    // Second request must include the malformed output as an assistant turn +
    // synthetic repair-user turn.
    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 2);
    let second_body: Value = serde_json::from_slice(&received[1].body).unwrap();
    let messages = second_body["messages"].as_array().unwrap();
    // Original user + appended assistant(malformed) + appended user(repair prompt).
    assert!(messages.len() >= 3);
    let last_user = &messages[messages.len() - 1];
    assert_eq!(last_user["role"], "user");
    let last_text = last_user["content"].as_str().unwrap_or("");
    assert!(last_text.contains("Return ONLY JSON"), "got: {last_text}");
    let assistant_echo = &messages[messages.len() - 2];
    assert_eq!(assistant_echo["role"], "assistant");
    let echoed = assistant_echo["content"].as_str().unwrap_or("");
    assert!(echoed.contains("not json at all"));
}

#[tokio::test]
async fn openai_provider_complete_surfaces_structured_output_parse_after_retry() {
    let server = MockServer::start().await;

    // Both calls serve unrepairable garbage.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_one_shot("garbage garbage garbage")))
        .expect(2)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("q")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: Some(serde_json::json!({"type":"object"})),
    };
    let err = provider.complete(&req).await.expect_err("must fail");
    // Downcast through Box<dyn Error> to AgentError.
    let ae = err.downcast_ref::<AgentError>().expect("AgentError");
    match ae {
        AgentError::StructuredOutputParse { raw, err } => {
            assert!(raw.contains("garbage"), "raw preserved: {raw}");
            assert!(!err.is_empty(), "err non-empty");
        }
        other => panic!("expected StructuredOutputParse, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_provider_stream_translates_response_event_to_stream_chunk() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_tool_calls()))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);
    let req = CompletionRequest {
        messages: vec![Message::user("weather in Paris?")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: Some(ToolChoiceStrategy::Auto),
        response_schema: None,
    };
    let mut stream = provider.stream(&req).await.expect("stream ok");
    let mut saw_text = false;
    let mut saw_tool_start = false;
    let mut saw_tool_delta = false;
    let mut saw_done = false;
    while let Some(chunk) = stream.next().await {
        match chunk.expect("chunk") {
            StreamChunk::TextDelta(_) => saw_text = true,
            StreamChunk::ToolUseStart { name, .. } => {
                saw_tool_start = true;
                assert_eq!(name, "get_weather");
            }
            StreamChunk::ToolUseInputDelta(_) => saw_tool_delta = true,
            StreamChunk::Done(resp) => {
                saw_done = true;
                // Tool use must have assembled args.
                let has_tool = resp.parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. }));
                assert!(has_tool, "assembled tool_use in Done: {:?}", resp.parts);
            }
            _ => {}
        }
    }
    assert!(saw_text, "TextDelta");
    assert!(saw_tool_start, "ToolUseStart");
    assert!(saw_tool_delta, "ToolUseInputDelta");
    assert!(saw_done, "Done");
}

#[tokio::test]
async fn openai_provider_strips_unsigned_reasoning_from_prior_turn_before_resend() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(make_sse_response(chat_sse_one_shot("ok")))
        .expect(1)
        .mount(&server)
        .await;

    let provider = build_provider(&server);

    // Prior assistant turn carries an UNSIGNED reasoning segment (no signature).
    let prior_assistant = Message {
        role: MessageRole::Assistant,
        parts: vec![
            ContentPart::Thinking {
                thinking: "INTERNAL_REASONING_MARKER".to_string(),
                signature: String::new(),
            },
            ContentPart::Text {
                text: "visible answer".to_string(),
            },
        ],
    };
    let req = CompletionRequest {
        messages: vec![Message::user("q1"), prior_assistant, Message::user("q2")],
        tools: Vec::new(),
        max_tokens: 64,
        tool_choice: None,
        response_schema: None,
    };
    let _ = provider.complete(&req).await.expect("complete ok");

    // Inspect the OUTBOUND body — the unsigned marker must not appear anywhere.
    let received = server.received_requests().await.expect("requests");
    assert_eq!(received.len(), 1);
    let body_str = std::str::from_utf8(&received[0].body).expect("utf8");
    assert!(
        !body_str.contains("INTERNAL_REASONING_MARKER"),
        "OWM-04 / SC2: unsigned reasoning leaked to outbound body: {body_str}"
    );
    // Visible text survives.
    assert!(body_str.contains("visible answer"));
}
