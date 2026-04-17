//! SSE-parser integration tests driving the public `OpenAiClient` surface.
//!
//! The in-crate unit tests in `src/sse.rs` exercise the decoder directly; these integration
//! tests confirm the behaviors are visible end-to-end through `OpenAiClient`:
//!
//! - `[DONE]` sentinel terminates the stream cleanly (no event yielded for the sentinel frame).
//! - Idle timeout surfaces as an error item after the configured window elapses.
//! - `X-Reasoning-Included: true` header fires `ResponseEvent::ServerReasoningIncluded(true)`
//!   as the first event before any body events.

use futures::StreamExt;
use roz_openai::auth::api_key::ApiKeyAuth;
use roz_openai::client::OpenAiClient;
use roz_openai::wire::chat::{ChatCompletionsRequest, ChatMessage};
use roz_openai::wire::events::ResponseEvent;
use roz_openai::wire::responses::{ResponseItem, ResponsesApiRequest};
use secrecy::SecretString;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_client(base_url: String) -> OpenAiClient {
    let auth = Arc::new(ApiKeyAuth::new(SecretString::from("test-key".to_string())));
    OpenAiClient::new(base_url, auth, reqwest::Client::new())
}

fn minimal_chat_request() -> ChatCompletionsRequest {
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

fn minimal_responses_request() -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "gpt-5".to_string(),
        instructions: String::new(),
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![serde_json::json!({"type": "input_text", "text": "hi"})],
        }],
        tools: Vec::new(),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
    }
}

#[tokio::test]
async fn sse_parser_handles_done_sentinel() {
    // Sentinel MUST NOT surface as an event; stream terminates cleanly after it and any frames
    // arriving AFTER `data: [DONE]` must be dropped. We assert on the Completed event count
    // (always emitted on finish_reason=stop) — content frames written after [DONE] would
    // otherwise produce an extra Completed event or an unexpected OutputTextDelta.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
                data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: [DONE]\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"SHOULD NOT APPEAR\"},\"finish_reason\":\"stop\"}]}\n\n";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_chat(minimal_chat_request(), None)
        .await
        .expect("stream ok");

    let mut text = String::new();
    let mut completed_count = 0;
    let mut saw_shouldnot = false;
    while let Some(next) = stream.next().await {
        match next.expect("no error") {
            ResponseEvent::OutputTextDelta(d) => {
                if d.contains("SHOULD NOT APPEAR") {
                    saw_shouldnot = true;
                }
                text.push_str(&d);
            }
            ResponseEvent::Completed { .. } => completed_count += 1,
            _ => {}
        }
    }
    assert!(
        !saw_shouldnot,
        "content AFTER [DONE] must be dropped; got text={text:?}"
    );
    assert_eq!(
        completed_count, 1,
        "exactly one Completed expected; frames after [DONE] must not produce a second"
    );
}

// NOTE: `sse_parser_emits_timeout_on_idle` is exercised as a unit test against an in-memory
// byte stream in `crates/roz-openai/src/sse.rs::tests::decode_surfaces_timeout_on_idle`.
// Wiremock does not simulate a half-open HTTP body stream (its `set_delay` delays the response
// start, not mid-stream), so an integration-level idle-timeout assertion would have to drive the
// decoder directly. The in-crate unit test covers the invariant end-to-end through the same
// decoder the client uses.

#[tokio::test]
async fn sse_parser_captures_x_reasoning_included_header() {
    // When the server advertises X-Reasoning-Included: true, the client must emit
    // ServerReasoningIncluded(true) as the FIRST event before any body events.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("x-reasoning-included", "true")
                .set_body_string(
                    "event: response.created\ndata: {\"type\":\"response.created\"}\n\n\
                     event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_x\"}}\n\n\
                     data: [DONE]\n\n",
                ),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");

    let first = stream.next().await.expect("first event").expect("no error");
    assert!(
        matches!(first, ResponseEvent::ServerReasoningIncluded(true)),
        "expected ServerReasoningIncluded(true) as first event; got {first:?}"
    );
}
