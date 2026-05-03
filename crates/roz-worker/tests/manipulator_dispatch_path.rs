//! Phase 26.10 Plan 09 (FW-07 / Codex H4) — manipulator production-parity gate.
//!
//! Exercises the full agent/task production path:
//!   1. Build the worker `ToolDispatcher` with the three lifecycle tools the
//!      worker boot path registers (`promote_controller`, `controller_status`,
//!      `stop_controller`) — same shape as
//!      `crates/roz-worker/tests/promote_controller_registered.rs`.
//!   2. Spawn `CopperHandle` against the fake manipulator IO backend
//!      (`fake_manipulator_pair`) — Plan 08 (FW-07 part 1) wired this in.
//!   3. Wire `ToolContext.extensions` with the Copper `cmd_tx`, the shared
//!      `ControllerState` arc-swap, the `ControlInterfaceManifest`, and the
//!      compiled `EmbodimentRuntime` from a manipulator-class fixture.
//!   4. Drive `promote_controller` execute() with a non-zero `live-controller`
//!      WAT artifact that emits joint velocity through the WASM tick contract.
//!   5. Simulate external runtime rollout authority with `PromoteActive`.
//!   6. Confirm `controller_status` reports `running == true` after promotion
//!      and the fake manipulator joint position advances from WASM output.
//!   7. Drive `stop_controller`; confirm subsequent `controller_status` shows
//!      the controller halted.
//!
//! **Codex H4 fix — production-parity gate, NOT `#[ignore]`-gated.** The plan's
//! checker iter-1 enforcement bars `#[ignore]` on this specific test so any
//! `cargo test -p roz-worker --test manipulator_dispatch_path --features
//! test-fixtures` invocation triggers the H4 gate. The test file location is
//! `roz-worker/tests/` (not the plan-spec `roz-copper/tests/`) because the
//! lifecycle tools live in `roz-worker` and `roz-copper` cannot dev-dep
//! `roz-worker` (cycle). The H4 *intent* — default-runnable, exercises the
//! full dispatch path — is preserved.
//!
//! Live-matrix wiring: `scripts/run_live_e2e_matrix.sh::run_deterministic`
//! invokes this test by name in the manipulator row.

#![cfg(feature = "test-fixtures")]
#![allow(clippy::pedantic, clippy::nursery, reason = "test-only style/complexity lints")]

use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::mpsc;

use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher, TypedToolExecutor};
use roz_copper::channels::{ControllerCommand, ControllerState};
use roz_copper::deployment_manager::DeploymentManager;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::policy::new_hot_policy;
use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::{
    BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
};
use roz_core::tools::ToolCategory;

/// Non-zero `live-controller` WAT — emits a constant 0.4 rad/s command vector
/// and completes the WIT tick contract. This keeps the test production-path:
/// promote_controller componentizes/verifies the WAT, Copper ticks it, and the
/// fake manipulator backend must observe actual simulated motion.
const MOTION_LIVE_CONTROLLER_WAT: &str = r#"
    (module
        (type (func (result i32)))
        (type (func (param i32) (result i32)))
        (type (func (param i32)))
        (type (func (param i32 i32 i32 i32) (result i32)))
        (type (func))
        (import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode" (func $current_execution_mode (type 0)))
        (memory (export "cm32p2_memory") 1)
        (global $heap (mut i32) (i32.const 1024))
        (data (i32.const 0) "\40\00\00\00\01\00\00\00")
        (data (i32.const 64) "\9a\99\99\99\99\99\d9\3f")
        (func (export "cm32p2|bedrock:controller/control@1|process") (type 1) (param $input i32) (result i32)
            (i32.const 0)
        )
        (func (export "cm32p2|bedrock:controller/control@1|process_post") (type 2) (param $result i32)
            (global.set $heap (i32.const 1024))
        )
        (func (export "cm32p2_realloc") (type 3) (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
            (local $ptr i32)
            global.get $heap
            local.get $align
            i32.const 1
            i32.sub
            i32.add
            local.get $align
            i32.const 1
            i32.sub
            i32.const -1
            i32.xor
            i32.and
            local.tee $ptr
            local.get $new_size
            i32.add
            global.set $heap
            local.get $ptr
        )
        (func (export "cm32p2_initialize") (type 4))
    )
"#;

fn manipulator_control_manifest() -> ControlInterfaceManifest {
    let mut cm = ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: vec![ControlChannelDef {
            name: "joint0/velocity".into(),
            interface_type: CommandInterfaceType::JointVelocity,
            units: "rad/s".into(),
            frame_id: "link_0".into(),
        }],
        bindings: vec![ChannelBinding {
            physical_name: "joint_0".into(),
            channel_index: 0,
            binding_type: BindingType::JointVelocity,
            frame_id: "link_0".into(),
            units: "rad/s".into(),
            semantic_role: None,
        }],
    };
    cm.stamp_digest();
    cm
}

/// Build a `ToolContext` with all the extensions the worker boot path attaches
/// at OodaReAct task start. Mirrors the worker's `execute_task` setup so the
/// production tools see the same surface they see in production.
fn manipulator_tool_context(
    cmd_tx: mpsc::Sender<ControllerCommand>,
    state_handle: Arc<ArcSwap<ControllerState>>,
    control_manifest: ControlInterfaceManifest,
    embodiment_runtime: EmbodimentRuntime,
) -> ToolContext {
    let mut ext = Extensions::default();
    ext.insert(cmd_tx);
    ext.insert(state_handle);
    ext.insert(control_manifest);
    ext.insert(embodiment_runtime);
    ToolContext {
        task_id: "manipulator-dispatch-path".into(),
        tenant_id: "phase-26.10-09".into(),
        call_id: "h4-production-parity-gate".into(),
        extensions: ext,
    }
}

/// **Codex H4 production-parity gate** — exercises the full agent/task path:
/// dispatch → tool registration → promote_controller → controller_status →
/// stop_controller. NOT `#[ignore]`-gated; default-runnable so any
/// `cargo test -p roz-worker --test manipulator_dispatch_path --features
/// test-fixtures` triggers it.
#[tokio::test]
async fn manipulator_dispatch_through_promote_controller_path() {
    // Step 1 — build the dispatcher with the worker boot tool set.
    let cm = manipulator_control_manifest();
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(
        Box::new(roz_worker::tools::promote_controller::PromoteControllerTool::new(&cm)),
        ToolCategory::Physical,
    );
    dispatcher.register_with_category(
        Box::new(roz_worker::tools::stop_controller::StopControllerTool),
        ToolCategory::Physical,
    );
    dispatcher.register_with_category(
        Box::new(roz_worker::tools::controller_status::ControllerStatusTool),
        ToolCategory::Physical,
    );
    let names = dispatcher.tool_names();
    assert!(
        names.iter().any(|n| n == "promote_controller"),
        "promote_controller must register on the dispatcher"
    );
    assert!(
        names.iter().any(|n| n == "controller_status"),
        "controller_status must register on the dispatcher"
    );
    assert!(
        names.iter().any(|n| n == "stop_controller"),
        "stop_controller must register on the dispatcher"
    );

    // Step 2 — boot Copper with the fake manipulator IO backend (Plan 08 wiring).
    let runtime = roz_core::embodiment::test_fixtures::manipulator_runtime(2, 1.0, 3.14);
    let (actuator, sensor) = roz_copper::fake_manipulator::fake_manipulator_pair(&runtime);
    let actuator = Arc::new(actuator);
    let actuator_probe = Arc::clone(&actuator);
    let policy = new_hot_policy();
    let bp = Arc::new(AtomicU8::new(0));
    let deployment_manager = DeploymentManager::with_rollout_policy(false, true, true, 1, 1, 10_000, 10_000, u64::MAX);
    let handle = CopperHandle::spawn_with_io_and_deployment_manager_and_wiring(
        1.5,
        Some(actuator as Arc<dyn ActuatorSink>),
        Some(Box::new(sensor)),
        deployment_manager,
        Some(policy),
        Some(bp),
        None,
        roz_copper::latch::LatchState::Run,
    );

    // Step 3 — hold-out the cmd_tx + state handle on the test side; produce
    // ToolContext extensions that mirror the worker boot path.
    let cmd_tx = handle.cmd_tx();
    let state = Arc::clone(handle.state());
    let ctx = manipulator_tool_context(cmd_tx.clone(), Arc::clone(&state), cm.clone(), runtime.clone());

    // Step 4 — drive the production `promote_controller` tool through its
    // `TypedToolExecutor::execute()` entry point. This is the load-bearing
    // H4 contract: the tool's body componentizes WAT, runs verify_wasm
    // (100 verification ticks under production safety limits), invokes
    // run_lifecycle (artifact stage progression + verifier verdict), checks
    // the runtime authority (rejects synthesized embodiment runtimes), then
    // sends LoadArtifact via the cmd_tx pulled from `ToolContext.extensions`.
    //
    // Codex H4 enforcement (T-26.10-09-07 mitigation): we call execute() on
    // the actual production tool. We do NOT bypass to a hand-built
    // ControllerArtifact + cmd_tx.send() — that would re-create the original
    // H4 failure shape (test passing without exercising promote_controller).
    let promote_tool = roz_worker::tools::promote_controller::PromoteControllerTool::new(&cm);
    let promote_result = TypedToolExecutor::execute(
        &promote_tool,
        roz_worker::tools::promote_controller::PromoteControllerInput {
            code: MOTION_LIVE_CONTROLLER_WAT.to_string(),
        },
        &ctx,
    )
    .await
    .expect("promote_controller.execute() must complete (verify_wasm + run_lifecycle + LoadArtifact)");
    assert!(
        promote_result.is_success(),
        "promote_controller must report success against the manipulator fixture; got {:?}",
        promote_result
    );
    assert_eq!(
        promote_result.output["registered_state"], "VerifiedOnly",
        "promote_controller registers the controller but external runtime authority must authorize actuation"
    );
    // Step 5 — model the runtime's rollout authority. `promote_controller`
    // itself deliberately stops at VerifiedOnly; production task harnesses send
    // `PromoteActive` only when the runtime policy allows it.
    cmd_tx
        .send(ControllerCommand::PromoteActive)
        .await
        .expect("external rollout authority sends PromoteActive");
    // Allow VerifiedOnly -> Canary -> Active progression and subsequent
    // actuation through the fake manipulator.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Step 5 — drive controller_status via the dispatcher; assert the live
    // controller is running. This is the agent-visible surface: substrate-ide /
    // OodaReAct loops call this same tool at this same name.
    let status_tool = roz_worker::tools::controller_status::ControllerStatusTool;
    let status = TypedToolExecutor::execute(
        &status_tool,
        roz_worker::tools::controller_status::ControllerStatusInput {},
        &ctx,
    )
    .await
    .expect("controller_status must execute against the configured ToolContext");
    assert!(status.is_success(), "controller_status must report success");
    assert_eq!(
        status.output["running"], true,
        "controller must report running after LoadArtifact + 300ms warmup"
    );
    let last_tick = status.output["last_tick"].as_u64().unwrap_or(0);
    assert!(
        last_tick > 0,
        "controller must have ticked at least once (got last_tick={last_tick})"
    );
    let mut joint0 = 0.0;
    for _ in 0..20 {
        let joint_positions = actuator_probe.joint_positions_snapshot();
        joint0 = joint_positions.first().copied().unwrap_or(0.0);
        if joint0 > 0.05 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        joint0 > 0.05,
        "WASM-emitted joint velocity must move the fake manipulator through Copper; joint0={joint0}"
    );

    // Step 6 — drive stop_controller via the dispatcher. Subsequent
    // controller_status must report halted.
    let stop_tool = roz_worker::tools::stop_controller::StopControllerTool;
    let stop_result = TypedToolExecutor::execute(
        &stop_tool,
        roz_worker::tools::stop_controller::StopControllerInput {},
        &ctx,
    )
    .await
    .expect("stop_controller must execute");
    assert!(stop_result.is_success(), "stop_controller must report success");
    tokio::time::sleep(Duration::from_millis(80)).await;

    let post_status = TypedToolExecutor::execute(
        &status_tool,
        roz_worker::tools::controller_status::ControllerStatusInput {},
        &ctx,
    )
    .await
    .expect("controller_status post-halt must execute");
    assert_eq!(
        post_status.output["running"], false,
        "controller must report halted after stop_controller"
    );

    // Step 7 — clean shutdown.
    handle.shutdown().await;
}
