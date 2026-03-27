//! Vertical integration tests for roz-cli.
//!
//! These tests exercise production code paths across module boundaries:
//! session persistence, project context loading, and provider detection.

use std::fs;
use std::io::Write;

use roz_cli::tui::context::load_project_context_from;
use roz_cli::tui::provider::{Provider, ProviderConfig};
use roz_cli::tui::session::{Session, SessionEntry};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------

#[test]
fn session_persist_and_resume() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // Create a new session and write two entries.
    let session = Session::new_in(&sessions).unwrap();
    let id = session.id.clone();

    session.append(&SessionEntry::now("user", "What is 2+2?"));
    session.append(&SessionEntry::now("assistant", "4").with_usage("claude-sonnet-4-6", 12, 3));

    // Load the same session by ID and verify entries survived the round-trip.
    let resumed = Session::load_from(&id, &sessions).unwrap();
    let entries = resumed.entries().unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].role, "user");
    assert_eq!(entries[0].content, "What is 2+2?");
    assert_eq!(entries[1].role, "assistant");
    assert_eq!(entries[1].content, "4");
    assert_eq!(entries[1].model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(entries[1].input_tokens, Some(12));
    assert_eq!(entries[1].output_tokens, Some(3));

    // load_latest_from should return this same session (only one exists).
    let latest = Session::load_latest_from(&sessions).unwrap();
    assert_eq!(latest.id, id);
}

// ---------------------------------------------------------------------------
// Project context
// ---------------------------------------------------------------------------

#[test]
fn project_context_loads_agents_md() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("AGENTS.md"),
        "You are a helpful robot assistant.\nAlways confirm before moving.",
    )
    .unwrap();

    let ctx = load_project_context_from(dir.path()).expect("should load AGENTS.md");
    assert!(ctx.contains("AGENTS.md"), "header should reference file name");
    assert!(ctx.contains("helpful robot assistant"), "content should be present");
}

#[test]
fn project_context_loads_robot_md() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("ROBOT.md"), "UR5e cobot\nPayload: 5 kg\nReach: 850 mm").unwrap();

    let ctx = load_project_context_from(dir.path()).expect("should load ROBOT.md");
    assert!(ctx.contains("ROBOT.md"), "header should reference file name");
    assert!(ctx.contains("UR5e cobot"), "content should be present");
    assert!(ctx.contains("Payload: 5 kg"), "multi-line content preserved");
}

// ---------------------------------------------------------------------------
// Provider detection
// ---------------------------------------------------------------------------

#[test]
fn detect_bare_model_with_roz_sk_routes_to_cloud() {
    // A bare model name (no "provider/" prefix) with a roz_sk_ API key
    // should auto-detect Provider::Cloud, not Anthropic.
    let config = ProviderConfig::detect(
        Some("claude-sonnet-4-6"), // bare model, no provider prefix
        Some("roz_sk_test_key_abc123"),
        None,
    );
    assert_eq!(
        config.provider,
        Provider::Cloud,
        "roz_sk_ key with bare model should route to Cloud"
    );
    assert_eq!(config.model, "claude-sonnet-4-6");
    assert_eq!(config.api_key.as_deref(), Some("roz_sk_test_key_abc123"));
    assert_eq!(config.api_url, "https://roz-api.fly.dev");
}

#[test]
fn both_api_keys_set_cloud_wins() {
    // When roz_sk_ key is present with a bare model ref from roz.toml (no explicit --model),
    // Cloud should win regardless of what else is set.
    // This differs from detect_bare_model_with_roz_sk_routes_to_cloud which supplies
    // the model via explicit_model (first param); here the model resolves via roz_toml_model.
    let config = ProviderConfig::detect(None, Some("roz_sk_test_key"), Some("claude-sonnet-4-6"));
    assert_eq!(
        config.provider,
        Provider::Cloud,
        "roz_sk_ should always route to Cloud with bare ref"
    );
}

// ---------------------------------------------------------------------------
// Session compaction
// ---------------------------------------------------------------------------

#[test]
fn session_compact_preserves_recent_entries() {
    use roz_cli::tui::session::compact_entries;

    let dir = TempDir::new().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let session = Session::new_in(&sessions_dir).unwrap();

    // Add 8 entries
    for i in 0..8 {
        session.append(&SessionEntry::now(
            if i % 2 == 0 { "user" } else { "assistant" },
            &format!("message {i}"),
        ));
    }

    // Compact
    let compacted = compact_entries(&session, None).unwrap();
    assert_eq!(compacted, 6, "should compact 6 entries (keep last 2)");

    // Verify remaining entries
    let entries = session.entries().unwrap();
    assert_eq!(entries.len(), 3, "should have 3 entries: 1 summary + 2 kept");

    // First entry is the summary
    assert_eq!(entries[0].role, "system");
    assert!(
        entries[0].content.contains("compacted"),
        "summary should mention compaction"
    );

    // Last 2 are the original entries 6 and 7
    assert_eq!(entries[1].content, "message 6");
    assert_eq!(entries[2].content, "message 7");
}

// ---------------------------------------------------------------------------
// robot.toml -> system prompt
// ---------------------------------------------------------------------------

#[test]
fn robot_toml_generates_system_prompt() {
    let toml_str = r#"
[robot]
name = "test-arm"
description = "A 6-DOF test arm"

[[capabilities]]
name = "arm"
type = "joint_group"
actions = ["move_joint", "set_velocity"]

[[sensors]]
name = "imu"
type = "imu"
data = ["orientation"]
rate_hz = 100

[safety]
e_stop_behavior = "hold_position"
max_contact_force_n = 80.0
"#;
    let manifest: roz_copper::manifest::RobotManifest = toml::from_str(toml_str).unwrap();
    let prompt = manifest.to_system_prompt();

    assert!(prompt.contains("test-arm"), "prompt should contain robot name");
    assert!(prompt.contains("arm"), "prompt should contain capability name");
    assert!(prompt.contains("imu"), "prompt should contain sensor name");
    assert!(prompt.contains("80"), "prompt should contain force limit");
    assert!(
        prompt.contains("hold_position"),
        "prompt should contain e-stop behavior"
    );
}

// ---------------------------------------------------------------------------
// Corrupted JSONL resilience
// ---------------------------------------------------------------------------

#[test]
fn session_handles_corrupted_jsonl() {
    let dir = TempDir::new().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let session = Session::new_in(&sessions_dir).unwrap();

    // Write valid entries
    session.append(&SessionEntry::now("user", "hello"));
    session.append(&SessionEntry::now("assistant", "hi"));

    // Manually append garbage to the JSONL file
    let path = sessions_dir.join(format!("{}.jsonl", session.id));
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{{broken json line").unwrap();

    // Write another valid entry after the corruption
    session.append(&SessionEntry::now("user", "still works?"));

    // Reading should not crash
    let result = session.entries();
    // Current behavior: may error on bad line or skip it.
    // Either is acceptable -- the key is NO PANIC.
    match result {
        Ok(entries) => {
            // If it skips bad lines, we should get at least the valid ones
            assert!(entries.len() >= 2, "should recover some valid entries");
        }
        Err(e) => {
            // If it errors, the error should be about parsing, not a panic.
            // serde_json errors mention "key must be a string", "expected", etc.
            let msg = e.to_string();
            assert!(
                msg.contains("key must be") || msg.contains("expected") || msg.contains("parse") || msg.contains("EOF"),
                "error should mention JSON parse failure: {msg}"
            );
        }
    }
}
