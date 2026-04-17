//! Integration tests for the Chat Completions wire (API-key path).
//!
//! Drives `OpenAiClient::stream_chat` against wiremock-served SSE fixtures and asserts on the
//! wire-level `ResponseEvent` sequence. The full `json_repair` loop exercised by
//! `OpenAiProvider::complete` lives in `roz-agent/tests/openai_provider.rs` — the malformed-JSON
//! fixture here drives the wire layer only (asserting the raw content flows through intact so
//! the provider layer can repair).

use futures::StreamExt;
use roz_openai::auth::api_key::ApiKeyAuth;
use roz_openai::client::OpenAiClient;
use roz_openai::wire::chat::{ChatCompletionsRequest, ChatMessage};
use roz_openai::wire::events::ResponseEvent;
use roz_openai::wire::responses::ResponseItem;
use secrecy::SecretString;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

fn make_client(base_url: String) -> OpenAiClient {
    let auth = Arc::new(ApiKeyAuth::new(SecretString::from("test-key".to_string())));
    OpenAiClient::new(base_url, auth, reqwest::Client::new())
}

fn chat_request() -> ChatCompletionsRequest {
    ChatCompletionsRequest {
        model: "gpt-4o-mini".to_string(),
        messages: vec![ChatMessage::User {
            content: "hi".to_string(),
        }],
        tools: Vec::new(),
        tool_choice: None,
        stream: true,
        max_tokens: None,
        temperature: None,
        response_format: None,
    }
}

async fn collect_events(server: &MockServer, fixture_name: &str) -> Vec<ResponseEvent> {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture(fixture_name)),
        )
        .expect(1)
        .mount(server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client.stream_chat(chat_request(), None).await.expect("stream ok");

    let mut events = Vec::new();
    while let Some(next) = stream.next().await {
        events.push(next.expect("no wire error"));
    }
    events
}

#[tokio::test]
async fn chat_wire_single_tool_call() {
    let server = MockServer::start().await;
    let events = collect_events(&server, "chat_single_tool_call.sse").await;

    // Expect a ToolCallStart then args deltas then OutputItemDone(FunctionCall).
    let mut saw_start = false;
    let mut assembled = String::new();
    let mut done_call: Option<(String, String, String)> = None;
    for ev in &events {
        match ev {
            ResponseEvent::ToolCallStart { id, name } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "move_arm");
                saw_start = true;
            }
            ResponseEvent::ToolCallArgsDelta(d) => assembled.push_str(d),
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            }) => {
                done_call = Some((call_id.clone(), name.clone(), arguments.clone()));
            }
            _ => {}
        }
    }
    assert!(saw_start, "expected ToolCallStart");
    assert_eq!(assembled, "{\"x\":1.0}");
    let (call_id, name, args) = done_call.expect("expected OutputItemDone(FunctionCall)");
    assert_eq!(call_id, "call_1");
    assert_eq!(name, "move_arm");
    assert_eq!(args, "{\"x\":1.0}");
}

#[tokio::test]
async fn chat_wire_multi_tool_call() {
    let server = MockServer::start().await;
    let events = collect_events(&server, "chat_multi_tool_call.sse").await;

    // Both tool calls must be emitted in order (index 0 then index 1).
    let mut starts: Vec<(String, String)> = Vec::new();
    let mut dones: Vec<(String, String, String)> = Vec::new();
    for ev in &events {
        match ev {
            ResponseEvent::ToolCallStart { id, name } => starts.push((id.clone(), name.clone())),
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            }) => dones.push((call_id.clone(), name.clone(), arguments.clone())),
            _ => {}
        }
    }
    assert_eq!(
        starts,
        vec![
            ("call_1".to_string(), "move_arm".to_string()),
            ("call_2".to_string(), "grip".to_string()),
        ]
    );
    assert_eq!(dones.len(), 2, "expected 2 FunctionCall done items; got {dones:?}");
    assert_eq!(dones[0].0, "call_1");
    assert_eq!(dones[0].2, "{\"x\":1.0}");
    assert_eq!(dones[1].0, "call_2");
    assert_eq!(dones[1].2, "{\"force\":0.5}");
}

#[tokio::test]
async fn chat_wire_reasoning_think_tags() {
    let server = MockServer::start().await;
    let events = collect_events(&server, "chat_reasoning_stream.sse").await;

    let mut reasoning = String::new();
    let mut visible = String::new();
    let mut completed = false;
    for ev in events {
        match ev {
            ResponseEvent::ReasoningContentDelta { delta, .. } => reasoning.push_str(&delta),
            ResponseEvent::OutputTextDelta(d) => visible.push_str(&d),
            ResponseEvent::Completed { .. } => completed = true,
            _ => {}
        }
    }
    assert!(
        reasoning.contains("Need to move..."),
        "reasoning should capture pre-close-tag text; got {reasoning:?}"
    );
    assert!(
        visible.contains("Actually yes."),
        "visible text should contain post-close-tag content; got {visible:?}"
    );
    assert!(completed, "stream must emit Completed on finish_reason=stop");
}

#[tokio::test]
async fn chat_wire_reasoning_field() {
    let server = MockServer::start().await;
    let events = collect_events(&server, "chat_reasoning_field.sse").await;

    let mut reasoning = String::new();
    let mut visible = String::new();
    for ev in events {
        match ev {
            ResponseEvent::ReasoningContentDelta { delta, .. } => reasoning.push_str(&delta),
            ResponseEvent::OutputTextDelta(d) => visible.push_str(&d),
            _ => {}
        }
    }
    assert_eq!(reasoning, "Need to move...");
    assert_eq!(visible, "Actually yes.");
}

#[tokio::test]
async fn chat_wire_malformed_json_structured_output_delivers_raw_body() {
    // Wire-level invariant: the wire client delivers the raw malformed content verbatim so the
    // provider-level json_repair loop (exercised by roz-agent/tests/openai_provider.rs) can act on
    // it. wiremock `.expect(1)` asserts the client makes exactly ONE call for the single-fixture
    // turn (no silent retry inside the wire layer).
    let server = MockServer::start().await;
    let events = collect_events(&server, "chat_malformed_json_structured_output.sse").await;

    let mut text = String::new();
    let mut completed = false;
    for ev in events {
        match ev {
            ResponseEvent::OutputTextDelta(d) => text.push_str(&d),
            ResponseEvent::Completed { .. } => completed = true,
            _ => {}
        }
    }
    assert_eq!(text, "{\"ok\":true,}");
    assert!(completed);
}

#[tokio::test]
async fn chat_wire_ollama_regression_finish_reason_without_tool_calls() {
    // OWM-08 regression: Ollama sometimes omits `tool_calls` on finish_reason=stop.
    // The wire layer MUST still assemble the tool call from prior chunks and emit it.
    let server = MockServer::start().await;
    let events = collect_events(&server, "ollama_single_tool_call.sse").await;

    let mut saw_start = false;
    let mut args = String::new();
    for ev in &events {
        match ev {
            ResponseEvent::ToolCallStart { id, name } => {
                assert_eq!(id, "call_o1");
                assert_eq!(name, "move_arm");
                saw_start = true;
            }
            ResponseEvent::ToolCallArgsDelta(d) => args.push_str(d),
            _ => {}
        }
    }
    assert!(saw_start, "expected ToolCallStart before stop");
    assert_eq!(args, "{\"x\":1}");

    // At least one Completed event must fire even though the final chunk omits tool_calls.
    let completed_count = events
        .iter()
        .filter(|e| matches!(e, ResponseEvent::Completed { .. }))
        .count();
    assert!(
        completed_count >= 1,
        "expected at least one Completed; got events {events:?}"
    );
}
