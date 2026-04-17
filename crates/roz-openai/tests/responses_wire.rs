//! Integration tests for the Responses wire (API-key path + ChatGPT-backend transforms).
//!
//! Drives `OpenAiClient::stream_responses` against wiremock-served SSE fixtures and asserts on
//! wire-level `ResponseEvent` sequences plus serialized request-body shape (via
//! `body_partial_json` wiremock matchers).
//!
//! # Adapter-layer concerns NOT covered here
//!
//! - ChatGPT-backend request headers (`ChatGPT-Account-ID`, `OpenAI-Beta`, `originator`,
//!   `conversation_id`, `session_id`) are attached by the Plan 19-10 adapter via
//!   `ChatGptBackendHeaders` — the wire client only attaches `Authorization` + `Accept`.
//!   Header-level assertions live in `roz-agent/tests/openai_provider.rs`.
//! - URL rewrite `/responses` → `/codex/responses` happens in the adapter via
//!   `rewrite_url_for_chatgpt`; the wire client always posts to `/responses`.
//!
//! # Transform coverage
//!
//! `responses_wire_chatgpt_backend_transforms_applied_to_body` exercises T1, T2, T3, T4, T5, T6,
//! T7, T8, T9, T12 against a single request with IDs, an orphan, an item_reference, and a gpt-5
//! family model. T10+T11 are documented no-ops (fields absent from wire type); T13-T19 are
//! adapter-layer concerns (see above); T20 is covered by
//! `responses_wire_chatgpt_backend_non_streaming_caller_synthesizes_json`.

use futures::StreamExt;
use roz_openai::auth::api_key::ApiKeyAuth;
use roz_openai::client::OpenAiClient;
use roz_openai::prompts::codex_instructions;
use roz_openai::transform::{apply_chatgpt_backend_transforms, collect_stream_to_json};
use roz_openai::wire::events::ResponseEvent;
use roz_openai::wire::responses::{
    ResponseItem, ResponsesApiRequest, TextControls, TextFormat, TextFormatType, create_text_param_for_request,
};
use secrecy::SecretString;
use std::sync::Arc;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

fn make_client(base_url: String) -> OpenAiClient {
    let auth = Arc::new(ApiKeyAuth::new(SecretString::from("test-key".to_string())));
    OpenAiClient::new(base_url, auth, reqwest::Client::new())
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

// ============================================================================
// 1. API-key happy path
// ============================================================================

#[tokio::test]
async fn responses_wire_api_key_text_delta() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_api_key_turn.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");

    let mut saw_created = false;
    let mut text = String::new();
    let mut completed_id: Option<String> = None;
    let mut saw_usage = false;
    while let Some(next) = stream.next().await {
        match next.expect("no wire error") {
            ResponseEvent::Created => saw_created = true,
            ResponseEvent::OutputTextDelta(d) => text.push_str(&d),
            ResponseEvent::Completed {
                response_id,
                token_usage,
            } => {
                completed_id = response_id;
                saw_usage = token_usage.is_some();
            }
            _ => {}
        }
    }
    assert!(saw_created, "expected Created");
    assert_eq!(text, "Hello");
    assert_eq!(completed_id.as_deref(), Some("resp_1"));
    assert!(saw_usage, "expected token usage on Completed");
}

// ============================================================================
// 2. ChatGPT-backend transforms applied to request body
// ============================================================================

#[tokio::test]
async fn responses_wire_chatgpt_backend_transforms_applied_to_body() {
    let server = MockServer::start().await;

    // body_partial_json matches a subset; drive per-field expectations via multiple matchers.
    Mock::given(method("POST"))
        .and(path("/responses"))
        // T1: store = false
        // T2: stream = true
        // T7: include contains reasoning.encrypted_content
        .and(body_partial_json(serde_json::json!({
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_oauth_chatgpt_turn.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Build a request with: input items having IDs, an orphaned function_call_output, an
    // item_reference item, and `max_output_tokens` (absent from wire type but its analogous
    // no-op is asserted via absence below).
    let mut req = ResponsesApiRequest {
        model: "gpt-5.1-codex".to_string(),
        instructions: String::new(),
        input: vec![
            ResponseItem::Message {
                id: Some("msg_1".to_string()),
                role: "user".to_string(),
                content: vec![serde_json::json!({"type": "input_text", "text": "hi"})],
            },
            ResponseItem::FunctionCallOutput {
                call_id: "orphan_call".to_string(),
                output: "orphan result".to_string(),
            },
            ResponseItem::ItemReference {
                id: "rs_stored_123".to_string(),
            },
        ],
        tools: Vec::new(),
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning: None,
        store: true,         // T1 flips to false
        stream: false,       // T2 flips to true
        include: Vec::new(), // T7 adds reasoning.encrypted_content
        service_tier: None,
        prompt_cache_key: None,
        text: None, // T8 sets verbosity=medium
        client_metadata: None,
    };

    // Apply the transforms that the ChatGPT-backend hook would apply (T3 carries the real
    // Codex instructions text — this is how Plan 19-10's adapter wires the hook).
    let instructions = codex_instructions(&req.model);
    apply_chatgpt_backend_transforms(&mut req, instructions);

    // Serialize and inspect locally before the HTTP call, to assert structural invariants
    // wiremock's body_partial_json can't express.
    let body = serde_json::to_value(&req).expect("serialize");

    // T3: instructions injected, non-empty.
    let body_instructions = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .expect("instructions present");
    assert!(
        !body_instructions.is_empty(),
        "Codex instructions must be non-empty (Plan 19-09 codex_instructions())"
    );

    // T4: every input[] item has no `id` field.
    let inputs = body.get("input").and_then(|v| v.as_array()).expect("input array");
    for item in inputs {
        assert!(
            !item.as_object().is_some_and(|o| o.contains_key("id")),
            "T4 failed — input item still has id: {item}"
        );
    }

    // T5: no item_reference item remains.
    for item in inputs {
        assert_ne!(
            item.get("type").and_then(|v| v.as_str()),
            Some("item_reference"),
            "T5 failed — item_reference not removed"
        );
    }

    // T6: orphaned function_call_output converted to an assistant message (NOT dropped).
    let has_assistant_with_orphan = inputs.iter().any(|item| {
        item.get("type").and_then(|v| v.as_str()) == Some("message")
            && item.get("role").and_then(|v| v.as_str()) == Some("assistant")
            && item.get("content").and_then(|c| c.as_array()).is_some_and(|arr| {
                arr.iter()
                    .any(|b| b.get("text").and_then(|t| t.as_str()) == Some("orphan result"))
            })
    });
    assert!(
        has_assistant_with_orphan,
        "T6 failed — orphan not converted to assistant message"
    );

    // Length preserved: original had 3 items, after T5 removes item_reference → 2.
    assert_eq!(inputs.len(), 2, "after T5 we expect 2 remaining items");

    // T8: text.verbosity defaulted to medium.
    assert_eq!(
        body.get("text")
            .and_then(|t| t.get("verbosity"))
            .and_then(|v| v.as_str()),
        Some("medium"),
        "T8 failed — verbosity not defaulted to medium"
    );

    // T9: reasoning set for gpt-5.1-codex family (effort=medium, summary=auto).
    assert_eq!(
        body.get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|v| v.as_str()),
        Some("medium"),
        "T9 failed — reasoning.effort not medium"
    );
    assert_eq!(
        body.get("reasoning")
            .and_then(|r| r.get("summary"))
            .and_then(|v| v.as_str()),
        Some("auto"),
        "T9 failed — reasoning.summary not auto"
    );

    // T10/T11: max_output_tokens / max_completion_tokens absent (wire type omits them).
    assert!(body.get("max_output_tokens").is_none(), "T10 failed");
    assert!(body.get("max_completion_tokens").is_none(), "T11 failed");

    // T12: model normalized — `gpt-5.1-codex` → `gpt-5.1-codex`.
    assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("gpt-5.1-codex"));

    // Drive the stream to assert wiremock's .expect(1) matcher actually matched.
    let client = make_client(server.uri());
    let mut stream = client.stream_responses(req).await.expect("stream ok");
    while let Some(next) = stream.next().await {
        next.expect("no wire error");
    }
}

// ============================================================================
// 3. Non-streaming caller synthesizes JSON via T20
// ============================================================================

#[tokio::test]
async fn responses_wire_chatgpt_backend_non_streaming_caller_synthesizes_json() {
    // Drive the stream through `collect_stream_to_json` (T20) and assert the synthesized body
    // shape matches the Responses API non-streaming shape.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_api_key_turn.sse")),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");

    let synthesized = collect_stream_to_json(stream).await.expect("collect");

    assert_eq!(synthesized.get("id").and_then(|v| v.as_str()), Some("resp_1"));
    assert_eq!(synthesized.get("object").and_then(|v| v.as_str()), Some("response"));
    let output = synthesized.get("output").and_then(|v| v.as_array()).expect("output");
    // There should be a message entry with the accumulated text.
    let message = output
        .iter()
        .find(|item| item.get("type").and_then(|v| v.as_str()) == Some("message"))
        .expect("message entry");
    let text = message
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .expect("text content");
    assert_eq!(text, "Hello");
}

// ============================================================================
// 4. Structured output — text.format.json_schema strict:true name:roz_output_schema
// ============================================================================

#[tokio::test]
async fn responses_wire_api_key_response_schema_uses_text_format_json_schema_strict_true() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"],
        "additionalProperties": false,
    });

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(serde_json::json!({
            "text": {
                "format": {
                    "type": "json_schema",
                    "strict": true,
                    "name": "roz_output_schema",
                }
            }
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_api_key_turn.sse")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut req = minimal_responses_request();
    req.text = create_text_param_for_request(None, Some(&schema));

    // Sanity-check the constructed TextControls.
    let tc: &TextControls = req.text.as_ref().expect("text controls");
    let fmt: &TextFormat = tc.format.as_ref().expect("text format");
    assert!(matches!(fmt.kind, TextFormatType::JsonSchema));
    assert!(fmt.strict);
    assert_eq!(fmt.name, "roz_output_schema");

    let client = make_client(server.uri());
    let mut stream = client.stream_responses(req).await.expect("stream ok");
    while let Some(next) = stream.next().await {
        next.expect("no wire error");
    }
}

// ============================================================================
// 5. Encrypted reasoning content preserved through the event stream
// ============================================================================

#[tokio::test]
async fn responses_wire_reasoning_encrypted_content_preserved() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(fixture("responses_reasoning_encrypted.sse")),
        )
        .mount(&server)
        .await;

    let client = make_client(server.uri());
    let mut stream = client
        .stream_responses(minimal_responses_request())
        .await
        .expect("stream ok");

    let mut saw_encrypted_reasoning = false;
    while let Some(next) = stream.next().await {
        if let ResponseEvent::OutputItemDone(ResponseItem::Reasoning { encrypted_content, .. }) =
            next.expect("no wire error")
            && encrypted_content.as_deref() == Some("ZW5jcnlwdGVkLXBheWxvYWQ=")
        {
            saw_encrypted_reasoning = true;
        }
    }
    assert!(
        saw_encrypted_reasoning,
        "encrypted reasoning content must be delivered intact through OutputItemDone"
    );
}

// ============================================================================
// Sentinel: silence the unused imports when future helpers land
// ============================================================================

#[allow(dead_code)]
fn _silence_unused_import(_r: Request) {}
