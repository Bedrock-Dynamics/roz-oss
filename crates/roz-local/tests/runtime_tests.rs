use roz_agent::model::types::{CompletionResponse, ContentPart, MockModel, ModelCapability, StopReason, TokenUsage};
use roz_local::runtime::LocalRuntime;
use std::io::Write;

/// Helper: write a minimal roz.toml to a temp directory.
fn write_manifest(dir: &std::path::Path) {
    let mut f = std::fs::File::create(dir.join("roz.toml")).unwrap();
    writeln!(
        f,
        r#"
[project]
name = "test"
[model]
provider = "ollama"
name = "llama3.1"
"#
    )
    .unwrap();
}

/// Helper: create a `MockModel` that returns simple text responses.
fn text_mock(responses: Vec<&str>) -> MockModel {
    MockModel::new(
        vec![ModelCapability::TextReasoning],
        responses
            .into_iter()
            .map(|text| CompletionResponse {
                parts: vec![ContentPart::Text { text: text.to_string() }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            })
            .collect(),
    )
}

#[tokio::test]
async fn runtime_run_turn_returns_response() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path());

    let mut rt =
        LocalRuntime::with_model_factory(dir.path(), || Ok(Box::new(text_mock(vec!["Hello from mock model!"]))))
            .unwrap();

    let output = rt.run_turn("hello").await.unwrap();

    // The agent loop should produce at least one assistant message
    let assistant_text: String = output
        .messages
        .iter()
        .filter(|m| m.role == roz_agent::model::types::MessageRole::Assistant)
        .filter_map(|m| m.text())
        .collect();
    assert!(
        assistant_text.contains("Hello from mock model"),
        "expected mock response, got: {assistant_text}"
    );
}

#[tokio::test]
async fn runtime_session_persists_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path());

    let mut rt = LocalRuntime::with_model_factory(dir.path(), || Ok(Box::new(text_mock(vec!["response"])))).unwrap();

    let session_id = rt.session_id().to_string();
    rt.run_turn("hello").await.unwrap();

    // Session file should exist on disk
    let session_path = dir
        .path()
        .join(".roz")
        .join("sessions")
        .join(format!("{session_id}.json"));
    assert!(session_path.exists(), "session file should be saved to disk");

    let content = std::fs::read_to_string(session_path).unwrap();
    assert!(!content.is_empty(), "session file should not be empty");
}

#[tokio::test]
async fn runtime_new_fails_without_manifest() {
    let dir = tempfile::tempdir().unwrap();
    // No roz.toml
    let result = LocalRuntime::new(dir.path());
    assert!(result.is_err(), "should fail without roz.toml");
}

#[tokio::test]
async fn runtime_agents_md_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path());
    std::fs::write(dir.path().join("AGENTS.md"), "You are a helpful drone assistant.").unwrap();

    // Just verify it loads without error — the AGENTS.md content
    // goes into the system prompt which we can't easily inspect from here
    let mut rt = LocalRuntime::with_model_factory(dir.path(), || Ok(Box::new(text_mock(vec!["ok"])))).unwrap();
    let output = rt.run_turn("hello").await.unwrap();
    assert!(!output.messages.is_empty());
}

#[test]
fn runtime_model_name_routing() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path());
    let rt = LocalRuntime::new(dir.path()).unwrap();
    assert_eq!(rt.model_name(), "ollama/llama3.1");
}
