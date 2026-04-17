//! Deterministic fixture-inventory check (Plan 19-13 W7 fix).
//!
//! Replaces an earlier brittle `wc -l | grep` verify command with a const-array iteration over
//! the fixture set. If a fixture is deleted or renamed, this test fails loudly with the missing
//! name rather than relying on a line-count regex.

const EXPECTED_SSE_FIXTURES: &[&str] = &[
    "chat_single_tool_call.sse",
    "chat_multi_tool_call.sse",
    "chat_reasoning_stream.sse",
    "chat_reasoning_field.sse",
    "chat_malformed_json_structured_output.sse",
    "responses_api_key_turn.sse",
    "responses_oauth_chatgpt_turn.sse",
    "responses_reasoning_encrypted.sse",
    "ollama_single_tool_call.sse",
];

fn fixtures_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn all_sse_fixtures_present_and_nonempty() {
    let root = fixtures_root();
    for name in EXPECTED_SSE_FIXTURES {
        let path = root.join(name);
        let meta = std::fs::metadata(&path).unwrap_or_else(|e| panic!("missing fixture {name}: {e}"));
        assert!(meta.len() > 0, "empty fixture {name}");
    }
}

#[test]
fn jwt_fixture_present_and_nonempty() {
    let path = fixtures_root().join("jwt_chatgpt_account_id.jwt");
    let meta = std::fs::metadata(&path).expect("jwt fixture missing");
    assert!(meta.len() > 0, "jwt fixture is empty");
}

#[test]
fn fixtures_readme_present() {
    let path = fixtures_root().join("README.md");
    let meta = std::fs::metadata(&path).expect("README missing");
    assert!(meta.len() > 0, "README is empty");
}
