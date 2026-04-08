use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_nats::jetstream::Context as JetStreamContext;
use futures::StreamExt;
use roz_agent::agent_loop::{AgentInputSeed, AgentLoop, AgentOutput, PresenceSignal};
use roz_agent::error::AgentError;
use roz_agent::model::types::StreamChunk;
use roz_agent::session_runtime::{
    ApprovalRuntimeHandle, PreparedTurn, SessionConfig, SessionRuntime, SessionRuntimeError, StreamingTurnExecutor,
    StreamingTurnHandle, StreamingTurnResult, TurnExecutionFailure, TurnOutput,
};
use roz_agent::spatial_provider::{NullWorldStateProvider, PrimedWorldStateProvider, WorldStateProvider};
use roz_core::session::activity::RuntimeFailureKind;
use roz_core::session::event::{EventEnvelope, SessionEvent};
use roz_core::session::feedback::ApprovalOutcome;
use roz_core::team::{SequencedTeamEvent, TeamEvent};
use roz_nats::dispatch::{TaskInvocation, TaskStatusEvent, TaskTerminalStatus};
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

fn validate_control_interface_manifest(invocation: &TaskInvocation) -> Result<()> {
    if matches!(invocation.mode, roz_nats::dispatch::ExecutionMode::OodaReAct)
        && invocation.control_interface_manifest.is_none()
    {
        return Err(anyhow::anyhow!("OODA task missing control_interface_manifest"));
    }
    Ok(())
}

fn decode_team_event_payload(payload: &[u8]) -> Option<TeamEvent> {
    if let Ok(sequenced) = serde_json::from_slice::<SequencedTeamEvent>(payload) {
        return Some(sequenced.event);
    }

    serde_json::from_slice::<TeamEvent>(payload).ok()
}

async fn publish_task_status(nats: &async_nats::Client, event: &TaskStatusEvent) {
    let subject = roz_nats::dispatch::task_status_subject(event.task_id);
    match serde_json::to_vec(event) {
        Ok(payload) => {
            if let Err(error) = nats.publish(subject, payload.into()).await {
                tracing::warn!(%error, task_id = %event.task_id, status = %event.status, "failed to publish task status");
            }
        }
        Err(error) => {
            tracing::warn!(%error, task_id = %event.task_id, status = %event.status, "failed to serialize task status");
        }
    }
}

fn classify_terminal_status(
    output: &Result<AgentOutput, AgentError>,
    timed_out: bool,
) -> (TaskTerminalStatus, Option<String>) {
    if timed_out {
        let detail = output
            .as_ref()
            .err()
            .map_or_else(|| "task timed out".to_string(), ToString::to_string);
        return (TaskTerminalStatus::TimedOut, Some(detail));
    }

    match output {
        Ok(_) => (TaskTerminalStatus::Succeeded, None),
        Err(AgentError::Cancelled { .. }) => (TaskTerminalStatus::Cancelled, Some("task cancelled".into())),
        Err(AgentError::Safety(message)) => (TaskTerminalStatus::SafetyStop, Some(message.clone())),
        Err(error) => (TaskTerminalStatus::Failed, Some(error.to_string())),
    }
}

struct WorkerTaskStreamingExecutor {
    agent: Option<AgentLoop>,
    agent_input: roz_agent::agent_loop::AgentInput,
    cancellation: CancellationToken,
    estop_rx: tokio::sync::watch::Receiver<bool>,
}

impl StreamingTurnExecutor for WorkerTaskStreamingExecutor {
    fn execute_turn_streaming(&mut self, prepared: PreparedTurn) -> StreamingTurnHandle<'_> {
        let cognition_mode = prepared.cognition_mode();
        let (chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (presence_tx, presence_rx) = mpsc::channel::<PresenceSignal>(16);
        let cancellation = self.cancellation.clone();
        let mut estop_rx = self.estop_rx.clone();
        let system_prompt: Vec<String> = prepared.system_blocks.into_iter().map(|block| block.content).collect();
        let seed = AgentInputSeed::new(system_prompt, prepared.history, prepared.user_message);
        let mut agent_input = self.agent_input.clone();
        agent_input.mode = cognition_mode;
        let mut agent = self.agent.take().expect("streaming executor called more than once");

        StreamingTurnHandle {
            completion: Box::pin(async move {
                let result = tokio::select! {
                    result = agent.run_streaming_seeded(agent_input, seed, chunk_tx, presence_tx) => result,
                    () = cancellation.cancelled() => {
                        Err(AgentError::Cancelled {
                            partial_input_tokens: 0,
                            partial_output_tokens: 0,
                        })
                    }
                    changed = estop_rx.changed() => {
                        if changed.is_ok() && *estop_rx.borrow() {
                            cancellation.cancel();
                            Err(AgentError::Safety("E-STOP activated during task execution".into()))
                        } else {
                            Err(AgentError::Internal(anyhow::anyhow!(
                                "worker estop watch fired without activation"
                            )))
                        }
                    }
                };

                match result {
                    Ok(output) => Ok(TurnOutput {
                        assistant_message: output.final_response.unwrap_or_default(),
                        tool_calls_made: output.cycles,
                        input_tokens: u64::from(output.total_usage.input_tokens),
                        output_tokens: u64::from(output.total_usage.output_tokens),
                        cache_read_tokens: u64::from(output.total_usage.cache_read_tokens),
                        cache_creation_tokens: u64::from(output.total_usage.cache_creation_tokens),
                        messages: output.messages,
                    }),
                    Err(error) => Err(Box::new(agent_error_to_turn_execution_failure(error))
                        as Box<dyn std::error::Error + Send + Sync>),
                }
            }),
            chunk_rx,
            presence_rx,
            tool_call_rx: None,
        }
    }
}

fn agent_error_to_turn_execution_failure(error: AgentError) -> TurnExecutionFailure {
    match error {
        AgentError::Safety(message) => TurnExecutionFailure::new(RuntimeFailureKind::SafetyBlocked, message),
        AgentError::ToolDispatch { message, .. } => TurnExecutionFailure::new(RuntimeFailureKind::ToolError, message),
        AgentError::CircuitBreakerTripped {
            consecutive_error_turns,
        } => TurnExecutionFailure::new(
            RuntimeFailureKind::CircuitBreakerTripped,
            format!("circuit breaker tripped after {consecutive_error_turns} consecutive all-error turns"),
        ),
        AgentError::Cancelled { .. } => TurnExecutionFailure::new(RuntimeFailureKind::OperatorAbort, "turn cancelled"),
        other => TurnExecutionFailure::new(RuntimeFailureKind::ModelError, other.to_string()),
    }
}

fn session_runtime_error_to_agent_error(error: &SessionRuntimeError, estop_active: bool) -> AgentError {
    match error {
        SessionRuntimeError::SessionPaused => AgentError::Safety("session paused".into()),
        SessionRuntimeError::SessionCompleted => AgentError::Internal(anyhow::anyhow!("session already completed")),
        SessionRuntimeError::TurnFailed(RuntimeFailureKind::OperatorAbort)
        | SessionRuntimeError::SessionFailed(RuntimeFailureKind::OperatorAbort) => AgentError::Cancelled {
            partial_input_tokens: 0,
            partial_output_tokens: 0,
        },
        SessionRuntimeError::TurnFailed(RuntimeFailureKind::SafetyBlocked)
        | SessionRuntimeError::SessionFailed(RuntimeFailureKind::SafetyBlocked)
            if estop_active =>
        {
            AgentError::Safety("E-STOP activated during task execution".into())
        }
        SessionRuntimeError::TurnFailed(failure) => {
            AgentError::Internal(anyhow::anyhow!("turn runtime failed: {failure:?}"))
        }
        SessionRuntimeError::SessionFailed(failure) => {
            AgentError::Internal(anyhow::anyhow!("session runtime failed: {failure:?}"))
        }
    }
}

async fn relay_worker_approval_events(
    mut event_rx: broadcast::Receiver<EventEnvelope>,
    task_js: JetStreamContext,
    parent_task_id: Uuid,
    worker_task_id: Uuid,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            event = event_rx.recv() => {
                match event {
                    Ok(envelope) => match envelope.event {
                        SessionEvent::ApprovalRequested {
                            approval_id,
                            action,
                            reason,
                            timeout_secs,
                        } => {
                            let event = roz_core::team::TeamEvent::WorkerApprovalRequested {
                                worker_id: worker_task_id,
                                task_id: worker_task_id,
                                approval_id,
                                tool_name: action,
                                reason,
                                timeout_secs,
                            };
                            if let Err(error) = roz_nats::team::publish_team_event(
                                &task_js,
                                parent_task_id,
                                worker_task_id,
                                &event,
                            )
                            .await
                            {
                                tracing::warn!(%error, task_id = %worker_task_id, "failed to publish worker approval request event");
                            }
                        }
                        SessionEvent::ApprovalResolved {
                            approval_id,
                            outcome,
                        } => {
                            let approved = !matches!(outcome, ApprovalOutcome::Denied { .. });
                            let event = roz_core::team::TeamEvent::WorkerApprovalResolved {
                                worker_id: worker_task_id,
                                task_id: worker_task_id,
                                approval_id,
                                approved,
                                modifier: None,
                            };
                            if let Err(error) = roz_nats::team::publish_team_event(
                                &task_js,
                                parent_task_id,
                                worker_task_id,
                                &event,
                            )
                            .await
                            {
                                tracing::warn!(%error, task_id = %worker_task_id, "failed to publish worker approval resolved event");
                            }
                        }
                        _ => {}
                    },
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(task_id = %worker_task_id, skipped, "worker runtime approval event stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn consume_team_approval_events(
    task_js: JetStreamContext,
    approval_owner_task_id: Uuid,
    worker_task_id: Uuid,
    approval_runtime: ApprovalRuntimeHandle,
    cancel: CancellationToken,
) {
    let stream = match task_js.get_stream(roz_nats::team::TEAM_STREAM).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::error!(%error, %approval_owner_task_id, task_id = %worker_task_id, "failed to open team event stream for approvals");
            return;
        }
    };
    let consumer = match stream
        .create_consumer(async_nats::jetstream::consumer::push::OrderedConfig {
            filter_subject: roz_nats::team::worker_subject(approval_owner_task_id, worker_task_id),
            ..Default::default()
        })
        .await
    {
        Ok(consumer) => consumer,
        Err(error) => {
            tracing::error!(%error, %approval_owner_task_id, task_id = %worker_task_id, "failed to create team approval consumer");
            return;
        }
    };
    let mut messages = match consumer.messages().await {
        Ok(messages) => messages,
        Err(error) => {
            tracing::error!(%error, %approval_owner_task_id, task_id = %worker_task_id, "failed to subscribe to team approval events");
            return;
        }
    };

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            maybe_msg = messages.next() => {
                let Some(message) = maybe_msg else { break; };
                let msg = match message {
                    Ok(msg) => msg,
                    Err(error) => {
                        tracing::warn!(%error, %approval_owner_task_id, task_id = %worker_task_id, "team approval message stream error");
                        break;
                    }
                };
                if let Err(error) = msg.ack().await {
                    tracing::warn!(%error, %approval_owner_task_id, task_id = %worker_task_id, "failed to ack team approval event");
                }
                let Some(event) = decode_team_event_payload(&msg.payload) else {
                    tracing::warn!(%approval_owner_task_id, task_id = %worker_task_id, "failed to decode team approval event");
                    continue;
                };
                if let TeamEvent::WorkerApprovalResolved {
                    worker_id,
                    approval_id,
                    approved,
                    modifier,
                    ..
                } = event
                {
                    if worker_id != worker_task_id {
                        continue;
                    }
                    let resolved = approval_runtime.resolve_approval(&approval_id, approved, modifier);
                    if !resolved {
                        if approval_owner_task_id == worker_task_id {
                            tracing::debug!(
                                task_id = %worker_task_id,
                                approval_id = %approval_id,
                                approved,
                                "ignoring self-owned approval event without a pending worker approval"
                            );
                        } else {
                            tracing::warn!(
                                task_id = %worker_task_id,
                                approval_id = %approval_id,
                                approved,
                                "team approval event did not match a pending worker approval"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Run the agent loop for a single task, publish `WorkerExited` to the parent's team stream
/// if this is a child task, then signal the result back to Restate.
#[expect(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "sequential task lifecycle with model + tools + safety"
)]
async fn execute_task(
    invocation: TaskInvocation,
    task_id: Uuid,
    task_config: roz_worker::config::WorkerConfig,
    task_nats: async_nats::Client,
    task_js: JetStreamContext,
    task_http: reqwest::Client,
    restate_url: String,
    estop_rx: tokio::sync::watch::Receiver<bool>,
    camera_manager: Option<Arc<roz_worker::camera::CameraManager>>,
) {
    tracing::info!("starting task execution");

    if invocation.parent_task_id.is_some() && invocation.delegation_scope.is_none() {
        tracing::error!(task_id = %task_id, "child worker invocation missing delegation scope");
        publish_task_status(
            &task_nats,
            &TaskStatusEvent {
                task_id,
                status: "failed".into(),
                detail: Some("child worker invocation missing delegation scope".into()),
                host_id: Some(invocation.host_id),
            },
        )
        .await;
        let result = roz_worker::dispatch::build_task_result(
            task_id,
            TaskTerminalStatus::Failed,
            Err(roz_agent::error::AgentError::Safety(
                "child worker invocation missing delegation scope".into(),
            )),
        );
        if let Err(e) =
            roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await
        {
            tracing::error!(error = %e, "failed to signal missing-delegation-scope result to Restate");
        }
        return;
    }

    if let Err(error) = validate_control_interface_manifest(&invocation) {
        tracing::error!(task_id = %task_id, %error, "invalid control interface manifest for task");
        publish_task_status(
            &task_nats,
            &TaskStatusEvent {
                task_id,
                status: "failed".into(),
                detail: Some(error.to_string()),
                host_id: Some(invocation.host_id),
            },
        )
        .await;
        let result = roz_worker::dispatch::build_task_result(
            task_id,
            TaskTerminalStatus::Failed,
            Err(roz_agent::error::AgentError::Safety(error.to_string())),
        );
        if let Err(e) =
            roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await
        {
            tracing::error!(error = %e, "failed to signal missing-control-interface-manifest result to Restate");
        }
        return;
    }

    let task_agent_cancel = CancellationToken::new();
    let model = match roz_worker::model_factory::build_model(&task_config, None) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "failed to build model for task, aborting");
            publish_task_status(
                &task_nats,
                &TaskStatusEvent {
                    task_id,
                    status: "failed".into(),
                    detail: Some(format!("failed to build model: {e}")),
                    host_id: Some(invocation.host_id),
                },
            )
            .await;
            let agent_err = roz_agent::error::AgentError::Model(e.into());
            let result = roz_worker::dispatch::build_task_result(task_id, TaskTerminalStatus::Failed, Err(agent_err));
            if let Err(sig_err) =
                roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await
            {
                tracing::error!(error = %sig_err, "failed to signal model-build failure to Restate");
            }
            return;
        }
    };

    let mut dispatcher = roz_agent::dispatch::ToolDispatcher::new(Duration::from_secs(30));
    let guards: Vec<Box<dyn roz_agent::safety::SafetyGuard>> = vec![Box::new(
        roz_agent::safety::guards::VelocityLimiter::new(task_config.max_velocity.unwrap_or(1.5)),
    )];
    let safety = roz_agent::safety::SafetyStack::new(guards);

    // Spawn Copper controller for OodaReAct mode.
    //
    // Worker task invocations currently carry the canonical control contract
    // but not a compiled EmbodimentRuntime or runtime-owned rollout policy, so
    // the worker must stay on the execution boundary only.
    let mut copper_handle = match invocation.mode {
        roz_nats::dispatch::ExecutionMode::OodaReAct => {
            let max_velocity = task_config.max_velocity.unwrap_or(1.5);
            let handle = roz_worker::copper_handle::CopperHandle::spawn_execution_only(max_velocity);
            tracing::info!("copper controller spawned for OodaReAct task in execution-only mode");
            Some(handle)
        }
        roz_nats::dispatch::ExecutionMode::React => None,
    };

    let spatial: Arc<dyn WorldStateProvider> = if let Some(ref handle) = copper_handle {
        Arc::new(roz_worker::spatial_bridge::CopperSpatialProvider::new(Arc::clone(
            handle.state(),
        )))
    } else {
        Arc::new(NullWorldStateProvider)
    };
    let primed_spatial_context = if matches!(invocation.mode, roz_nats::dispatch::ExecutionMode::OodaReAct) {
        Some(spatial.snapshot(&task_id.to_string()).await)
    } else {
        None
    };
    let spatial: Box<dyn WorldStateProvider> = if let Some(context) = primed_spatial_context.clone() {
        Box::new(PrimedWorldStateProvider::new(Box::new(spatial.clone()), context))
    } else {
        Box::new(spatial.clone())
    };

    // When Copper is active, keep the canonical control manifest in Extensions
    // and avoid fabricating the legacy channel manifest here. Legacy lowering
    // is still available inside the last-mile Copper host paths that require it.
    let mut extensions = roz_agent::dispatch::Extensions::new();
    if let Some(ref handle) = copper_handle {
        let control_manifest = invocation
            .control_interface_manifest
            .clone()
            .expect("ooda tasks must be validated to carry control_interface_manifest");
        extensions.insert(handle.cmd_tx());
        extensions.insert(control_manifest);
    }

    // Register camera perception tools when cameras are available.
    if let Some(ref cam_mgr) = camera_manager {
        extensions.insert(cam_mgr.clone());
        let shared_vision_config = Arc::new(tokio::sync::RwLock::new(roz_core::edge::vision::VisionConfig::default()));
        extensions.insert(shared_vision_config);
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::CaptureFrameTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::ListCamerasTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::SetVisionStrategyTool),
            roz_core::tools::ToolCategory::Pure,
        );
        tracing::info!("camera perception tools registered");
    }

    // Register team tools (spawn_worker, watch_team) when this is an orchestrator
    // (no parent_task_id). Workers cannot spawn their own workers.
    if invocation.parent_task_id.is_none() {
        if let Ok(tenant_uuid) = invocation.tenant_id.parse::<Uuid>() {
            dispatcher.register_with_category(
                Box::new(roz_agent::tools::spawn_worker::SpawnWorkerTool::new(
                    task_nats.clone(),
                    task_id,
                    invocation.environment_id,
                    task_js.clone(),
                    tenant_uuid,
                )),
                roz_core::tools::ToolCategory::Pure,
            );
            dispatcher.register_with_category(
                Box::new(roz_agent::tools::watch_team::WatchTeamTool::new(
                    task_js.clone(),
                    task_id,
                )),
                roz_core::tools::ToolCategory::Pure,
            );
            tracing::info!("team tools registered (orchestrator mode)");
        } else {
            tracing::warn!(
                tenant_id = %invocation.tenant_id,
                "skipping team tool registration: tenant_id is not a valid UUID"
            );
        }
    }

    if let Some(scope) = &invocation.delegation_scope {
        roz_worker::dispatch::apply_allowed_tools(&mut dispatcher, &scope.allowed_tools);
        roz_worker::dispatch::apply_trust_posture(&mut dispatcher, &scope.trust_posture);
    }

    let effective_delegation_scope =
        invocation
            .delegation_scope
            .clone()
            .unwrap_or_else(|| roz_core::tasks::DelegationScope {
                allowed_tools: dispatcher.tool_names(),
                trust_posture: roz_core::trust::TrustPosture::default(),
            });
    extensions.insert(effective_delegation_scope);

    let task_sidecars_cancel = CancellationToken::new();

    let turn_input = roz_worker::dispatch::build_turn_input(&invocation, &dispatcher);
    let prompt_state = roz_worker::dispatch::build_prompt_state(&invocation, &dispatcher);
    let agent_input = roz_worker::dispatch::build_runtime_shell_input(&invocation, Some(task_agent_cancel.clone()));
    let session_config = SessionConfig {
        session_id: task_id.to_string(),
        tenant_id: invocation.tenant_id.clone(),
        mode: roz_core::session::control::SessionMode::Edge,
        cognition_mode: roz_worker::dispatch::effective_cognition_mode(&invocation),
        constitution_text: prompt_state.constitution_text,
        blueprint_toml: String::new(),
        model_name: Some(task_config.model_name.clone()),
        permissions: roz_worker::dispatch::derive_session_permissions(&dispatcher),
        tool_schemas: prompt_state.tool_schemas,
        project_context: prompt_state.project_context,
        initial_history: Vec::new(),
    };
    let mut session_runtime = SessionRuntime::new(&session_config);
    let approval_runtime = session_runtime.approval_handle();

    if let Some(parent_task_id) = invocation.parent_task_id {
        tokio::spawn(consume_team_approval_events(
            task_js.clone(),
            parent_task_id,
            task_id,
            approval_runtime.clone(),
            task_sidecars_cancel.clone(),
        ));
    } else {
        tokio::spawn(relay_worker_approval_events(
            session_runtime.subscribe_events(),
            task_js.clone(),
            task_id,
            task_id,
            task_sidecars_cancel.clone(),
        ));
        tokio::spawn(consume_team_approval_events(
            task_js.clone(),
            task_id,
            task_id,
            approval_runtime.clone(),
            task_sidecars_cancel.clone(),
        ));
    }

    if let Some(parent_task_id) = invocation.parent_task_id {
        tokio::spawn(relay_worker_approval_events(
            session_runtime.subscribe_events(),
            task_js.clone(),
            parent_task_id,
            task_id,
            task_sidecars_cancel.clone(),
        ));
    }

    let agent = AgentLoop::new(model, dispatcher, safety, spatial)
        .with_extensions(extensions)
        .with_approval_runtime(approval_runtime);
    let mut executor = WorkerTaskStreamingExecutor {
        agent: Some(agent),
        agent_input,
        cancellation: task_agent_cancel.clone(),
        estop_rx: estop_rx.clone(),
    };
    let _ = session_runtime.start_session().await;
    session_runtime.sync_world_state(primed_spatial_context);
    publish_task_status(
        &task_nats,
        &TaskStatusEvent {
            task_id,
            status: "running".into(),
            detail: Some("worker accepted invocation".into()),
            host_id: Some(invocation.host_id),
        },
    )
    .await;
    let timeout_secs = u64::from(invocation.timeout_secs.max(1));
    let mut timed_out = false;
    let output = match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        session_runtime.run_turn_streaming(turn_input, None, &mut executor),
    )
    .await
    {
        Ok(Ok(StreamingTurnResult::Completed(turn_output))) => {
            Ok(roz_worker::dispatch::build_agent_output_from_turn_output(turn_output))
        }
        Ok(Ok(StreamingTurnResult::Cancelled)) => Err(AgentError::Cancelled {
            partial_input_tokens: 0,
            partial_output_tokens: 0,
        }),
        Ok(Err(error)) => {
            if *estop_rx.borrow() {
                tracing::error!(task_id = %task_id, "E-STOP during task execution");
                drop(copper_handle.take());
            }
            Err(session_runtime_error_to_agent_error(&error, *estop_rx.borrow()))
        }
        Err(_) => {
            timed_out = true;
            task_agent_cancel.cancel();
            Err(AgentError::Internal(anyhow::anyhow!(
                "task timed out after {timeout_secs}s"
            )))
        }
    };

    task_sidecars_cancel.cancel();

    // If this is a child task (has a parent), notify the parent's team stream that this
    // child worker has exited. Complements WorkerCompleted/WorkerFailed which are published
    // earlier in the model result path.
    if let Some(parent_task_id) = invocation.parent_task_id {
        let event = roz_core::team::TeamEvent::WorkerExited {
            worker_id: task_id,
            parent_task_id,
        };
        if let Err(e) = roz_nats::team::publish_team_event(&task_js, parent_task_id, task_id, &event).await {
            tracing::warn!(
                error = %e,
                %task_id,
                %parent_task_id,
                "failed to publish WorkerExited"
            );
        }
    }

    let (terminal_status, terminal_detail) = classify_terminal_status(&output, timed_out);
    let result = roz_worker::dispatch::build_task_result(task_id, terminal_status, output);
    publish_task_status(
        &task_nats,
        &TaskStatusEvent {
            task_id,
            status: terminal_status.as_str().into(),
            detail: terminal_detail.or_else(|| result.error.clone()),
            host_id: Some(invocation.host_id),
        },
    )
    .await;

    if let Err(e) = roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await {
        tracing::error!(error = %e, "failed to signal result to Restate");
    }

    // Shut down Copper if it was spawned.
    if let Some(handle) = copper_handle {
        handle.shutdown().await;
    }
}

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "sequential startup with telemetry + capabilities + task loop"
)]
async fn main() -> Result<()> {
    let logfire = logfire::configure()
        .with_service_name("roz-worker")
        .with_service_version(env!("CARGO_PKG_VERSION"))
        .with_environment(std::env::var("ROZ_ENVIRONMENT").unwrap_or_else(|_| "development".into()))
        .finish()
        .expect("failed to configure logfire");
    let _guard = logfire.shutdown_guard();

    let config = roz_worker::config::WorkerConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;

    tracing::info!(worker_id = %config.worker_id, "starting roz-worker");
    let task_slots = Arc::new(Semaphore::new(config.max_concurrent_tasks));
    tracing::info!(
        max_concurrent_tasks = config.max_concurrent_tasks,
        "task admission slots configured"
    );

    // Connect to NATS
    let nats = async_nats::connect(&config.nats_url).await?;
    tracing::info!(nats_url = %config.nats_url, "connected to NATS");
    let js = async_nats::jetstream::new(nats.clone());

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");

    // Publish heartbeat on interval
    let hb_nats = nats.clone();
    let hb_worker_id = config.worker_id.clone();
    tokio::spawn(async move {
        let subject = roz_nats::subjects::Subjects::event(&hb_worker_id, "heartbeat").expect("valid worker_id");
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(e) = hb_nats.publish(subject.clone(), bytes::Bytes::from_static(b"{}")).await {
                tracing::warn!(error = %e, "failed to publish heartbeat");
            }
        }
    });

    // Spawn telemetry publisher (10 Hz)
    let telem_nats = nats.clone();
    let telem_worker_id = config.worker_id.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            let state = serde_json::json!({
                "timestamp": chrono::Utc::now().timestamp_millis(),
                "joints": [],
                "sensors": {}
            });
            if let Err(e) = roz_worker::telemetry::publish_state(&telem_nats, &telem_worker_id, &state).await {
                tracing::trace!(error = %e, "telemetry publish failed");
            }
        }
    });

    // Initialize camera system
    let camera_manager: Option<Arc<roz_worker::camera::CameraManager>> =
        if config.camera.enabled || config.camera.test_pattern {
            let hub = roz_worker::camera::stream_hub::StreamHub::new();
            let mut manager = roz_worker::camera::CameraManager::new(hub);
            if config.camera.test_pattern {
                let cam_info = manager.add_test_pattern().await;
                tracing::info!(camera = %cam_info.id, "test pattern camera registered");
            }
            Some(Arc::new(manager))
        } else {
            tracing::info!("camera system disabled");
            None
        };

    // Publish capabilities on startup
    let mut caps = roz_core::capabilities::RobotCapabilities {
        robot_type: "generic".to_string(),
        joints: vec![],
        control_modes: vec!["position".to_string(), "velocity".to_string()],
        workspace_bounds: None,
        sensors: vec![],
        max_velocity: config.max_velocity.unwrap_or(1.5),
        cameras: vec![],
    };

    if let Some(ref cam_mgr) = camera_manager {
        caps.cameras = cam_mgr
            .cameras()
            .iter()
            .map(|c| roz_core::capabilities::CameraCapability {
                id: c.id.0.clone(),
                label: c.label.clone(),
                resolution: [
                    c.supported_resolutions.first().map_or(640, |r| r.0),
                    c.supported_resolutions.first().map_or(480, |r| r.1),
                ],
                fps: c.max_fps,
                hw_encoder: c.hw_encoder_available,
            })
            .collect();
    }
    let caps_subject =
        roz_nats::subjects::Subjects::capabilities(&config.worker_id).expect("valid worker_id for capabilities");
    if let Ok(payload) = serde_json::to_vec(&caps)
        && let Err(e) = nats.publish(caps_subject, payload.into()).await
    {
        tracing::warn!(error = %e, "failed to publish capabilities");
    }

    // Subscribe to e-stop events
    let estop_sub = roz_worker::estop::subscribe_estop(&nats, &config.worker_id).await?;
    let estop_rx = roz_worker::estop::spawn_estop_listener(estop_sub);
    tracing::info!(worker_id = %config.worker_id, "e-stop listener active");

    // Spawn idle watchdog — fires if no NATS message arrives within 30s.
    let watchdog = Arc::new(roz_worker::command_watchdog::CommandWatchdog::new(Duration::from_secs(
        30,
    )));
    let watchdog_cancel = CancellationToken::new();
    let wd = watchdog.clone();
    let wd_cancel = watchdog_cancel.clone();
    tokio::spawn(async move { wd.run(wd_cancel).await });
    tracing::info!("idle watchdog active (30s deadline)");

    // Load embodiment model from robot.toml if configured (D-01, D-02)
    let embodiment_model: Option<roz_core::embodiment::model::EmbodimentModel> =
        config.robot_toml.as_ref().and_then(|toml_path| {
            match roz_core::manifest::EmbodimentManifest::load(std::path::Path::new(toml_path)) {
                Ok(manifest) => {
                    if let Some(rt) = manifest.embodiment_runtime() {
                        tracing::info!(path = %toml_path, digest = %rt.model.model_digest, "loaded embodiment model from manifest");
                        Some(rt.model)
                    } else {
                        tracing::info!(path = %toml_path, "manifest has no channels section, skipping embodiment upload");
                        None
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %toml_path, error = %e, "failed to load embodiment manifest");
                    None
                }
            }
        });

    // Register with server
    if !config.api_key.is_empty() {
        match roz_worker::registration::register_host(&config.api_url, &config.api_key, &config.worker_id).await {
            Ok(host_id) => {
                tracing::info!(host_id = %host_id, "registered with server");
                // Upload embodiment model if available (D-04: log-and-continue, D-05: None for runtime)
                if let Some(ref model) = embodiment_model {
                    match roz_worker::registration::upload_embodiment(
                        &http,
                        &config.api_url,
                        &config.api_key,
                        host_id,
                        model,
                        None,
                    )
                    .await
                    {
                        Ok(()) => tracing::info!(host_id = %host_id, "embodiment model uploaded"),
                        Err(e) => tracing::warn!(host_id = %host_id, error = %e, "embodiment upload failed"),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "host registration failed"),
        }
    }

    // Spawn edge agent session relay (handles gRPC sessions relayed via NATS).
    let relay_nats = nats.clone();
    let relay_worker_id = config.worker_id.clone();
    let relay_config = config.clone();
    let relay_estop_rx = estop_rx.clone();
    let relay_camera_mgr = camera_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = roz_worker::session_relay::spawn_session_relay(
            relay_nats,
            relay_worker_id,
            relay_config,
            relay_estop_rx,
            relay_camera_mgr,
        )
        .await
        {
            tracing::error!(error = %e, "session relay exited");
        }
    });

    // Subscribe to task invocations
    let worker_id = &config.worker_id;
    let subject = format!("invoke.{worker_id}.>");
    let mut sub = nats.subscribe(subject.clone()).await?;
    tracing::info!(subject, "subscribed to invocations, waiting for tasks");

    let restate_url = config.restate_url.clone();

    while let Some(msg) = sub.next().await {
        watchdog.pet();

        if *estop_rx.borrow() {
            tracing::error!("E-STOP active — rejecting task invocation");
            continue;
        }

        tracing::info!(
            subject = %msg.subject,
            bytes = msg.payload.len(),
            "received invocation"
        );

        let invocation: TaskInvocation = match serde_json::from_slice(&msg.payload) {
            Ok(inv) => inv,
            Err(e) => {
                tracing::error!(error = %e, "failed to deserialize TaskInvocation");
                continue;
            }
        };

        if let Some(ref tp) = invocation.traceparent {
            tracing::info!(traceparent = %tp, task_id = %invocation.task_id, "linking to server trace");
        }

        tracing::info!(
            task_id = %invocation.task_id,
            tenant_id = %invocation.tenant_id,
            mode = ?invocation.mode,
            "dispatching task"
        );

        let task_nats = nats.clone();
        let task_http = http.clone();
        let restate_url = restate_url.clone();
        let task_id = invocation.task_id;
        let task_config = config.clone();
        let task_js = js.clone();
        let task_camera_mgr = camera_manager.clone();

        let task_estop_rx = estop_rx.clone();
        let Ok(task_permit) = task_slots.clone().try_acquire_owned() else {
            tracing::warn!(
                task_id = %task_id,
                max_concurrent_tasks = config.max_concurrent_tasks,
                "worker saturated; rejecting task"
            );
            let error = format!(
                "worker saturated: max_concurrent_tasks={} exhausted",
                config.max_concurrent_tasks
            );
            publish_task_status(
                &task_nats,
                &TaskStatusEvent {
                    task_id,
                    status: "failed".into(),
                    detail: Some(error.clone()),
                    host_id: Some(invocation.host_id),
                },
            )
            .await;
            let result = roz_worker::dispatch::build_task_result(
                task_id,
                TaskTerminalStatus::Failed,
                Err(AgentError::Internal(anyhow::anyhow!(error))),
            );
            if let Err(sig_err) =
                roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await
            {
                tracing::error!(error = %sig_err, task_id = %task_id, "failed to signal saturation result");
            }
            continue;
        };
        let span = tracing::info_span!("worker.execute_task", task_id = %task_id);
        tokio::spawn(
            async move {
                let _task_permit = task_permit;
                execute_task(
                    invocation,
                    task_id,
                    task_config,
                    task_nats,
                    task_js,
                    task_http,
                    restate_url,
                    task_estop_rx,
                    task_camera_mgr,
                )
                .await;
            }
            .instrument(span),
        );
    }

    tracing::warn!("NATS subscription closed, worker exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::team::TeamEvent as CoreTeamEvent;

    fn sample_invocation(mode: roz_nats::dispatch::ExecutionMode) -> TaskInvocation {
        TaskInvocation {
            task_id: Uuid::new_v4(),
            tenant_id: "tenant".into(),
            prompt: "test".into(),
            environment_id: Uuid::new_v4(),
            safety_policy_id: None,
            host_id: Uuid::new_v4(),
            timeout_secs: 30,
            mode,
            parent_task_id: None,
            restate_url: "http://localhost:8080".into(),
            traceparent: None,
            phases: vec![],
            control_interface_manifest: None,
            delegation_scope: None,
        }
    }

    #[test]
    fn ooda_tasks_require_control_interface_manifest() {
        let invocation = sample_invocation(roz_nats::dispatch::ExecutionMode::OodaReAct);
        let err = validate_control_interface_manifest(&invocation).expect_err("ooda task should be rejected");
        assert!(err.to_string().contains("control_interface_manifest"));
    }

    #[test]
    fn react_tasks_do_not_require_control_interface_manifest() {
        let invocation = sample_invocation(roz_nats::dispatch::ExecutionMode::React);
        validate_control_interface_manifest(&invocation).expect("react task should be allowed");
    }

    #[test]
    fn decode_team_event_payload_accepts_sequenced_wrapper() {
        let worker_id = Uuid::new_v4();
        let payload = serde_json::to_vec(&SequencedTeamEvent {
            seq: 7,
            timestamp_ns: 99,
            event: CoreTeamEvent::WorkerApprovalResolved {
                worker_id,
                task_id: worker_id,
                approval_id: "apr_worker".into(),
                approved: true,
                modifier: None,
            },
        })
        .expect("serialize sequenced worker event");

        match decode_team_event_payload(&payload).expect("decode worker event") {
            CoreTeamEvent::WorkerApprovalResolved {
                worker_id: id,
                approval_id,
                approved,
                ..
            } => {
                assert_eq!(id, worker_id);
                assert_eq!(approval_id, "apr_worker");
                assert!(approved);
            }
            other => panic!("expected WorkerApprovalResolved, got {other:?}"),
        }
    }

    #[test]
    fn decode_team_event_payload_still_accepts_legacy_payload() {
        let worker_id = Uuid::new_v4();
        let payload = serde_json::to_vec(&CoreTeamEvent::WorkerStarted {
            worker_id,
            host_id: "legacy-host".into(),
        })
        .expect("serialize legacy worker event");

        match decode_team_event_payload(&payload).expect("decode legacy worker event") {
            CoreTeamEvent::WorkerStarted { worker_id: id, host_id } => {
                assert_eq!(id, worker_id);
                assert_eq!(host_id, "legacy-host");
            }
            other => panic!("expected WorkerStarted, got {other:?}"),
        }
    }
}
