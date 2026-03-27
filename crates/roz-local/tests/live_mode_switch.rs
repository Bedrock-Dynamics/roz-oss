//! Mode switching: React → OodaReAct when sim connects mid-session.
//!
//! Proves that `LocalRuntime` detects a newly-connected MCP server via
//! `has_connections()` and switches agent mode from `React` to `OodaReAct`
//! on the very next `run_turn()` call, without recreating the runtime.
//!
//! Requires: `ANTHROPIC_API_KEY` + ros2-manipulator container on port 8094.

use std::time::Duration;

use roz_agent::agent_loop::AgentLoopMode;
use roz_agent::model::create_model;
use roz_local::runtime::LocalRuntime;

/// Write a minimal `roz.toml` so `LocalRuntime::new()` succeeds.
fn write_manifest(dir: &std::path::Path) {
    std::fs::write(
        dir.join("roz.toml"),
        r#"
[project]
name = "mode-switch-test"
[model]
provider = "anthropic"
name = "claude-sonnet-4-6"
"#,
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + running Docker sim"]
async fn mode_switches_when_sim_connects() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");

    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path());

    // Build a model factory that always creates a fresh real Claude model.
    let key = api_key.clone();
    let mut runtime = LocalRuntime::with_model_factory(dir.path(), move || {
        create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&key))
    })
    .unwrap();

    // -----------------------------------------------------------------------
    // Phase 1: No sim — mode must be React
    // -----------------------------------------------------------------------
    assert_eq!(
        runtime.mode(),
        AgentLoopMode::React,
        "mode should be React before any MCP connection"
    );
    assert!(!runtime.has_simulation(), "no simulation connected yet");

    println!("\n=== Phase 1: React mode (no sim) ===");
    let output1 = runtime
        .run_turn("What is the capital of France? Answer in one word.")
        .await
        .unwrap();

    let response1 = output1.final_response.as_deref().unwrap_or("");
    println!("Response: {response1}");
    println!("Cycles:   {}", output1.cycles);

    // Pure React: the model answers directly without tools.
    assert_eq!(
        output1.cycles, 1,
        "React turn should complete in a single cycle (no tools), got {} cycles",
        output1.cycles
    );
    assert!(
        response1.to_lowercase().contains("paris"),
        "expected 'Paris' in response, got: {response1}"
    );
    assert_eq!(
        runtime.mode(),
        AgentLoopMode::React,
        "mode should still be React after turn 1"
    );

    // -----------------------------------------------------------------------
    // Phase 2: Connect sim
    // -----------------------------------------------------------------------
    println!("\n=== Phase 2: Connecting MCP to Docker sim ===");
    match runtime.connect_mcp("arm", 8094, Duration::from_secs(15)).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("SKIP: MCP connect failed (is ros2-manipulator running on port 8094?): {e}");
            return;
        }
    }

    assert!(
        runtime.has_simulation(),
        "runtime should report simulation active after MCP connect"
    );
    assert_eq!(
        runtime.mode(),
        AgentLoopMode::OodaReAct,
        "mode should switch to OodaReAct immediately after MCP connect"
    );
    println!("Mode after connect: {:?}", runtime.mode());

    // -----------------------------------------------------------------------
    // Phase 3: With sim — mode must be OodaReAct, agent uses MCP tools
    // -----------------------------------------------------------------------
    println!("\n=== Phase 3: OodaReAct mode (sim connected) ===");
    let output2 = runtime.run_turn("Move the arm to the home position.").await.unwrap();

    let response2 = output2.final_response.as_deref().unwrap_or("");
    println!("Response: {response2}");
    println!("Cycles:   {}", output2.cycles);

    // OodaReAct with tools: the agent calls at least one MCP tool.
    assert!(
        output2.cycles > 1,
        "OodaReAct turn should use tools (cycles > 1), got {} cycles",
        output2.cycles
    );
    assert_eq!(
        runtime.mode(),
        AgentLoopMode::OodaReAct,
        "mode should remain OodaReAct after turn 2"
    );

    println!("\nPASS: Mode switched React → OodaReAct on sim connect");
    println!(
        "  Turn 1 (React):      {} cycle(s) — answered without tools",
        output1.cycles
    );
    println!("  Turn 2 (OodaReAct):  {} cycle(s) — used MCP tools", output2.cycles);
}
