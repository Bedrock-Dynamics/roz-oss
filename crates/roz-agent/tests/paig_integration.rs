//! Integration tests that hit real model APIs via the Pydantic AI Gateway.
//!
//! These tests are `#[ignore]`d by default. Run serially to avoid PAIG rate limits:
//!   PAIG_API_KEY=<key> cargo test -p roz-agent --test paig_integration -- --ignored --test-threads=1

use std::time::Duration;

use roz_agent::model::anthropic::{AnthropicConfig, AnthropicProvider};
use roz_agent::model::gemini::{GeminiConfig, GeminiProvider};
use roz_agent::model::{CompletionRequest, Message, Model};

fn paig_api_key() -> String {
    std::env::var("PAIG_API_KEY").expect("PAIG_API_KEY must be set for these tests")
}

fn anthropic_provider() -> AnthropicProvider {
    AnthropicProvider::new(AnthropicConfig {
        gateway_url: "https://gateway-us.pydantic.dev".to_string(),
        api_key: paig_api_key(),
        model: "claude-haiku-4-5-20251001".to_string(),
        thinking: None,
        timeout: Duration::from_secs(120),
        proxy_provider: "anthropic".to_string(),
        direct_api_key: None,
    })
}

fn gemini_provider() -> GeminiProvider {
    GeminiProvider::new(GeminiConfig {
        gateway_url: "https://gateway-us.pydantic.dev".to_string(),
        api_key: paig_api_key(),
        model: "gemini-2.5-flash".to_string(),
        timeout: Duration::from_secs(120),
    })
}

fn simple_request() -> CompletionRequest {
    CompletionRequest {
        messages: vec![Message::user("Reply with exactly the word 'pong'. Nothing else.")],
        tools: vec![],
        max_tokens: 64,
        tool_choice: None,
        response_schema: None,
    }
}

#[tokio::test]
#[ignore]
async fn anthropic_complete_returns_response() {
    let provider = anthropic_provider();
    let response = provider
        .complete(&simple_request())
        .await
        .expect("complete() should succeed");

    let content = response.text();
    assert!(content.is_some(), "response should have content");
    let content = content.unwrap();
    assert!(
        content.to_lowercase().contains("pong"),
        "expected content to contain 'pong', got: {content:?}"
    );
    assert!(response.usage.input_tokens > 0, "input_tokens should be > 0");
    assert!(response.usage.output_tokens > 0, "output_tokens should be > 0");
}

#[tokio::test]
#[ignore]
async fn gemini_complete_returns_response() {
    let provider = gemini_provider();
    let response = provider
        .complete(&simple_request())
        .await
        .expect("complete() should succeed");

    let content = response.text();
    assert!(content.is_some(), "response should have content");
    let content = content.unwrap();
    assert!(
        content.to_lowercase().contains("pong"),
        "expected content to contain 'pong', got: {content:?}"
    );
    assert!(response.usage.input_tokens > 0, "input_tokens should be > 0");
}
