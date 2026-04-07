//! Edge agent session relay — subscribes to NATS session requests and runs local agent loops.
//!
//! When `agent_placement` is Edge, the server relays gRPC session messages to the worker
//! via NATS. This module handles the worker side: subscribing to
//! `session.{worker_id}.*.request`, spawning a per-session `AgentLoop`, and publishing
//! responses back on `session.{worker_id}.{session_id}.response`.
//!
//! Messages use JSON envelopes for debuggability (not protobuf binary).

#![allow(
    clippy::collapsible_if,
    clippy::match_same_arms,
    clippy::needless_pass_by_value,
    clippy::redundant_clone,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::type_complexity
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode};
use roz_agent::constitution::build_worker_constitution;
use roz_agent::dispatch::remote::{PendingResults, RemoteToolCall, RemoteToolExecutor};
use roz_agent::dispatch::{Extensions, ToolDispatcher, ToolExecutor};
use roz_agent::model::types::StreamChunk;
use roz_agent::safety::SafetyStack;
use roz_agent::session_runtime::{
    ApprovalRuntimeHandle, PendingApprovalState, PreparedTurn, SessionRuntime, SessionRuntimeBootstrap,
    SessionRuntimeBootstrapImport, SessionRuntimeError, StreamingTurnExecutor, StreamingTurnHandle,
    StreamingTurnResult, TurnExecutionFailure, TurnOutput,
};
use roz_agent::spatial_provider::{
    PrimedWorldStateProvider, WorldStateProvider, format_runtime_world_state_bootstrap_note,
    world_state_has_runtime_data,
};
use roz_core::recovery::{RecoveryConfig, recovery_action_for};
use roz_core::session::activity::RuntimeFailureKind;
use roz_core::session::event::{
    CanonicalSessionEventEnvelope, CorrelationId, EventEnvelope, EventId, SessionEvent, SessionPermissionRule,
};
use roz_core::spatial::{EntityState, WorldState};
use roz_core::tools::{ToolCategory, ToolResult, ToolSchema};
use roz_nats::subjects::Subjects;

use crate::camera::CameraManager;
use crate::config::WorkerConfig;

const SESSION_EVENT_PREFIX: &str = "roz.v1";
const EDGE_MAX_CYCLES: u32 = 20;
const EDGE_MAX_TOKENS: u32 = 8192;
const EDGE_MAX_CONTEXT_TOKENS: u32 = 200_000;

/// JSON envelope used for session messages over NATS.
///
/// The `type` field discriminates message variants; remaining fields are
/// flattened from the variant-specific payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionMessage {
    pub session_id: String,
    #[serde(flatten)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EdgeToolDefinition {
    name: String,
    description: String,
    #[serde(default)]
    parameters: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<String>,
}

const INLINE_TURN_TOOL_SOURCE: &str = "__user_message";

impl EdgeToolDefinition {
    fn category(&self) -> ToolCategory {
        match self.category.as_deref() {
            Some("pure") => ToolCategory::Pure,
            _ => ToolCategory::Physical,
        }
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

struct EdgeSessionSpatialProvider {
    camera_manager: Option<Arc<CameraManager>>,
}

impl EdgeSessionSpatialProvider {
    const fn new(camera_manager: Option<Arc<CameraManager>>) -> Self {
        Self { camera_manager }
    }

    fn snapshot_now(&self) -> WorldState {
        let entities = self.camera_manager.as_ref().map_or_else(Vec::new, |manager| {
            manager
                .cameras()
                .into_iter()
                .map(|camera| EntityState {
                    id: format!("camera:{}", camera.id),
                    kind: "camera_sensor".into(),
                    properties: HashMap::from([
                        ("label".into(), serde_json::Value::String(camera.label)),
                        ("device_path".into(), serde_json::Value::String(camera.device_path)),
                        ("max_fps".into(), serde_json::Value::from(camera.max_fps)),
                        (
                            "hw_encoder_available".into(),
                            serde_json::Value::Bool(camera.hw_encoder_available),
                        ),
                        (
                            "supported_resolutions".into(),
                            serde_json::to_value(camera.supported_resolutions).unwrap_or(serde_json::Value::Null),
                        ),
                    ]),
                    frame_id: "world".into(),
                    ..Default::default()
                })
                .collect()
        });

        WorldState {
            entities,
            ..Default::default()
        }
    }
}

#[async_trait]
impl WorldStateProvider for EdgeSessionSpatialProvider {
    async fn snapshot(&self, _task_id: &str) -> WorldState {
        self.snapshot_now()
    }
}

fn register_edge_camera_tools(
    dispatcher: &mut ToolDispatcher,
    extensions: &mut Extensions,
    camera_manager: Option<&Arc<CameraManager>>,
    vision_config: Option<&Arc<tokio::sync::RwLock<roz_core::edge::vision::VisionConfig>>>,
) {
    let Some(camera_manager) = camera_manager else {
        return;
    };

    extensions.insert(Arc::clone(camera_manager));
    if let Some(vision_config) = vision_config {
        extensions.insert(Arc::clone(vision_config));
    }

    dispatcher.register_with_category(
        Box::new(crate::camera::perception::CaptureFrameTool),
        ToolCategory::Pure,
    );
    dispatcher.register_with_category(Box::new(crate::camera::perception::ListCamerasTool), ToolCategory::Pure);
    dispatcher.register_with_category(
        Box::new(crate::camera::perception::SetVisionStrategyTool),
        ToolCategory::Pure,
    );
}

fn local_edge_tool_inventory(camera_manager: Option<&Arc<CameraManager>>) -> Vec<(ToolSchema, ToolCategory)> {
    let Some(_camera_manager) = camera_manager else {
        return Vec::new();
    };

    let capture = crate::camera::perception::CaptureFrameTool;
    let list = crate::camera::perception::ListCamerasTool;
    let set_strategy = crate::camera::perception::SetVisionStrategyTool;

    vec![
        (capture.schema(), ToolCategory::Pure),
        (list.schema(), ToolCategory::Pure),
        (set_strategy.schema(), ToolCategory::Pure),
    ]
}

fn combined_edge_tool_inventory(
    camera_manager: Option<&Arc<CameraManager>>,
    remote_tools: &[EdgeToolDefinition],
) -> Vec<(ToolSchema, ToolCategory)> {
    let mut inventory = local_edge_tool_inventory(camera_manager);
    inventory.extend(remote_tools.iter().map(|tool| (tool.schema(), tool.category())));
    inventory
}

fn bootstrap_remote_tool_inventory(
    bootstrap: &SessionRuntimeBootstrap,
    camera_manager: Option<&Arc<CameraManager>>,
) -> Vec<EdgeToolDefinition> {
    let local_tool_names: std::collections::HashSet<String> = local_edge_tool_inventory(camera_manager)
        .into_iter()
        .map(|(tool, _)| tool.name)
        .collect();

    bootstrap
        .tool_schemas
        .iter()
        .filter(|tool| !local_tool_names.contains(&tool.name))
        .map(|tool| {
            let category = bootstrap
                .permissions
                .iter()
                .find(|rule| rule.tool_pattern == tool.name)
                .and_then(|rule| rule.category.clone())
                .unwrap_or_else(|| "physical".to_string());
            EdgeToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: serde_json::from_str(&tool.parameters_json).unwrap_or(serde_json::Value::Null),
                category: Some(category),
            }
        })
        .collect()
}

fn flatten_registered_edge_tools(
    registered_sources: &HashMap<String, Vec<EdgeToolDefinition>>,
) -> Vec<EdgeToolDefinition> {
    let mut sources: Vec<_> = registered_sources.iter().collect();
    sources.sort_by(|(left, _), (right, _)| left.cmp(right));
    sources
        .into_iter()
        .flat_map(|(_, tools)| tools.iter().cloned())
        .collect()
}

fn prompt_tool_schema(tool: &ToolSchema) -> roz_agent::prompt_assembler::ToolSchema {
    roz_agent::prompt_assembler::ToolSchema {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters_json: serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn edge_turn_prompt_state(
    mode: AgentLoopMode,
    camera_manager: Option<&Arc<CameraManager>>,
    remote_tools: &[EdgeToolDefinition],
) -> (
    String,
    Vec<roz_agent::prompt_assembler::ToolSchema>,
    Vec<SessionPermissionRule>,
) {
    let inventory = combined_edge_tool_inventory(camera_manager, remote_tools);
    let tool_names: Vec<String> = inventory.iter().map(|(tool, _)| tool.name.clone()).collect();
    let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();

    let permissions = if inventory.is_empty() {
        vec![
            SessionPermissionRule {
                tool_pattern: "*".into(),
                policy: "require_confirmation".into(),
                category: Some("physical".into()),
                reason: Some("default: physical tools require confirmation".into()),
            },
            SessionPermissionRule {
                tool_pattern: "*".into(),
                policy: "allow".into(),
                category: Some("pure".into()),
                reason: Some("default: pure tools auto-allowed".into()),
            },
        ]
    } else {
        inventory
            .iter()
            .map(|(tool, category)| SessionPermissionRule {
                tool_pattern: tool.name.clone(),
                policy: if matches!(category, ToolCategory::Pure) {
                    "allow".into()
                } else {
                    "require_confirmation".into()
                },
                category: Some(if matches!(category, ToolCategory::Pure) {
                    "pure".into()
                } else {
                    "physical".into()
                }),
                reason: None,
            })
            .collect()
    };

    (
        build_worker_constitution(mode, &tool_name_refs),
        inventory.iter().map(|(tool, _)| prompt_tool_schema(tool)).collect(),
        permissions,
    )
}

fn set_registered_edge_tools_source(
    registered_sources: &mut HashMap<String, Vec<EdgeToolDefinition>>,
    source: impl Into<String>,
    tools: Vec<EdgeToolDefinition>,
) -> bool {
    let source = source.into();
    if tools.is_empty() {
        return registered_sources.remove(&source).is_some();
    }
    match registered_sources.get(&source) {
        Some(existing) if existing == &tools => false,
        _ => {
            registered_sources.insert(source, tools);
            true
        }
    }
}

fn sync_edge_runtime_surface(
    session_runtime: &mut SessionRuntime,
    session_mode: AgentLoopMode,
    camera_manager: Option<&Arc<CameraManager>>,
    registered_tool_sources: &HashMap<String, Vec<EdgeToolDefinition>>,
    session_project_context: &[String],
) {
    let registered_tools = flatten_registered_edge_tools(registered_tool_sources);
    let (constitution_text, tool_schemas, session_permissions) =
        edge_turn_prompt_state(session_mode, camera_manager, &registered_tools);
    session_runtime.sync_cognition_mode(session_mode);
    session_runtime.sync_prompt_surface(constitution_text, tool_schemas, session_project_context.to_vec());
    session_runtime.sync_permissions(session_permissions);
}

fn edge_runtime_spatial_note(mode: AgentLoopMode, observed_context: &WorldState, has_cameras: bool) -> String {
    let base = format_runtime_world_state_bootstrap_note(
        "edge_camera_inventory",
        world_state_has_runtime_data(observed_context).then_some(observed_context),
        if has_cameras {
            "registered edge cameras did not expose live world-state beyond inventory at turn start"
        } else {
            "no cameras are registered for this edge session"
        },
    );

    if matches!(mode, AgentLoopMode::React) {
        format!(
            "{base} Current turn mode is React, so the runtime did not bind this bootstrap as active spatial context."
        )
    } else {
        base
    }
}

struct EdgeTurnExecutor {
    config: WorkerConfig,
    camera_manager: Option<Arc<CameraManager>>,
    vision_config: Option<Arc<tokio::sync::RwLock<roz_core::edge::vision::VisionConfig>>>,
    spatial_provider: Arc<PrimedWorldStateProvider>,
    session_id: String,
    tenant_id: String,
    model_name: String,
    response_subject: String,
    nats: async_nats::Client,
    estop_rx: tokio::sync::watch::Receiver<bool>,
    pending_results: PendingResults,
    approval_runtime: ApprovalRuntimeHandle,
    turn_tools: Vec<EdgeToolDefinition>,
    turn_cancel: CancellationToken,
}

impl EdgeTurnExecutor {
    fn configure_turn(&mut self, turn_tools: Vec<EdgeToolDefinition>) {
        self.turn_tools = turn_tools;
        self.turn_cancel = CancellationToken::new();
        self.pending_results
            .lock()
            .expect("edge pending results mutex poisoned")
            .clear();
        self.approval_runtime.clear_pending_approvals();
    }

    fn build_agent_loop(
        config: &WorkerConfig,
        model_name: &str,
        camera_manager: Option<Arc<CameraManager>>,
        vision_config: Option<Arc<tokio::sync::RwLock<roz_core::edge::vision::VisionConfig>>>,
        spatial_provider: Arc<PrimedWorldStateProvider>,
        pending_results: PendingResults,
        approval_runtime: ApprovalRuntimeHandle,
        turn_tools: Vec<EdgeToolDefinition>,
    ) -> Result<(AgentLoop, Option<mpsc::Receiver<RemoteToolCall>>), Box<dyn std::error::Error + Send + Sync>> {
        let model = crate::model_factory::build_model(config, Some(model_name)).map_err(|error| {
            Box::new(std::io::Error::other(error.to_string())) as Box<dyn std::error::Error + Send + Sync>
        })?;
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
        let mut extensions = Extensions::new();
        register_edge_camera_tools(
            &mut dispatcher,
            &mut extensions,
            camera_manager.as_ref(),
            vision_config.as_ref(),
        );

        let (tool_request_tx, tool_request_rx) = mpsc::channel::<RemoteToolCall>(16);
        for tool in &turn_tools {
            dispatcher.register_with_category(
                Box::new(RemoteToolExecutor::new(
                    &tool.name,
                    &tool.description,
                    tool.parameters.clone(),
                    tool_request_tx.clone(),
                    pending_results.clone(),
                    Duration::from_secs(30),
                )),
                tool.category(),
            );
        }

        let guards: Vec<Box<dyn roz_agent::safety::SafetyGuard>> = vec![Box::new(
            roz_agent::safety::guards::VelocityLimiter::new(config.max_velocity.unwrap_or(1.5)),
        )];
        let safety = SafetyStack::new(guards);
        let spatial: Box<dyn WorldStateProvider> = Box::new(spatial_provider);
        let agent = AgentLoop::new(model, dispatcher, safety, spatial)
            .with_extensions(extensions)
            .with_approval_runtime(approval_runtime);

        Ok((agent, (!turn_tools.is_empty()).then_some(tool_request_rx)))
    }
}

impl StreamingTurnExecutor for EdgeTurnExecutor {
    fn execute_turn_streaming(&mut self, prepared: PreparedTurn) -> StreamingTurnHandle<'_> {
        debug_assert!(
            !prepared.system_blocks.is_empty(),
            "SessionRuntime should always provide system blocks"
        );
        let (chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (presence_tx, presence_rx) = mpsc::channel(16);
        let config = self.config.clone();
        let camera_manager = self.camera_manager.clone();
        let vision_config = self.vision_config.clone();
        let spatial_provider = Arc::clone(&self.spatial_provider);
        let pending_results = self.pending_results.clone();
        let approval_runtime = self.approval_runtime.clone();
        let turn_tools = self.turn_tools.clone();
        let session_id = self.session_id.clone();
        let tenant_id = self.tenant_id.clone();
        let model_name = self.model_name.clone();
        let response_subject = self.response_subject.clone();
        let nats = self.nats.clone();
        let turn_cancel = self.turn_cancel.clone();
        let mut estop_rx = self.estop_rx.clone();
        let mode: AgentLoopMode = prepared.cognition_mode();
        let system_prompt = prepared.system_blocks.into_iter().map(|block| block.content).collect();
        let seed = AgentInputSeed::new(system_prompt, prepared.history, prepared.user_message);

        let (agent, tool_call_rx, build_error) = match Self::build_agent_loop(
            &config,
            &model_name,
            camera_manager.clone(),
            vision_config.clone(),
            Arc::clone(&spatial_provider),
            pending_results.clone(),
            approval_runtime.clone(),
            turn_tools,
        ) {
            Ok((agent, tool_call_rx)) => (Some(agent), tool_call_rx, None),
            Err(error) => (None, None, Some(error)),
        };

        StreamingTurnHandle {
            completion: Box::pin(async move {
                if let Some(error) = build_error {
                    return Err(error);
                }
                let mut agent = agent.expect("edge agent should be present when build_error is absent");

                let keepalive_nats = nats.clone();
                let keepalive_subject = response_subject.clone();
                let keepalive_cancel = CancellationToken::new();
                let keepalive_task = keepalive_cancel.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(5));
                    loop {
                        tokio::select! {
                            _ = interval.tick() => {
                                let message = serde_json::json!({"type": "keepalive"});
                                if let Ok(payload) = serde_json::to_vec(&message) {
                                    let _ = keepalive_nats.publish(keepalive_subject.clone(), payload.into()).await;
                                }
                            }
                            () = keepalive_task.cancelled() => return,
                        }
                    }
                });

                let agent_cancel = CancellationToken::new();
                let input = AgentInput::runtime_shell(
                    session_id,
                    tenant_id,
                    model_name,
                    mode,
                    EDGE_MAX_CYCLES,
                    EDGE_MAX_TOKENS,
                    EDGE_MAX_CONTEXT_TOKENS,
                    false,
                    Some(agent_cancel.clone()),
                    roz_core::safety::ControlMode::for_remote(),
                );

                let output = tokio::select! {
                    result = agent.run_streaming_seeded(input, seed, chunk_tx, presence_tx) => {
                        result.map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
                    }
                    changed = estop_rx.changed() => {
                        if changed.is_ok() && *estop_rx.borrow() {
                            agent_cancel.cancel();
                            Err(Box::new(TurnExecutionFailure::new(
                                RuntimeFailureKind::SafetyBlocked,
                                "E-STOP activated during execution",
                            )) as Box<dyn std::error::Error + Send + Sync>)
                        } else {
                            Err(Box::new(TurnExecutionFailure::new(
                                RuntimeFailureKind::EdgeTransportLost,
                                "edge stop watch fired unexpectedly",
                            )) as Box<dyn std::error::Error + Send + Sync>)
                        }
                    }
                    () = turn_cancel.cancelled() => {
                        agent_cancel.cancel();
                        Err(Box::new(TurnExecutionFailure::new(
                            RuntimeFailureKind::OperatorAbort,
                            "edge turn cancelled",
                        )) as Box<dyn std::error::Error + Send + Sync>)
                    }
                };

                keepalive_cancel.cancel();

                let output = output?;

                Ok(TurnOutput {
                    assistant_message: output.final_response.unwrap_or_default(),
                    tool_calls_made: output.cycles,
                    input_tokens: u64::from(output.total_usage.input_tokens),
                    output_tokens: u64::from(output.total_usage.output_tokens),
                    cache_read_tokens: u64::from(output.total_usage.cache_read_tokens),
                    cache_creation_tokens: u64::from(output.total_usage.cache_creation_tokens),
                    messages: output.messages,
                })
            }),
            chunk_rx,
            presence_rx,
            tool_call_rx,
        }
    }
}

/// Spawns the session relay loop, listening for edge session requests on NATS.
///
/// Subscribes to `session.{worker_id}.*.request` (wildcard for `session_id`).
/// On the first `start_session` message for a new `session_id`, spawns a
/// dedicated per-session task that manages the `AgentLoop` lifecycle.
pub async fn spawn_session_relay(
    nats: async_nats::Client,
    worker_id: String,
    config: WorkerConfig,
    estop_rx: tokio::sync::watch::Receiver<bool>,
    camera_manager: Option<Arc<CameraManager>>,
) -> anyhow::Result<()> {
    let subject = format!("session.{worker_id}.*.request");
    let mut sub = nats.subscribe(subject.clone()).await?;
    tracing::info!(subject, "session relay listening");

    let sessions: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(HashMap::new()));

    while let Some(msg) = sub.next().await {
        // Extract session_id from subject: session.{worker_id}.{session_id}.request
        let parts: Vec<&str> = msg.subject.as_str().split('.').collect();
        if parts.len() < 4 {
            tracing::warn!(subject = %msg.subject, "malformed session relay subject");
            continue;
        }
        let session_id = parts[2].to_string();

        let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
            tracing::warn!(session_id, "failed to deserialize session relay message");
            continue;
        };

        let msg_type = envelope["type"].as_str().unwrap_or("");

        let mut sessions_lock = sessions.lock().await;

        if msg_type == "start_session" && !sessions_lock.contains_key(&session_id) {
            let nats_clone = nats.clone();
            let worker_id_clone = worker_id.clone();
            let session_id_clone = session_id.clone();
            let config_clone = config.clone();
            let sessions_ref = sessions.clone();
            let estop_rx_clone = estop_rx.clone();
            let cam_mgr_clone = camera_manager.clone();

            let handle = tokio::spawn(async move {
                if let Err(e) = handle_edge_session(
                    nats_clone,
                    &worker_id_clone,
                    &session_id_clone,
                    &config_clone,
                    envelope,
                    estop_rx_clone,
                    cam_mgr_clone,
                )
                .await
                {
                    tracing::error!(error = %e, session_id = %session_id_clone, "edge session failed");
                }
                // Clean up session entry on exit.
                sessions_ref.lock().await.remove(&session_id_clone);
            });

            sessions_lock.insert(session_id, handle);
        }
        // For existing sessions, the per-session subscription handles subsequent messages.
    }

    Ok(())
}

/// Runs a single edge session: creates an `AgentLoop`, listens for messages,
/// and publishes responses.
#[expect(clippy::too_many_lines, reason = "sequential session lifecycle with model setup")]
async fn handle_edge_session(
    nats: async_nats::Client,
    worker_id: &str,
    session_id: &str,
    config: &WorkerConfig,
    start_msg: serde_json::Value,
    estop_rx: tokio::sync::watch::Receiver<bool>,
    camera_manager: Option<Arc<CameraManager>>,
) -> anyhow::Result<()> {
    let response_subject = Subjects::session_response(worker_id, session_id)?;
    let bootstrap = parse_runtime_bootstrap(session_id, &start_msg)?;
    let session_mode = parse_edge_session_mode(&start_msg, bootstrap.cognition_mode(), config.max_velocity.is_some());
    let model_name = bootstrap
        .model_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| start_msg["model"].as_str().filter(|name| !name.trim().is_empty()))
        .unwrap_or(&config.model_name)
        .to_string();
    let tenant_id = if bootstrap.tenant_id.trim().is_empty() {
        "edge".to_string()
    } else {
        bootstrap.tenant_id.trim().to_string()
    };
    let session_project_context = bootstrap.project_context.clone();
    let session_permissions = bootstrap.permissions.clone();
    let mut registered_tool_sources = HashMap::new();
    let bootstrap_remote_tools = bootstrap_remote_tool_inventory(&bootstrap, camera_manager.as_ref());
    if !bootstrap_remote_tools.is_empty() {
        registered_tool_sources.insert("__bootstrap".to_string(), bootstrap_remote_tools);
    }

    // Subscribe to this specific session's requests.
    let request_subject = Subjects::session_request(worker_id, session_id)?;
    let mut session_sub = nats.subscribe(request_subject).await?;
    tracing::info!(session_id, model = %model_name, "edge session started");

    let mut session_mode = session_mode;
    let shared_vision_config = camera_manager
        .as_ref()
        .map(|_| Arc::new(tokio::sync::RwLock::new(roz_core::edge::vision::VisionConfig::default())));
    if camera_manager.is_some() {
        tracing::info!(session_id, "camera perception tools registered for edge session");
    }
    let registered_tools = flatten_registered_edge_tools(&registered_tool_sources);
    let (constitution_text, tool_schemas, derived_permissions) =
        edge_turn_prompt_state(session_mode, camera_manager.as_ref(), &registered_tools);
    let session_permissions = if session_permissions.is_empty() {
        derived_permissions
    } else {
        session_permissions
    };
    let spatial_provider = Arc::new(PrimedWorldStateProvider::unprimed(Box::new(
        EdgeSessionSpatialProvider::new(camera_manager.clone()),
    )));

    // SessionRuntime for canonical lifecycle tracking
    let mut session_runtime = SessionRuntime::from_bootstrap(
        bootstrap,
        SessionRuntimeBootstrapImport {
            cognition_mode_override: Some(session_mode),
            constitution_text_override: Some(constitution_text.clone()),
            blueprint_version_override: None,
            tool_schemas_override: Some(tool_schemas.clone()),
        },
    );
    session_runtime.sync_cognition_mode(session_mode);
    session_runtime.sync_prompt_surface(constitution_text, tool_schemas, session_project_context.clone());
    session_runtime.sync_permissions(session_permissions.clone());
    let prompt_staging = session_runtime.turn_prompt_staging();
    let mut event_rx = session_runtime.subscribe_events();
    session_runtime.start_session().await?;
    drain_runtime_events(&nats, session_id, &response_subject, &mut event_rx).await?;
    let mut checkpoint_bootstrap = session_runtime.export_bootstrap();
    publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;

    let mut executor = EdgeTurnExecutor {
        config: config.clone(),
        camera_manager: camera_manager.clone(),
        vision_config: shared_vision_config,
        spatial_provider: Arc::clone(&spatial_provider),
        session_id: session_id.to_string(),
        tenant_id,
        model_name: model_name.clone(),
        response_subject: response_subject.clone(),
        nats: nats.clone(),
        estop_rx: estop_rx.clone(),
        pending_results: Arc::new(std::sync::Mutex::new(HashMap::new())),
        approval_runtime: session_runtime.approval_handle(),
        turn_tools: Vec::new(),
        turn_cancel: CancellationToken::new(),
    };

    tracing::info!(session_id, ?session_mode, "edge session mode resolved");
    let mut session_closed = false;
    let mut pending_surface_resync = false;

    // Process subsequent messages on this session's dedicated subscription.
    while let Some(msg) = session_sub.next().await {
        if *estop_rx.borrow() {
            tracing::error!(session_id, "E-STOP received — terminating edge session");
            session_runtime.handle_failure(RuntimeFailureKind::SafetyBlocked);
            drain_runtime_events(&nats, session_id, &response_subject, &mut event_rx).await?;
            checkpoint_bootstrap = session_runtime.export_bootstrap();
            publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;
            session_closed = true;
            break;
        }

        let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
            tracing::warn!(session_id, "edge session: failed to deserialize message");
            continue;
        };

        let msg_type = envelope["type"].as_str().unwrap_or("");

        match msg_type {
            "register_tools" => {
                let system_context = parse_runtime_system_context(&envelope);
                let source = envelope["source"].as_str().unwrap_or("").to_string();
                let remote_tools = parse_edge_turn_tools(session_id, &envelope);
                set_registered_edge_tools_source(&mut registered_tool_sources, source, remote_tools);
                sync_edge_runtime_surface(
                    &mut session_runtime,
                    session_mode,
                    camera_manager.as_ref(),
                    &registered_tool_sources,
                    &session_project_context,
                );
                prompt_staging.stage_system_context(system_context);
                checkpoint_bootstrap = session_runtime.export_bootstrap();
                publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;
            }
            "user_message" => {
                let user_text = envelope["text"].as_str().unwrap_or("").to_string();
                let mode_changed = if let Some(requested_mode) = parse_user_message_mode(&envelope)
                    && requested_mode != session_mode
                {
                    session_mode = requested_mode;
                    true
                } else {
                    false
                };
                let requested_turn_tools = parse_edge_turn_tools(session_id, &envelope);
                let tools_changed = if requested_turn_tools.is_empty() {
                    false
                } else {
                    set_registered_edge_tools_source(
                        &mut registered_tool_sources,
                        INLINE_TURN_TOOL_SOURCE,
                        requested_turn_tools,
                    )
                };
                if mode_changed || tools_changed {
                    sync_edge_runtime_surface(
                        &mut session_runtime,
                        session_mode,
                        camera_manager.as_ref(),
                        &registered_tool_sources,
                        &session_project_context,
                    );
                }
                let turn_tools = flatten_registered_edge_tools(&registered_tool_sources);
                executor.configure_turn(turn_tools);
                let custom_context = prompt_staging.take_turn_custom_context(parse_runtime_system_context(&envelope));
                let volatile_blocks = parse_runtime_volatile_blocks(session_id, &envelope);
                let message_id = parse_edge_message_id(&envelope);
                let observed_context = executor.spatial_provider.prime_from_live_snapshot(session_id).await;
                let spatial_context = if matches!(session_mode, AgentLoopMode::OodaReAct)
                    && world_state_has_runtime_data(&observed_context)
                {
                    Some(observed_context.clone())
                } else {
                    None
                };
                session_runtime.sync_world_state_with_note(
                    spatial_context,
                    Some(edge_runtime_spatial_note(
                        session_mode,
                        &observed_context,
                        camera_manager.is_some(),
                    )),
                );
                let pending_results = executor.pending_results.clone();
                let approval_runtime = executor.approval_runtime.clone();
                let turn_cancel = executor.turn_cancel.clone();
                let (turn_result, close_session_after_turn) = {
                    let turn_future = session_runtime.run_turn_streaming(
                        roz_agent::session_runtime::TurnInput {
                            user_message: user_text,
                            cognition_mode: session_mode,
                            custom_context,
                            volatile_blocks,
                        },
                        message_id,
                        &mut executor,
                    );
                    tokio::pin!(turn_future);
                    let mut close_session_after_turn = false;

                    let result = loop {
                        tokio::select! {
                            turn_result = &mut turn_future => break turn_result,
                            recv = event_rx.recv() => {
                                match recv {
                                    Ok(envelope) => {
                                        apply_runtime_event_to_checkpoint(&mut checkpoint_bootstrap, &envelope.event);
                                        publish_event_envelope(&nats, session_id, &response_subject, &envelope).await?;
                                        if approval_checkpoint_event(&envelope.event) {
                                            publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap)
                                                .await?;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                        tracing::warn!(session_id, skipped, "edge session event stream lagged");
                                    }
                                    Err(broadcast::error::RecvError::Closed) => {}
                                }
                            }
                            maybe_msg = session_sub.next() => {
                                let Some(msg) = maybe_msg else {
                                    close_session_after_turn = true;
                                    turn_cancel.cancel();
                                    continue;
                                };
                                let Ok(turn_envelope) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
                                    tracing::warn!(session_id, "edge session: failed to deserialize in-flight message");
                                    continue;
                                };
                                match turn_envelope["type"].as_str().unwrap_or("") {
                                    "register_tools" => {
                                        let system_context = parse_runtime_system_context(&turn_envelope);
                                        let source = turn_envelope["source"].as_str().unwrap_or("").to_string();
                                        let remote_tools = parse_edge_turn_tools(session_id, &turn_envelope);
                                        if set_registered_edge_tools_source(&mut registered_tool_sources, source, remote_tools) {
                                            pending_surface_resync = true;
                                        }
                                        prompt_staging.stage_system_context(system_context);
                                    }
                                    "tool_result" => {
                                        if let Some((tool_call_id, tool_result)) = parse_edge_tool_result(session_id, &turn_envelope) {
                                            if !roz_agent::dispatch::remote::resolve_pending(&pending_results, &tool_call_id, tool_result) {
                                                tracing::warn!(session_id, %tool_call_id, "edge tool result did not match a pending call");
                                            }
                                        }
                                    }
                                    "permission_decision" => {
                                        if let Some((approval_id, approved, modifier)) = parse_edge_permission_decision(&turn_envelope) {
                                            if !approval_runtime.resolve_approval(&approval_id, approved, modifier) {
                                                tracing::warn!(
                                                    session_id,
                                                    %approval_id,
                                                    approved,
                                                    "edge permission decision did not match a pending approval"
                                                );
                                            }
                                        }
                                    }
                                    "cancel_turn" => turn_cancel.cancel(),
                                    "cancel_session" => {
                                        tracing::info!(session_id, "edge session cancelled by server");
                                        close_session_after_turn = true;
                                        turn_cancel.cancel();
                                    }
                                    "user_message" => {
                                        publish_session_event(
                                            &nats,
                                            session_id,
                                            &response_subject,
                                            CorrelationId::new(),
                                            SessionEvent::SessionRejected {
                                                code: "turn_in_progress".to_string(),
                                                message: "a turn is already in progress".to_string(),
                                                retryable: false,
                                            },
                                        )
                                        .await?;
                                    }
                                    other => {
                                        tracing::debug!(msg_type = other, session_id, "unhandled in-flight edge session message type");
                                    }
                                }
                            }
                        }
                    };
                    (result, close_session_after_turn)
                };

                match turn_result {
                    Ok(StreamingTurnResult::Completed(_) | StreamingTurnResult::Cancelled) => {
                        if pending_surface_resync {
                            sync_edge_runtime_surface(
                                &mut session_runtime,
                                session_mode,
                                camera_manager.as_ref(),
                                &registered_tool_sources,
                                &session_project_context,
                            );
                            pending_surface_resync = false;
                        }
                        drain_runtime_events(&nats, session_id, &response_subject, &mut event_rx).await?;
                        checkpoint_bootstrap = session_runtime.export_bootstrap();
                        publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;
                        if close_session_after_turn
                            && session_runtime.complete_session("cancelled by server").await.is_ok()
                        {
                            drain_runtime_events(&nats, session_id, &response_subject, &mut event_rx).await?;
                            checkpoint_bootstrap = session_runtime.export_bootstrap();
                            publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;
                            session_closed = true;
                        }
                    }
                    Err(error) => {
                        tracing::error!(session_id, error = %error, "edge session runtime error");
                        if pending_surface_resync {
                            sync_edge_runtime_surface(
                                &mut session_runtime,
                                session_mode,
                                camera_manager.as_ref(),
                                &registered_tool_sources,
                                &session_project_context,
                            );
                            pending_surface_resync = false;
                        }
                        drain_runtime_events(&nats, session_id, &response_subject, &mut event_rx).await?;
                        checkpoint_bootstrap = session_runtime.export_bootstrap();
                        publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;
                        if let Some(event) = runtime_error_event(&error) {
                            publish_session_event(&nats, session_id, &response_subject, CorrelationId::new(), event)
                                .await?;
                        }
                        session_closed =
                            matches!(error, SessionRuntimeError::SessionFailed(_)) || close_session_after_turn;
                    }
                }

                if session_closed {
                    break;
                }
            }
            "cancel_session" => {
                tracing::info!(session_id, "edge session cancelled by server");
                if session_runtime.complete_session("cancelled by server").await.is_ok() {
                    drain_runtime_events(&nats, session_id, &response_subject, &mut event_rx).await?;
                    checkpoint_bootstrap = session_runtime.export_bootstrap();
                    publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;
                }
                session_closed = true;
                break;
            }
            "tool_result" | "permission_decision" | "cancel_turn" => {
                tracing::warn!(
                    msg_type,
                    session_id,
                    "received edge turn control message without an active turn"
                );
            }
            _ => {
                tracing::debug!(msg_type, session_id, "unhandled edge session message type");
            }
        }
    }

    if !session_closed
        && !matches!(
            session_runtime.activity(),
            roz_core::session::activity::RuntimeActivity::Degraded
        )
    {
        if session_runtime.complete_session("edge session ended").await.is_ok() {
            drain_runtime_events(&nats, session_id, &response_subject, &mut event_rx).await?;
            checkpoint_bootstrap = session_runtime.export_bootstrap();
            publish_runtime_checkpoint(&nats, &response_subject, &checkpoint_bootstrap).await?;
        }
    }

    tracing::info!(session_id, "edge session ended");
    Ok(())
}

async fn drain_runtime_events(
    nats: &async_nats::Client,
    session_id: &str,
    response_subject: &str,
    event_rx: &mut broadcast::Receiver<EventEnvelope>,
) -> anyhow::Result<()> {
    loop {
        match event_rx.try_recv() {
            Ok(envelope) => {
                publish_event_envelope(nats, session_id, response_subject, &envelope).await?;
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => return Ok(()),
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                tracing::warn!(session_id, skipped, "edge session event stream lagged");
            }
        }
    }
}

async fn publish_runtime_checkpoint(
    nats: &async_nats::Client,
    response_subject: &str,
    bootstrap: &SessionRuntimeBootstrap,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "type": "runtime_checkpoint",
        "bootstrap": bootstrap,
    }))?;
    nats.publish(response_subject.to_string(), payload.into()).await?;
    Ok(())
}

async fn publish_event_envelope(
    nats: &async_nats::Client,
    session_id: &str,
    response_subject: &str,
    envelope: &EventEnvelope,
) -> anyhow::Result<()> {
    let canonical_payload = serde_json::to_vec(&CanonicalSessionEventEnvelope::from_event_envelope(envelope))?;
    nats.publish(response_subject.to_string(), canonical_payload.into())
        .await?;

    let payload = serde_json::to_vec(envelope)?;
    let event_subject = crate::event_nats::event_subject(SESSION_EVENT_PREFIX, session_id, &envelope.event);
    nats.publish(event_subject, payload.into()).await?;
    Ok(())
}

async fn publish_session_event(
    nats: &async_nats::Client,
    session_id: &str,
    response_subject: &str,
    correlation_id: CorrelationId,
    event: SessionEvent,
) -> anyhow::Result<()> {
    let envelope = EventEnvelope {
        event_id: EventId::new(),
        correlation_id,
        parent_event_id: None,
        timestamp: chrono::Utc::now(),
        event,
    };
    publish_event_envelope(nats, session_id, response_subject, &envelope).await
}

fn apply_runtime_event_to_checkpoint(bootstrap: &mut SessionRuntimeBootstrap, event: &SessionEvent) {
    match event {
        SessionEvent::ActivityChanged { state, .. } => {
            bootstrap.activity = *state;
        }
        SessionEvent::ApprovalRequested {
            approval_id,
            action,
            reason,
            timeout_secs,
        } => {
            bootstrap
                .pending_approvals
                .retain(|pending| pending.approval_id != *approval_id);
            bootstrap.pending_approvals.push(PendingApprovalState {
                approval_id: approval_id.clone(),
                action: action.clone(),
                reason: reason.clone(),
                timeout_secs: *timeout_secs,
            });
        }
        SessionEvent::ApprovalResolved { approval_id, .. } => {
            bootstrap
                .pending_approvals
                .retain(|pending| pending.approval_id != *approval_id);
        }
        _ => {}
    }
}

const fn approval_checkpoint_event(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::ApprovalRequested { .. } | SessionEvent::ApprovalResolved { .. }
    )
}

fn runtime_error_event(error: &SessionRuntimeError) -> Option<SessionEvent> {
    match error {
        SessionRuntimeError::SessionPaused => Some(SessionEvent::SessionRejected {
            code: "session_paused".to_string(),
            message: "session is paused".to_string(),
            retryable: false,
        }),
        SessionRuntimeError::SessionCompleted => Some(SessionEvent::SessionRejected {
            code: "session_completed".to_string(),
            message: "session already completed".to_string(),
            retryable: false,
        }),
        SessionRuntimeError::TurnFailed(failure) => {
            let action = recovery_action_for(failure, &RecoveryConfig::default());
            Some(SessionEvent::SessionRejected {
                code: "turn_failed".to_string(),
                message: format!("turn failed: {failure:?}"),
                retryable: action.retry,
            })
        }
        SessionRuntimeError::SessionFailed(_) => None,
    }
}

fn parse_edge_turn_tools(session_id: &str, message: &serde_json::Value) -> Vec<EdgeToolDefinition> {
    serde_json::from_value(
        message
            .get("tools")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new())),
    )
    .unwrap_or_else(|error| {
        tracing::warn!(session_id, %error, "failed to parse edge forwarded tool schemas");
        Vec::new()
    })
}

fn parse_edge_message_id(message: &serde_json::Value) -> Option<String> {
    message
        .get("message_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message_id| !message_id.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_edge_tool_result(session_id: &str, message: &serde_json::Value) -> Option<(String, ToolResult)> {
    let tool_call_id = message
        .get("tool_call_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|call_id| !call_id.is_empty())
        .map(ToOwned::to_owned)?;
    let success = message
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let raw_result = message
        .get("result")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let exit_code = message
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    let truncated = message
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let duration_ms = message
        .get("duration_ms")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| {
            u64::try_from(value)
                .map_err(|error| {
                    tracing::warn!(session_id, %error, duration_ms = value, "invalid edge tool result duration");
                })
                .ok()
        });

    let result = if success {
        ToolResult {
            output: serde_json::from_str(&raw_result).unwrap_or(serde_json::Value::String(raw_result)),
            error: None,
            exit_code,
            truncated,
            duration_ms,
        }
    } else {
        ToolResult {
            output: serde_json::Value::Null,
            error: Some(raw_result),
            exit_code,
            truncated,
            duration_ms,
        }
    };

    Some((tool_call_id, result))
}

fn parse_edge_permission_decision(message: &serde_json::Value) -> Option<(String, bool, Option<serde_json::Value>)> {
    let approval_id = message
        .get("approval_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|approval_id| !approval_id.is_empty())
        .map(ToOwned::to_owned)?;
    let approved = message
        .get("approved")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let modifier = message.get("modifier").cloned().filter(|value| !value.is_null());
    Some((approval_id, approved, modifier))
}

/// Parse the agent loop mode from a `start_session` message.
///
/// Explicit `"mode"` field takes precedence. When absent, defaults to
/// the bootstrapped runtime mode. If the bootstrap is unavailable, fall back
/// to `OodaReAct` for physical workers and `React` otherwise.
fn parse_edge_session_mode(
    start_msg: &serde_json::Value,
    bootstrap_mode: AgentLoopMode,
    has_physical: bool,
) -> AgentLoopMode {
    match start_msg["mode"].as_str() {
        Some("react") => AgentLoopMode::React,
        Some("ooda_react" | "ooda_re_act") => AgentLoopMode::OodaReAct,
        _ if start_msg.get("bootstrap").is_some() => bootstrap_mode,
        _ if has_physical => AgentLoopMode::OodaReAct,
        _ => AgentLoopMode::React,
    }
}

fn parse_user_message_mode(message: &serde_json::Value) -> Option<AgentLoopMode> {
    match message["ai_mode"].as_str() {
        Some("react") => Some(AgentLoopMode::React),
        Some("ooda_react" | "ooda_re_act" | "ooda") => Some(AgentLoopMode::OodaReAct),
        _ => None,
    }
}

fn parse_runtime_system_context(message: &serde_json::Value) -> Option<String> {
    message
        .get("system_context")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn parse_runtime_volatile_blocks(session_id: &str, message: &serde_json::Value) -> Vec<String> {
    serde_json::from_value(
        message
            .get("volatile_blocks")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .unwrap_or_else(|error| {
        tracing::warn!(session_id, %error, "failed to parse edge volatile prompt blocks");
        Vec::new()
    })
}

fn parse_runtime_bootstrap(session_id: &str, start_msg: &serde_json::Value) -> anyhow::Result<SessionRuntimeBootstrap> {
    serde_json::from_value(
        start_msg
            .get("bootstrap")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing edge runtime bootstrap for session {session_id}"))?,
    )
    .map_err(|error| anyhow::anyhow!("failed to parse edge runtime bootstrap for session {session_id}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::stream_hub::StreamHub;

    fn sample_runtime_bootstrap(session_id: &str) -> SessionRuntimeBootstrap {
        SessionRuntimeBootstrap::from_config(&roz_agent::session_runtime::SessionConfig {
            session_id: session_id.to_string(),
            tenant_id: "tenant-edge".into(),
            mode: roz_core::session::control::SessionMode::Edge,
            cognition_mode: roz_core::session::control::CognitionMode::React,
            constitution_text: String::new(),
            blueprint_toml: String::new(),
            model_name: Some("claude-sonnet-4-6".into()),
            permissions: vec![roz_core::session::event::SessionPermissionRule {
                tool_pattern: "capture_frame".into(),
                policy: "allow".into(),
                category: Some("pure".into()),
                reason: None,
            }],
            tool_schemas: Vec::new(),
            project_context: vec!["# AGENTS.md\nEdge bootstrap".into()],
            initial_history: vec![roz_agent::model::types::Message::user("hello edge")],
        })
    }

    #[test]
    fn session_message_serializes_with_flattened_payload() {
        let msg = SessionMessage {
            session_id: "sess-123".to_string(),
            payload: serde_json::json!({"type": "start_session", "model": "claude-sonnet-4-6"}),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["session_id"], "sess-123");
        assert_eq!(json["type"], "start_session");
        assert_eq!(json["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn session_message_deserializes_from_json() {
        let json = serde_json::json!({
            "session_id": "sess-456",
            "type": "user_message",
            "text": "hello"
        });
        let msg: SessionMessage = serde_json::from_value(json).unwrap();
        assert_eq!(msg.session_id, "sess-456");
        assert_eq!(msg.payload["type"], "user_message");
        assert_eq!(msg.payload["text"], "hello");
    }

    #[test]
    fn parse_edge_session_mode_explicit_react() {
        let msg = serde_json::json!({"type": "start_session", "mode": "react"});
        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::OodaReAct, true),
            AgentLoopMode::React
        );
        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::React, false),
            AgentLoopMode::React
        );
    }

    #[test]
    fn parse_edge_session_mode_explicit_ooda() {
        let msg = serde_json::json!({"type": "start_session", "mode": "ooda_react"});
        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::React, false),
            AgentLoopMode::OodaReAct
        );
    }

    #[test]
    fn parse_edge_session_mode_absent_defaults_by_physical() {
        let msg = serde_json::json!({"type": "start_session"});
        // Physical worker defaults to OodaReAct
        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::React, true),
            AgentLoopMode::OodaReAct
        );
        // Non-physical worker defaults to React
        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::OodaReAct, false),
            AgentLoopMode::React
        );
    }

    #[test]
    fn parse_edge_session_mode_unknown_value_treated_as_absent() {
        let msg = serde_json::json!({"type": "start_session", "mode": "turbo"});
        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::React, true),
            AgentLoopMode::OodaReAct
        );
        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::OodaReAct, false),
            AgentLoopMode::React
        );
    }

    #[test]
    fn parse_edge_session_mode_prefers_bootstrap_mode_when_present() {
        let msg = serde_json::json!({
            "type": "start_session",
            "bootstrap": sample_runtime_bootstrap("sess-edge-bootstrap-mode"),
        });

        assert_eq!(
            parse_edge_session_mode(&msg, AgentLoopMode::OodaReAct, false),
            AgentLoopMode::OodaReAct
        );
    }

    #[test]
    fn parse_runtime_bootstrap_reads_canonical_payload() {
        let bootstrap = sample_runtime_bootstrap("sess-edge-bootstrap");
        let start_msg = serde_json::json!({
            "type": "start_session",
            "bootstrap": bootstrap,
        });

        let parsed = parse_runtime_bootstrap("sess-edge-bootstrap", &start_msg).expect("bootstrap should parse");

        assert_eq!(parsed.session_id, "sess-edge-bootstrap");
        assert_eq!(parsed.tenant_id, "tenant-edge");
        assert_eq!(parsed.model_name.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(parsed.project_context, vec!["# AGENTS.md\nEdge bootstrap".to_string()]);
        assert_eq!(parsed.permissions.len(), 1);
        assert_eq!(parsed.history.len(), 1);
    }

    #[test]
    fn parse_runtime_bootstrap_requires_canonical_payload() {
        let start_msg = serde_json::json!({
            "type": "start_session",
            "model": "claude-sonnet-4-6",
        });

        let error = parse_runtime_bootstrap("sess-edge-missing", &start_msg).expect_err("bootstrap should be required");
        assert!(error.to_string().contains("missing edge runtime bootstrap"));
    }

    #[test]
    fn runtime_error_event_marks_retryable_turn_failures() {
        let retryable = runtime_error_event(&SessionRuntimeError::TurnFailed(RuntimeFailureKind::ModelError))
            .expect("turn failure should produce relay event");
        let blocking = runtime_error_event(&SessionRuntimeError::TurnFailed(RuntimeFailureKind::SafetyBlocked))
            .expect("turn failure should produce relay event");

        match retryable {
            SessionEvent::SessionRejected { code, retryable, .. } => {
                assert_eq!(code, "turn_failed");
                assert!(retryable, "model errors should surface as retryable");
            }
            other => panic!("expected SessionRejected, got {other:?}"),
        }

        match blocking {
            SessionEvent::SessionRejected { retryable, .. } => {
                assert!(!retryable, "safety failures should not surface as retryable");
            }
            other => panic!("expected SessionRejected, got {other:?}"),
        }
    }

    #[test]
    fn bootstrap_remote_tool_inventory_rehydrates_remote_prompt_tools() {
        let mut bootstrap = sample_runtime_bootstrap("sess-edge-bootstrap-tools");
        bootstrap.tool_schemas = vec![roz_agent::prompt_assembler::ToolSchema {
            name: "scan_area".into(),
            description: "Scan the area".into(),
            parameters_json: r#"{"type":"object","properties":{"radius_m":{"type":"number"}}}"#.into(),
        }];
        bootstrap.permissions = vec![roz_core::session::event::SessionPermissionRule {
            tool_pattern: "scan_area".into(),
            policy: "allow".into(),
            category: Some("pure".into()),
            reason: None,
        }];

        let tools = bootstrap_remote_tool_inventory(&bootstrap, None);

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "scan_area");
        assert_eq!(tools[0].category(), ToolCategory::Pure);
        assert_eq!(tools[0].parameters["properties"]["radius_m"]["type"], "number");
    }

    #[test]
    fn parse_user_message_mode_recognizes_react() {
        let msg = serde_json::json!({"type": "user_message", "ai_mode": "react"});
        assert_eq!(parse_user_message_mode(&msg), Some(AgentLoopMode::React));
    }

    #[test]
    fn parse_user_message_mode_recognizes_ooda_aliases() {
        let ooda = serde_json::json!({"type": "user_message", "ai_mode": "ooda"});
        assert_eq!(parse_user_message_mode(&ooda), Some(AgentLoopMode::OodaReAct));

        let ooda_react = serde_json::json!({"type": "user_message", "ai_mode": "ooda_react"});
        assert_eq!(parse_user_message_mode(&ooda_react), Some(AgentLoopMode::OodaReAct));
    }

    #[test]
    fn parse_user_message_mode_ignores_unknown_values() {
        let msg = serde_json::json!({"type": "user_message", "ai_mode": "turbo"});
        assert_eq!(parse_user_message_mode(&msg), None);
    }

    #[test]
    fn parse_runtime_system_context_reads_inline_system_context() {
        let message = serde_json::json!({"system_context": "   "});

        let system_context = parse_runtime_system_context(&message);

        assert_eq!(system_context.as_deref(), Some("   "));
    }

    #[test]
    fn parse_runtime_volatile_blocks_reads_forwarded_context() {
        let message = serde_json::json!({
            "volatile_blocks": ["[Editor]\nfn main() {}", "[Selection]\nlet x = 1;"]
        });

        let volatile_blocks = parse_runtime_volatile_blocks("sess-1", &message);

        assert_eq!(
            volatile_blocks,
            vec![
                "[Editor]\nfn main() {}".to_string(),
                "[Selection]\nlet x = 1;".to_string()
            ]
        );
    }

    #[test]
    fn parse_edge_turn_tools_reads_forwarded_tool_schemas() {
        let message = serde_json::json!({
            "tools": [{
                "name": "scan_area",
                "description": "Scan the area",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "radius_m": { "type": "number" }
                    }
                },
                "category": "pure"
            }]
        });

        let tools = parse_edge_turn_tools("sess-edge-tools", &message);

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "scan_area");
        assert_eq!(tools[0].category(), ToolCategory::Pure);
        assert_eq!(tools[0].parameters["properties"]["radius_m"]["type"], "number");
    }

    #[test]
    fn parse_edge_tool_result_reads_structured_payload() {
        let message = serde_json::json!({
            "type": "tool_result",
            "tool_call_id": "toolu_123",
            "success": true,
            "result": "{\"ok\":true}",
            "exit_code": 0,
            "truncated": true,
            "duration_ms": 42
        });

        let (tool_call_id, result) =
            parse_edge_tool_result("sess-edge-tool-result", &message).expect("tool result should parse");

        assert_eq!(tool_call_id, "toolu_123");
        assert_eq!(result.output["ok"], true);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.truncated);
        assert_eq!(result.duration_ms, Some(42));
    }

    #[test]
    fn parse_edge_permission_decision_reads_modifier() {
        let message = serde_json::json!({
            "type": "permission_decision",
            "approval_id": "apr_approve",
            "approved": true,
            "modifier": {
                "position": { "x": 1.0 }
            }
        });

        let (approval_id, approved, modifier) =
            parse_edge_permission_decision(&message).expect("permission decision should parse");

        assert_eq!(approval_id, "apr_approve");
        assert!(approved);
        assert_eq!(modifier.expect("modifier should be present")["position"]["x"], 1.0);
    }

    #[test]
    fn approval_events_update_checkpoint_pending_approvals() {
        let mut bootstrap = sample_runtime_bootstrap("sess-edge-approval-checkpoint");
        bootstrap.pending_approvals.clear();

        let requested = SessionEvent::ApprovalRequested {
            approval_id: "apr-1".into(),
            action: "sensitive_op".into(),
            reason: "needs approval".into(),
            timeout_secs: 30,
        };
        apply_runtime_event_to_checkpoint(&mut bootstrap, &requested);

        assert!(approval_checkpoint_event(&requested));
        assert_eq!(bootstrap.pending_approvals.len(), 1);
        assert_eq!(bootstrap.pending_approvals[0].approval_id, "apr-1");
        assert_eq!(bootstrap.pending_approvals[0].action, "sensitive_op");

        let resolved = SessionEvent::ApprovalResolved {
            approval_id: "apr-1".into(),
            outcome: roz_core::session::feedback::ApprovalOutcome::Approved,
        };
        apply_runtime_event_to_checkpoint(&mut bootstrap, &resolved);

        assert!(approval_checkpoint_event(&resolved));
        assert!(bootstrap.pending_approvals.is_empty());
    }

    #[tokio::test]
    async fn edge_session_spatial_provider_bootstraps_registered_cameras() {
        let hub = StreamHub::new();
        let mut manager = CameraManager::new(hub);
        manager.add_test_pattern().await;
        let provider = EdgeSessionSpatialProvider::new(Some(Arc::new(manager)));

        let snapshot = provider.snapshot("task-1").await;

        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.entities[0].id, "camera:test-pattern");
        assert_eq!(snapshot.entities[0].kind, "camera_sensor");
    }

    #[test]
    fn edge_runtime_spatial_note_makes_react_suppression_explicit() {
        let note = edge_runtime_spatial_note(
            AgentLoopMode::React,
            &WorldState {
                entities: vec![EntityState {
                    id: "camera:test-pattern".into(),
                    kind: "camera_sensor".into(),
                    frame_id: "world".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            true,
        );

        assert!(note.contains("status=available"));
        assert!(note.contains("Current turn mode is React"));
    }
}
