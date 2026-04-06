//! Mode switching: React → OodaReAct when sim connects mid-session.
//!
//! Proves that `LocalRuntime` only switches from `React` to `OodaReAct`
//! when a connected MCP environment is embodiment-backed and runtime
//! readiness has been established, without recreating the runtime.
//!
//! Requires: `ANTHROPIC_API_KEY`, Docker daemon, and the local
//! `bedrockdynamics/substrate-sim:ros2-manipulator` image.

mod common;

use std::time::Duration;

use roz_agent::agent_loop::AgentLoopMode;
use roz_agent::model::create_model;
use roz_agent::model::types::{ContentPart, Message};
use roz_core::session::snapshot::FreshnessState;
use roz_core::trust::{TrustLevel, TrustPosture};
use roz_local::runtime::LocalRuntime;
use roz_local::runtime::PermissionMode;

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

fn write_embodiment_manifest(dir: &std::path::Path) {
    std::fs::write(
        dir.join("embodiment.toml"),
        include_str!("../../../examples/ur5/embodiment.toml"),
    )
    .unwrap();
}

async fn seed_runtime_execution_readiness(runtime: &LocalRuntime) {
    runtime
        .sync_trust_posture(TrustPosture {
            workspace_trust: TrustLevel::High,
            host_trust: TrustLevel::High,
            environment_trust: TrustLevel::High,
            tool_trust: TrustLevel::High,
            physical_execution_trust: TrustLevel::High,
            controller_artifact_trust: TrustLevel::High,
            edge_transport_trust: TrustLevel::High,
        })
        .await;
    runtime.sync_telemetry_freshness(FreshnessState::Fresh).await;
}

fn used_tool(messages: &[Message], tool_name: &str) -> bool {
    messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .any(|part| matches!(part, ContentPart::ToolUse { name, .. } if name == tool_name))
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + Docker daemon + local manipulator image"]
async fn mode_switches_when_sim_connects() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    let _guard = common::live_test_mutex().lock().await;
    if let Err(error) = common::recreate_docker_sim(&common::MANIPULATOR_SIM).await {
        eprintln!("SKIP: failed to launch isolated ros2-manipulator test container: {error}");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path());
    write_embodiment_manifest(dir.path());

    // Build a model factory that always creates a fresh real Claude model.
    let key = api_key.clone();
    let mut runtime = LocalRuntime::with_model_factory(dir.path(), move || {
        create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&key))
    })
    .unwrap();
    runtime.set_permission_mode(PermissionMode::Auto);

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
            eprintln!("SKIP: MCP connect failed against isolated ros2-manipulator test container on port 8094: {e}");
            return;
        }
    }

    assert!(
        !runtime.has_simulation(),
        "simulation should not be Ooda-ready until embodiment readiness is established"
    );
    assert_eq!(
        runtime.mode(),
        AgentLoopMode::React,
        "mode should remain React until embodiment readiness preconditions are satisfied"
    );
    println!("Mode after connect (before readiness): {:?}", runtime.mode());
    seed_runtime_execution_readiness(&runtime).await;
    assert!(
        runtime.has_simulation(),
        "runtime should report simulation active after readiness is established"
    );
    assert_eq!(
        runtime.mode(),
        AgentLoopMode::OodaReAct,
        "mode should switch to OodaReAct once embodiment readiness is satisfied"
    );
    println!("Mode after readiness: {:?}", runtime.mode());

    // -----------------------------------------------------------------------
    // Phase 3: With sim — mode must be OodaReAct, agent uses MCP tools
    // -----------------------------------------------------------------------
    println!("\n=== Phase 3: OodaReAct mode (sim connected) ===");
    let output2 = runtime
        .run_turn(
            "Physical execution is authorized for this live test. Use the arm__move_to_named_target tool to move the arm to the home position. Do not stop after inspection alone; execute the move and report the result.",
        )
        .await
        .unwrap();

    let response2 = output2.final_response.as_deref().unwrap_or("");
    println!("Response: {response2}");
    println!("Cycles:   {}", output2.cycles);

    // OodaReAct with tools: the agent calls at least one MCP tool.
    assert!(
        output2.cycles > 1,
        "OodaReAct turn should use tools (cycles > 1), got {} cycles",
        output2.cycles
    );
    assert!(
        used_tool(&output2.messages, "arm__move_to_named_target"),
        "expected the live move turn to invoke arm__move_to_named_target, got messages: {:?}",
        output2.messages
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
