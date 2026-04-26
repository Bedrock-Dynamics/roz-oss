//! Golden OpenClaw-inspired manipulator vertical test.
//!
//! This validates Roz's manipulator-class path with an OpenClaw-inspired fake
//! backend. It does not claim upstream OpenClaw hardware behavior or protocol
//! compatibility.

#![cfg(feature = "test-fixtures")]
#![allow(
    clippy::pedantic,
    clippy::nursery,
    reason = "vertical acceptance tests favor explicit setup and assertions"
)]

use std::time::Duration;

use roz_agent::model::types::{CompletionResponse, ContentPart, MockModel, ModelCapability, StopReason, TokenUsage};
use roz_copper::policy::{CopperEnforcementMode, CopperPolicy};
use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::{
    BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
};
use roz_core::embodiment::model::EmbodimentFamily;
use roz_core::session::event::SessionEvent;
use roz_nats::dispatch::{ExecutionMode, TaskInvocation, TaskTerminalStatus};
use roz_worker::task_invocation_harness::{
    WorkerTaskHarnessOptions, WorkerTaskHarnessResult, run_worker_task_invocation_for_tests,
};
use serde_json::json;
use uuid::Uuid;

fn openclaw_control_manifest() -> ControlInterfaceManifest {
    let mut manifest = ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: (0..2)
            .map(|index| ControlChannelDef {
                name: format!("j{index}/velocity"),
                interface_type: CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: format!("link_{index}"),
            })
            .collect(),
        bindings: (0..2)
            .map(|index| ChannelBinding {
                physical_name: format!("j{index}"),
                channel_index: index,
                binding_type: BindingType::JointVelocity,
                frame_id: format!("link_{index}"),
                units: "rad/s".into(),
                semantic_role: None,
            })
            .collect(),
    };
    manifest.stamp_digest();
    manifest
}

fn openclaw_runtime(family_id: &str, manifest: &ControlInterfaceManifest) -> EmbodimentRuntime {
    let mut runtime = roz_core::embodiment::test_fixtures::manipulator_runtime(2, 1.0, 3.14);
    runtime.model.embodiment_family = Some(EmbodimentFamily {
        family_id: family_id.to_string(),
        description: "Roz OpenClaw-inspired manipulator fixture".into(),
    });
    runtime.model.channel_bindings = manifest.bindings.clone();
    runtime.model.stamp_digest();
    EmbodimentRuntime::compile(runtime.model, runtime.calibration, runtime.safety_overlay)
}

fn invocation_for(family_id: &str) -> (Uuid, TaskInvocation) {
    let manifest = openclaw_control_manifest();
    let runtime = openclaw_runtime(family_id, &manifest);
    let task_id = Uuid::new_v4();
    let mut invocation = TaskInvocation::new(
        task_id,
        Uuid::new_v4().to_string(),
        "Move the OpenClaw-inspired manipulator through the worker task path".into(),
        Uuid::new_v4(),
        None,
        Uuid::new_v4(),
        30,
        ExecutionMode::OodaReAct,
        None,
        "http://127.0.0.1:9080".into(),
        None,
        Vec::new(),
        Some(manifest),
        None,
        Some(0.0),
        Some(1.0),
    );
    invocation.embodiment_runtime = Some(runtime);
    assert_eq!(invocation.mode, ExecutionMode::OodaReAct);
    assert!(invocation.control_interface_manifest.is_some());
    assert!(invocation.embodiment_runtime.is_some());
    (task_id, invocation)
}

fn bytes_to_wat_escape(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn live_controller_wat(values: &[f64]) -> String {
    let mut result_record = Vec::new();
    result_record.extend_from_slice(&(64u32).to_le_bytes());
    result_record.extend_from_slice(&(values.len() as u32).to_le_bytes());
    let result_record = bytes_to_wat_escape(&result_record);
    let value_bytes: Vec<u8> = values.iter().flat_map(|value| value.to_le_bytes()).collect();
    let value_bytes = bytes_to_wat_escape(&value_bytes);
    format!(
        r#"(module
          (type (func (result i32)))
          (type (func (param i32) (result i32)))
          (type (func (param i32)))
          (type (func (param i32 i32 i32 i32) (result i32)))
          (type (func))
          (import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode" (func $current_execution_mode (type 0)))
          (memory (export "cm32p2_memory") 1)
          (global $heap (mut i32) (i32.const 1024))
          (data (i32.const 0) "{result_record}")
          (data (i32.const 64) "{value_bytes}")
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
        )"#
    )
}

fn scripted_model(values: &[f64]) -> Box<dyn roz_agent::model::Model> {
    Box::new(MockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolUse {
                    id: "call_promote_openclaw".into(),
                    name: "promote_controller".into(),
                    input: json!({ "code": live_controller_wat(values) }),
                }],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 200,
                    output_tokens: 100,
                    ..Default::default()
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text {
                    text: "OpenClaw-inspired controller registered.".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 250,
                    output_tokens: 20,
                    ..Default::default()
                },
            },
        ],
    ))
}

fn policy(max_angular_rad_per_s: f64) -> CopperPolicy {
    CopperPolicy {
        max_linear_m_per_s: 10.0,
        max_angular_rad_per_s,
        max_force_newtons: 100.0,
        enforcement_mode: CopperEnforcementMode::Clamp,
    }
}

async fn run_openclaw_case(
    family_id: &str,
    values: &[f64],
    options: WorkerTaskHarnessOptions,
) -> (Uuid, WorkerTaskHarnessResult) {
    let (task_id, invocation) = invocation_for(family_id);
    let result = run_worker_task_invocation_for_tests(invocation, scripted_model(values), options).await;
    (task_id, result)
}

async fn wait_for_motion(
    result: &mut WorkerTaskHarnessResult,
) -> roz_worker::physical_runtime::FakeOpenclawObservedState {
    let observation = result
        .openclaw_observation
        .clone()
        .expect("OpenClaw-inspired run must expose fake backend observation");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            result.drain_session_events();
            let snapshot = observation.snapshot();
            let positions_changed = snapshot.joint_positions.iter().any(|value| value.abs() > 1e-6);
            let velocities_changed = snapshot.joint_velocities.iter().any(|value| value.abs() > 1e-6);
            if snapshot.command_count > 0 && (positions_changed || velocities_changed) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fake backend state should change within bounded timeout")
}

async fn wait_for_running(result: &mut WorkerTaskHarnessResult, expected: bool) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            result.drain_session_events();
            let status = result
                .dispatch_physical_tool("controller_status", json!({}))
                .await
                .expect("physical dispatcher should be available");
            if status.is_success() && status.output["running"].as_bool() == Some(expected) {
                return status.output;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("controller_status should reach expected running state")
}

fn has_session_evidence(result: &WorkerTaskHarnessResult) -> bool {
    result.session_events.iter().any(|event| {
        matches!(
            event.event,
            SessionEvent::ToolCallFinished { .. } | SessionEvent::SafetyIntervention { .. }
        )
    })
}

fn assert_promote_evidence(task_id: Uuid, result: &WorkerTaskHarnessResult) {
    assert!(
        has_session_evidence(result),
        "expected ToolCallFinished or SafetyIntervention evidence, got {:?}",
        result.session_events
    );
    assert!(
        result.tool_evidence.iter().any(|evidence| {
            evidence.task_id == task_id.to_string()
                && evidence.tool_name == "promote_controller"
                && evidence.lifecycle == "finished"
        }),
        "expected task-scoped promote_controller lifecycle telemetry, got {:?}",
        result.tool_evidence
    );
}

#[tokio::test]
async fn openclaw_task_invocation_agent_to_worker_to_fake_backend_records_state_telemetry_and_session_evidence() {
    let (task_id, mut result) = run_openclaw_case(
        "openclaw",
        &[0.6, -0.35],
        WorkerTaskHarnessOptions {
            copper_policy: policy(10.0),
            ..WorkerTaskHarnessOptions::default()
        },
    )
    .await;

    assert_eq!(result.terminal_status, TaskTerminalStatus::Succeeded);
    assert!(
        result
            .dispatcher_tool_names
            .iter()
            .any(|name| name == "promote_controller")
    );
    assert!(
        result
            .dispatcher_tool_names
            .iter()
            .any(|name| name == "controller_status")
    );
    assert!(
        result
            .dispatcher_tool_names
            .iter()
            .any(|name| name == "stop_controller")
    );

    let snapshot = wait_for_motion(&mut result).await;
    assert!(
        snapshot.joint_positions.iter().any(|value| value.abs() > 1e-6)
            || snapshot.joint_velocities.iter().any(|value| value.abs() > 1e-6),
        "fake backend state did not change: {snapshot:?}"
    );

    let running = wait_for_running(&mut result, true).await;
    assert_eq!(running["running"], true, "controller_status should report running");

    let stop = result
        .dispatch_physical_tool("stop_controller", json!({}))
        .await
        .expect("physical dispatcher should be available");
    assert!(stop.is_success(), "stop_controller should succeed: {stop:?}");
    let stopped = wait_for_running(&mut result, false).await;
    assert_eq!(stopped["running"], false, "controller_status should report stopped");

    result.drain_session_events();
    assert_promote_evidence(task_id, &result);
    result.shutdown().await;
}

#[tokio::test]
async fn missing_openclaw_backend_returns_structured_failure_from_task_invocation() {
    let (task_id, result) = run_openclaw_case(
        "unknown-openclaw-family",
        &[0.6, -0.35],
        WorkerTaskHarnessOptions::default(),
    )
    .await;

    assert_eq!(result.terminal_status, TaskTerminalStatus::Failed);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no IoFactory for embodiment_family")),
        "missing backend should return structured no-IoFactory failure, got {:?}",
        result.error
    );
    assert!(
        result
            .tool_evidence
            .iter()
            .any(|evidence| evidence.task_id == task_id.to_string()),
        "failure evidence should still be task-scoped: {:?}",
        result.tool_evidence
    );
}

#[tokio::test]
async fn restrictive_safety_prevents_unsafe_openclaw_output_from_task_invocation() {
    let cap = 0.2;
    let (permissive_task_id, mut permissive) = run_openclaw_case(
        "openclaw",
        &[0.8, -0.75],
        WorkerTaskHarnessOptions {
            copper_policy: policy(10.0),
            ..WorkerTaskHarnessOptions::default()
        },
    )
    .await;
    let permissive_snapshot = wait_for_motion(&mut permissive).await;
    let permissive_max = permissive_snapshot
        .joint_velocities
        .iter()
        .map(|value| value.abs())
        .fold(0.0, f64::max);
    permissive.drain_session_events();
    assert_promote_evidence(permissive_task_id, &permissive);
    assert!(
        permissive_max > cap + 1e-6,
        "permissive variant should move above restrictive cap; max={permissive_max}"
    );
    permissive.shutdown().await;

    let (restricted_task_id, mut restricted) = run_openclaw_case(
        "openclaw",
        &[0.8, -0.75],
        WorkerTaskHarnessOptions {
            copper_policy: policy(cap),
            ..WorkerTaskHarnessOptions::default()
        },
    )
    .await;
    let restricted_snapshot = wait_for_motion(&mut restricted).await;
    let restricted_max = restricted_snapshot
        .joint_velocities
        .iter()
        .map(|value| value.abs())
        .fold(0.0, f64::max);
    restricted.drain_session_events();
    assert_promote_evidence(restricted_task_id, &restricted);
    assert!(
        restricted_max <= cap + 1e-6,
        "restrictive policy should clamp actuator-observed velocity to {cap}, got {restricted_max}"
    );
    assert!(
        restricted_snapshot
            .joint_positions
            .iter()
            .any(|value| value.abs() > 1e-6),
        "restricted policy should still allow bounded motion: {restricted_snapshot:?}"
    );
    restricted.shutdown().await;
}
