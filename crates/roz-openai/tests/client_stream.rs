//! Integration tests for [`roz_openai::client::OpenAiClient`] (Plan 19-07 Task 3).
//!
//! Exercises the streaming client against wiremock fixtures:
//!
//! - `/chat/completions` endpoint dispatch (POST verb + stream body assertion)
//! - Happy-path Chat stream → OutputTextDelta events
//! - 500 error → `OpenAiError::Http`
//! - `/responses` endpoint dispatch
//! - Responses stream → Completed event with response_id
//! - `X-Reasoning-Included: true` → `ServerReasoningIncluded(true)` emitted as first event
//!
//! Fixtures live under `tests/fixtures/` and match the SSE framing Chat Completions + Responses
//! wires actually emit (tested against OpenAI + vLLM 0.9 responses during dev).

use futures::StreamExt;
use roz_openai::auth::api_key::ApiKeyAuth;
use roz_openai::client::OpenAiClient;
use roz_openai::error::OpenAiError;
use roz_openai::wire::chat::{ChatCompletionsRequest, ChatMessage};
use roz_openai::wire::events::ResponseEvent;
use roz_openai::wire::responses::{ResponseItem, ResponsesApiRequest};
use secrecy::SecretString;
use std::sync::Arc;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

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
async fn stream_chat_hits_chat_completions_endpoint() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("chat_simple_hello.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_chat(minimal_chat_request(), None)
        .await
        .expect("stream_chat OK");

    // Drain — the expect(1) assertion above verifies the request shape.
    while let Some(next) = stream.next().await {
        next.expect("no error");
    }
}

#[tokio::test]
async fn stream_chat_yields_text_delta_from_fixture() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("chat_simple_hello.sse")),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_chat(minimal_chat_request(), None)
        .await
        .expect("stream ok");

    let mut text = String::new();
    let mut completed = false;
    while let Some(next) = stream.next().await {
        match next.expect("no error") {
            ResponseEvent::OutputTextDelta(d) => text.push_str(&d),
            ResponseEvent::Completed { .. } => completed = true,
            _ => {}
        }
    }
    assert_eq!(text, "Hello world");
    assert!(completed, "must emit Completed from finish_reason=stop");
}

#[tokio::test]
async fn stream_chat_propagates_http_error_as_error() {
    // WR-06: 5xx now routes to ServerError (was OpenAiError::Http for every
    // non-2xx). 4xx remains Http so the provider-edge classifier can split
    // retryable-upstream vs. client-fault.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    match client.stream_chat(minimal_chat_request(), None).await {
        Err(OpenAiError::ServerError(msg)) => {
            assert!(msg.contains("500"), "msg: {msg}");
            assert!(msg.contains("upstream exploded"), "msg: {msg}");
        }
        Err(other) => panic!("expected ServerError, got {other:?}"),
        Ok(_) => panic!("500 must surface as error"),
    }
}

/// WR-06: 4xx still routes to OpenAiError::Http (not ServerError).
#[tokio::test]
async fn stream_chat_maps_4xx_to_http_not_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    match client.stream_chat(minimal_chat_request(), None).await {
        Err(OpenAiError::Http { status, body }) => {
            assert_eq!(status, 400);
            assert!(body.contains("bad request"));
        }
        Err(other) => panic!("expected Http err for 400, got {other:?}"),
        Ok(_) => panic!("400 must surface as error"),
    }
}

#[tokio::test]
async fn stream_responses_hits_responses_endpoint() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_hello.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");
    while let Some(next) = stream.next().await {
        next.expect("no error");
    }
}

#[tokio::test]
async fn stream_responses_yields_completed_event() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_hello.sse")),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");

    let mut final_response_id: Option<String> = None;
    let mut text = String::new();
    while let Some(next) = stream.next().await {
        match next.expect("no error") {
            ResponseEvent::OutputTextDelta(d) => text.push_str(&d),
            ResponseEvent::Completed { response_id, .. } => {
                final_response_id = response_id;
            }
            _ => {}
        }
    }
    assert_eq!(text, "Hello world");
    assert_eq!(final_response_id.as_deref(), Some("resp_abc"));
}

#[tokio::test]
async fn stream_responses_emits_server_reasoning_included_when_header_present() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("x-reasoning-included", "true")
                .set_body_string(fixture("responses_reasoning_included_header.sse")),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");

    let first = stream.next().await.expect("at least one event").expect("no error");
    assert!(
        matches!(first, ResponseEvent::ServerReasoningIncluded(true)),
        "first event must be ServerReasoningIncluded(true); got {first:?}"
    );
}

#[tokio::test]
async fn stream_responses_applies_transform_hook_before_send() {
    // Asserts the transform hook ran BEFORE the body was serialized to the wire.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(serde_json::json!({"parallel_tool_calls": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_hello.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Build a client with a transform hook that flips `parallel_tool_calls` to true.
    let auth = Arc::new(ApiKeyAuth::new(SecretString::from("test-key".to_string())));
    let client = OpenAiClient::new(server.uri(), auth, reqwest::Client::new()).with_transform_hook(Arc::new(
        |req: &mut ResponsesApiRequest| {
            req.parallel_tool_calls = true;
        },
    ));

    let mut stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");
    while let Some(next) = stream.next().await {
        let _: ResponseEvent = next.expect("no error");
    }
}

#[tokio::test]
async fn stream_chat_sets_authorization_bearer_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::header("authorization", "Bearer test-key"))
        .and(wiremock::matchers::header("accept", "text/event-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("chat_simple_hello.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_chat(minimal_chat_request(), None)
        .await
        .expect("stream ok");
    while let Some(next) = stream.next().await {
        next.expect("no error");
    }
}

/// WR-05 regression: when the auth provider yields an empty bearer
/// (AuthMode::None for Ollama / llama.cpp / vLLM without keys), the client
/// must NOT send an Authorization header at all. Sending `Bearer ` triggers
/// 401 on strict proxies.
#[tokio::test]
async fn stream_chat_omits_authorization_when_token_is_empty() {
    use wiremock::matchers::header_exists;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::header("accept", "text/event-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("chat_simple_hello.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Separate mock matcher asserting Authorization is ABSENT — will cause a
    // match failure if the client sends `Bearer ` with an empty token.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(599))
        .expect(0)
        .mount(&server)
        .await;

    let auth = Arc::new(ApiKeyAuth::new(SecretString::from(String::new())));
    let client = OpenAiClient::new(server.uri(), auth, reqwest::Client::new());
    let mut stream = client
        .stream_chat(minimal_chat_request(), None)
        .await
        .expect("stream ok");
    while let Some(next) = stream.next().await {
        next.expect("no error");
    }
}
