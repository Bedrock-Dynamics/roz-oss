//! Test-fixtures worker task harness.
//!
//! This module deliberately stays thin: it builds the same dispatcher,
//! extensions, prompt shell, AgentLoop, and physical runtime seam used by the
//! worker OodaReAct task path, but lets tests supply a deterministic model.

#![cfg(feature = "test-fixtures")]

use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInputSeed, AgentLoop};
use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_agent::model::Model;
use roz_agent::safety::SafetyStack;
use roz_agent::session_runtime::{EventEmitter, SessionRuntimeEventHook};
use roz_copper::channels::{ControllerCommand, ControllerState};
use roz_copper::policy::{CopperEnforcementMode, CopperPolicy};
use roz_core::session::event::{EventEnvelope, SessionEvent};
use roz_core::tools::{ToolCall, ToolCategory, ToolResult};
use roz_nats::dispatch::{ExecutionMode, TaskInvocation, TaskTerminalStatus};
use tokio::sync::mpsc;

use crate::dispatch::{build_prompt_state, build_runtime_shell_input, build_turn_input, effective_cognition_mode};
use crate::physical_runtime::{
    FakeOpenclawObservation, PhysicalRuntimeConfig, PhysicalRuntimeHandle, PhysicalRuntimeRolloutAuthority,
    spawn_physical_runtime,
};

/// Inputs that let tests shape policy and rollout authority without bypassing
/// the worker task/agent/tool path.
#[derive(Clone)]
pub struct WorkerTaskHarnessOptions {
    pub max_velocity: f64,
    pub copper_policy: CopperPolicy,
    pub authorize_rollout: bool,
    pub auto_promote_after_agent: bool,
}

impl Default for WorkerTaskHarnessOptions {
    fn default() -> Self {
        Self {
            max_velocity: 1.5,
            copper_policy: CopperPolicy {
                max_linear_m_per_s: 10.0,
                max_angular_rad_per_s: 10.0,
                max_force_newtons: 100.0,
                enforcement_mode: CopperEnforcementMode::Clamp,
            },
            authorize_rollout: true,
            auto_promote_after_agent: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerTaskToolEvidence {
    pub task_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub lifecycle: String,
}

pub struct WorkerTaskHarnessResult {
    pub terminal_status: TaskTerminalStatus,
    pub error: Option<String>,
    pub output: Option<roz_agent::agent_loop::AgentOutput>,
    pub session_events: Vec<EventEnvelope>,
    pub tool_evidence: Vec<WorkerTaskToolEvidence>,
    pub dispatcher_tool_names: Vec<String>,
    pub final_controller_state: Option<Arc<ControllerState>>,
    pub openclaw_observation: Option<FakeOpenclawObservation>,
    physical_runtime: Option<PhysicalRuntimeHandle>,
    event_rx: Option<tokio::sync::broadcast::Receiver<EventEnvelope>>,
}

impl WorkerTaskHarnessResult {
    pub fn drain_session_events(&mut self) {
        let Some(rx) = self.event_rx.as_mut() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    collect_tool_evidence(&mut self.tool_evidence, &event, &self.dispatcher_tool_names);
                    self.session_events.push(event);
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
    }

    pub async fn dispatch_physical_tool(&self, tool: &str, params: serde_json::Value) -> Option<ToolResult> {
        let physical = self.physical_runtime.as_ref()?;
        let call = ToolCall {
            id: format!("{tool}-manual"),
            tool: tool.to_string(),
            params,
        };
        Some(physical.dispatcher.dispatch(&call, &physical.context).await)
    }

    pub async fn shutdown(self) {
        if let Some(physical) = self.physical_runtime {
            physical.copper.shutdown().await;
        }
    }
}

fn collect_tool_evidence(evidence: &mut Vec<WorkerTaskToolEvidence>, event: &EventEnvelope, tool_names: &[String]) {
    let (call_id, tool_name, lifecycle) = match &event.event {
        SessionEvent::ToolCallRequested { call_id, tool_name, .. } => (call_id, tool_name, "requested"),
        SessionEvent::ToolCallStarted { call_id, tool_name, .. } => (call_id, tool_name, "started"),
        SessionEvent::ToolCallFinished { call_id, tool_name, .. } => (call_id, tool_name, "finished"),
        _ => return,
    };
    if tool_names.iter().any(|name| name == tool_name) {
        evidence.push(WorkerTaskToolEvidence {
            task_id: String::new(),
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            lifecycle: lifecycle.to_string(),
        });
    }
}

fn failure_result(task_id: &str, message: impl Into<String>) -> WorkerTaskHarnessResult {
    let message = message.into();
    WorkerTaskHarnessResult {
        terminal_status: TaskTerminalStatus::Failed,
        error: Some(message.clone()),
        output: None,
        session_events: Vec::new(),
        tool_evidence: vec![WorkerTaskToolEvidence {
            task_id: task_id.to_string(),
            call_id: String::new(),
            tool_name: "physical_runtime".to_string(),
            lifecycle: format!("failed: {message}"),
        }],
        dispatcher_tool_names: Vec::new(),
        final_controller_state: None,
        openclaw_observation: None,
        physical_runtime: None,
        event_rx: None,
    }
}

/// Run a real worker-style task invocation with a deterministic test model.
pub async fn run_worker_task_invocation_for_tests(
    invocation: TaskInvocation,
    model: Box<dyn Model>,
    options: WorkerTaskHarnessOptions,
) -> WorkerTaskHarnessResult {
    if invocation.mode != ExecutionMode::OodaReAct {
        return failure_result(
            &invocation.task_id.to_string(),
            "test harness expects ExecutionMode::OodaReAct",
        );
    }

    let Some(runtime) = invocation.embodiment_runtime.clone() else {
        return failure_result(
            &invocation.task_id.to_string(),
            "OodaReAct dispatch missing embodiment_runtime (FW-01)",
        );
    };
    let Some(control_manifest) = invocation.control_interface_manifest.clone() else {
        return failure_result(
            &invocation.task_id.to_string(),
            "OodaReAct dispatch missing control_interface_manifest",
        );
    };

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(
        Box::new(roz_agent::tools::execute_code::ExecuteCodeTool),
        ToolCategory::CodeSandbox,
    );
    let extensions = Extensions::new();
    let hot_policy = roz_copper::policy::new_hot_policy();
    hot_policy.store(Arc::new(options.copper_policy));
    let backpressure = crate::telemetry_backpressure::TelemetryBackpressure::new();

    let mut config = PhysicalRuntimeConfig::new(
        runtime,
        control_manifest,
        options.max_velocity,
        hot_policy,
        backpressure.shared(),
        None,
        dispatcher,
        extensions,
        invocation.task_id.to_string(),
        invocation.tenant_id.clone(),
    );
    if options.authorize_rollout {
        config = config.with_rollout_authority(PhysicalRuntimeRolloutAuthority::default());
    }

    let mut physical = match spawn_physical_runtime(config) {
        Ok(handle) => handle,
        Err(error) => return failure_result(&invocation.task_id.to_string(), error.to_string()),
    };

    let emitter = EventEmitter::new(256);
    let event_rx = emitter.subscribe();
    let (lifecycle_tx, mut lifecycle_rx) = mpsc::channel::<SessionEvent>(64);
    let lifecycle_emitter = emitter.clone();
    tokio::spawn(async move {
        while let Some(event) = lifecycle_rx.recv().await {
            lifecycle_emitter.emit(event);
        }
    });
    physical.context.extensions.insert(lifecycle_tx);

    let dispatcher_tool_names = physical.dispatcher.tool_names();
    let turn_input = build_turn_input(&invocation, &physical.dispatcher);
    let prompt_state = build_prompt_state(&invocation, &physical.dispatcher);
    let mut system_prompt = vec![prompt_state.constitution_text];
    system_prompt.extend(prompt_state.project_context);
    let agent_input = build_runtime_shell_input(&invocation, None);
    let seed = AgentInputSeed::new(system_prompt, Vec::new(), turn_input.user_message);
    let safety = SafetyStack::new(vec![Box::new(roz_agent::safety::guards::VelocityLimiter::new(
        options.max_velocity,
    ))]);
    let spatial = Box::new(crate::spatial_bridge::CopperSpatialProvider::new(Arc::clone(
        physical.copper.state(),
    )));

    let mut agent = AgentLoop::new(model, physical.dispatcher.clone(), safety, spatial)
        .with_extensions(physical.context.extensions.clone())
        .with_agent_event_hook(Arc::new(SessionRuntimeEventHook::new(emitter.clone())));

    let output = agent.run_seeded(agent_input, seed).await;
    if options.auto_promote_after_agent
        && let Some(cmd_tx) = physical.context.extensions.get::<mpsc::Sender<ControllerCommand>>()
    {
        let _ = cmd_tx.send(ControllerCommand::PromoteActive).await;
    }

    let mut result = match output {
        Ok(output) => WorkerTaskHarnessResult {
            terminal_status: TaskTerminalStatus::Succeeded,
            error: None,
            output: Some(output),
            session_events: Vec::new(),
            tool_evidence: Vec::new(),
            dispatcher_tool_names,
            final_controller_state: Some(physical.copper.state().load_full()),
            openclaw_observation: physical.openclaw_observation.clone(),
            physical_runtime: Some(physical),
            event_rx: Some(event_rx),
        },
        Err(error) => WorkerTaskHarnessResult {
            terminal_status: TaskTerminalStatus::Failed,
            error: Some(error.to_string()),
            output: None,
            session_events: Vec::new(),
            tool_evidence: Vec::new(),
            dispatcher_tool_names,
            final_controller_state: Some(physical.copper.state().load_full()),
            openclaw_observation: physical.openclaw_observation.clone(),
            physical_runtime: Some(physical),
            event_rx: Some(event_rx),
        },
    };

    result.drain_session_events();
    for evidence in &mut result.tool_evidence {
        evidence.task_id = invocation.task_id.to_string();
    }
    let _ = effective_cognition_mode(&invocation);
    result
}
