use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_nats::jetstream::Context as JetStreamContext;
use futures::StreamExt;
use parking_lot::RwLock;
use roz_agent::agent_loop::{AgentInputSeed, AgentLoop, AgentOutput, PresenceSignal};
use roz_agent::error::AgentError;
use roz_agent::model::types::StreamChunk;
use roz_agent::session_runtime::{
    ApprovalRuntimeHandle, PreparedTurn, SessionConfig, SessionRuntime, SessionRuntimeError, StreamingTurnExecutor,
    StreamingTurnHandle, StreamingTurnResult, TurnExecutionFailure, TurnOutput,
};
use roz_agent::spatial_provider::{NullWorldStateProvider, PrimedWorldStateProvider, WorldStateProvider};
use roz_core::reconnect::WorkerOnlineSnapshot;
use roz_core::session::activity::RuntimeFailureKind;
use roz_core::session::event::{EventEnvelope, SessionEvent};
use roz_core::session::feedback::ApprovalOutcome;
use roz_core::signing::{HEADER_NAME, SignatureError};
use roz_core::team::{SequencedTeamEvent, TeamEvent};
use roz_nats::dispatch::{TaskInvocation, TaskStatusEvent, TaskTerminalStatus};
use roz_worker::signing_hooks::{WorkerSigningContext, WorkerSigningError};
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

/// Phase 25 D-03 / D-11 / D-12: construct the per-task MAVLink backend when
/// `[mavlink]` config is present.
///
/// Scope guard (Phase 25): this helper constructs `MavlinkBackend` against the
/// configured transport and returns it wrapped in `Arc` for storage on the
/// worker's task-local state. It does NOT:
///   - perform DB-backed signing-key decryption (that is scoped to the
///     Phase 27 agent-loop integration that needs live-FCU access); the full
///     path is `roz_db::hosts::get_mavlink_signing_key(pool, host_id)` →
///     `key_provider.decrypt(ciphertext, nonce, tenant)` → 32-byte seed.
///   - wire `DiscreteCommandSink<FlightCommand>::send_command` into the
///     agent-loop tool-dispatch layer (also Phase 27 SC5).
///
/// Phase 25 worker behaviour: always construct with `seed: None` and emit the
/// D-12 warning — signing is force-disabled at the library layer per
/// `MavlinkSigningConfig { seed: None, .. }` semantics (same post-condition
/// as a pre-migration host returning `None` from `get_mavlink_signing_key`).
/// The compliance fixtures in plan 25-14 exercise signing end-to-end against a
/// stand-alone backend with a real seed, not through the worker path.
async fn construct_mavlink_backend(
    mavlink_cfg: &roz_worker::config::MavlinkConfig,
    host_id: Uuid,
) -> Option<Arc<roz_mavlink::MavlinkBackend>> {
    let Some(transport_url) = mavlink_cfg.transport.as_ref() else {
        return None;
    };

    // Phase 25 scope: signing seed left NULL (matches pre-migration hosts per
    // D-12). See module doc — Phase 27 wires the `get_mavlink_signing_key` +
    // `key_provider.decrypt` path end-to-end with a real pool + KeyProvider.
    tracing::warn!(
        %host_id,
        "MAVLink signing key columns are NULL on roz_hosts row — signing force-disabled (D-12). \
         Re-provision via host rotation to enable."
    );
    let seed: Option<[u8; 32]> = None;

    let posture = match mavlink_cfg.signing.posture.as_str() {
        "off" => roz_mavlink::SigningPosture::Off,
        "on" => roz_mavlink::SigningPosture::On,
        _ => roz_mavlink::SigningPosture::Auto,
    };
    let signing_config = roz_mavlink::MavlinkSigningConfig {
        seed,
        posture,
        allow_unsigned: mavlink_cfg.signing.allow_unsigned,
        local_link_id: mavlink_cfg.signing.local_link_id,
    };

    let autopilot_hint = match mavlink_cfg.autopilot_hint.as_deref() {
        Some("px4") => roz_mavlink::AutopilotHint::Px4,
        Some("arducopter") => roz_mavlink::AutopilotHint::ArduCopter,
        Some("arduplane") => roz_mavlink::AutopilotHint::ArduPlane,
        _ => roz_mavlink::AutopilotHint::Unknown,
    };

    // Sysid: last byte of host_id UUID, clamped >= 2 to avoid the FCU default
    // (sysid=1). 1/254 collision probability across multiple workers is
    // accepted in Phase 25 per T-25-13-04.
    let our_system_id: u8 = {
        let bytes = host_id.as_bytes();
        let candidate = bytes[15];
        if candidate < 2 { 2 } else { candidate }
    };

    let backend_result = if let Some(stripped) = transport_url.strip_prefix("serial:") {
        // "serial:{path}:{baud}" — split from the right so path-with-colons works.
        match stripped.rsplit_once(':') {
            Some((path, baud_str)) => match baud_str.parse::<u32>() {
                Ok(baud) => {
                    roz_mavlink::MavlinkBackend::new_serial(path, baud, signing_config, our_system_id, autopilot_hint)
                        .await
                }
                Err(e) => Err(anyhow::anyhow!("invalid serial baud '{}': {}", baud_str, e)),
            },
            None => Err(anyhow::anyhow!(
                "invalid serial URL '{}' — expected 'serial:<path>:<baud>'",
                transport_url
            )),
        }
    } else if let Some(bind) = transport_url.strip_prefix("udpin:") {
        roz_mavlink::MavlinkBackend::new_udp_in(bind, signing_config, our_system_id, autopilot_hint).await
    } else {
        Err(anyhow::anyhow!(
            "unsupported MAVLink transport URL '{}' — only 'serial:' and 'udpin:' are supported in Phase 25",
            transport_url
        ))
    };

    match backend_result {
        Ok(backend) => {
            tracing::info!(%transport_url, "MAVLink backend constructed");
            Some(Arc::new(backend))
        }
        Err(e) => {
            tracing::error!(error = %e, %transport_url, "failed to construct MAVLink backend — continuing without it");
            None
        }
    }
}

fn decode_team_event_payload(payload: &[u8]) -> Option<TeamEvent> {
    if let Ok(sequenced) = serde_json::from_slice::<SequencedTeamEvent>(payload) {
        return Some(sequenced.event);
    }

    serde_json::from_slice::<TeamEvent>(payload).ok()
}

async fn publish_task_status(
    nats: &async_nats::Client,
    signing_ctx: Option<&WorkerSigningContext>,
    event: &TaskStatusEvent,
) {
    // Phase 23 FS-04: when signing is enabled, route through the signed
    // helper so every task-status NATS message carries a roz-sig-v1 header
    // (plan 23-08 Task 2). When disabled (D-12 rollout window or local dev
    // without ROZ_ENCRYPTION_KEY), fall back to the unsigned path.
    if let Some(ctx) = signing_ctx {
        if let Err(error) = roz_worker::dispatch::publish_task_status_signed(nats, ctx, event).await {
            tracing::warn!(
                %error,
                task_id = %event.task_id,
                status = %event.status,
                "failed to publish signed task status"
            );
        }
        return;
    }

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
                // Phase 26.3 D-06: extract W3C trace context on the first line so
                // the rest of this closure runs under the server's trace. JetStream
                // `Message` derefs to `async_nats::Message`, so `msg.headers` is
                // `Option<HeaderMap>`.
                if let Some(ref headers) = msg.headers {
                    roz_nats::trace::extract_and_link_parent(headers);
                }
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
    signing_ctx: Option<WorkerSigningContext>,
    // Plan 24-12 Task 1: module-level policy state threaded in so the
    // pre-dispatch gate evaluates against the live `PolicyCache`/`HotPolicy`
    // updated by the policy push subscriber — not a fresh permissive
    // default. Arc clones are cheap (atomic bump).
    policy_cache: std::sync::Arc<roz_worker::policy_cache::PolicyCache>,
    hot_policy: std::sync::Arc<roz_worker::policy_cache::HotPolicy>,
    // Plan 24-12 Task 3: the copper chassis-level hot policy + shared
    // backpressure atom that `CopperHandle::spawn_with_policy` plugs into
    // the running task graph (Plan 24-10 API). Reads are lock-free on the
    // 100 Hz tick.
    copper_hot_policy: roz_copper::policy::HotCopperPolicy,
    telemetry_backpressure: std::sync::Arc<roz_worker::telemetry_backpressure::TelemetryBackpressure>,
    // Plan 24-12 Task 5: shared `WalStore` so `execute_task` can spawn a
    // per-task `CheckpointWriter` with a real `periodic_task_id` while a
    // task is active. `None` when signing bootstrap did not complete
    // (D-12 rollout / no `ROZ_ENCRYPTION_KEY`).
    worker_wal_shared: Option<std::sync::Arc<roz_worker::wal::WalStore>>,
    // Plan 24-13 Task 3: broadcast sender for session-scoped events. The
    // pre-dispatch gate emits `SessionEvent::SafetyViolation` on
    // Reject/Halt/Clamp outcomes via an mpsc→broadcast forwarder so the
    // existing mpsc-based `emit_violation_event` signature is preserved.
    // The resume subscriber (Plan 24-12 Task 4) publishes directly on this
    // same broadcast; both paths terminate on one fan-out stream.
    session_event_tx: tokio::sync::broadcast::Sender<roz_core::session::event::EventEnvelope>,
    // Phase 26-12 OBS-01: shared pointer to the currently-active copper
    // `ControllerState`. Set by this function when it spawns a
    // `CopperHandle`; cleared on task shutdown. The worker-wide 10 Hz
    // telemetry loop reads this to populate `TelemetryUpdate.end_effector_pose`.
    shared_copper_state: std::sync::Arc<
        arc_swap::ArcSwapOption<arc_swap::ArcSwap<roz_copper::channels::ControllerState>>,
    >,
) {
    tracing::info!("starting task execution");

    if invocation.parent_task_id.is_some() && invocation.delegation_scope.is_none() {
        tracing::error!(task_id = %task_id, "child worker invocation missing delegation scope");
        publish_task_status(
            &task_nats,
            signing_ctx.as_ref(),
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
            signing_ctx.as_ref(),
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

    // Plan 24-12 Task 5: spawn a short-lived per-task `CheckpointWriter`
    // with a real `periodic_task_id = task_id.to_string()` so the 5 s
    // periodic WAL checkpoints run for the duration of the task. The
    // boot-time writer at main.rs has `periodic_task_id=""` which disables
    // periodic writes; we bind a real task id here. The sender
    // (`task_ckpt_tx`) is held live for the task lifetime; dropping it +
    // cancelling `task_ckpt_cancel` at task exit drains the receiver
    // cleanly. Future plans wire the sender into the agent loop so
    // `ToolCallStarted` / `ToolCallCompleted` / `ApprovalReceived`
    // triggers fire.
    let task_ckpt_cancel = CancellationToken::new();
    let (task_ckpt_tx, task_ckpt_rx) = roz_worker::checkpoint_writer::checkpoint_writer_channel(
        roz_worker::checkpoint_writer::DEFAULT_CHANNEL_CAPACITY,
    );
    if let Some(wal) = worker_wal_shared.as_ref() {
        let wal_clone = wal.clone();
        let cancel_clone = task_ckpt_cancel.clone();
        let task_id_str = task_id.to_string();
        tokio::spawn(async move {
            let writer = roz_worker::checkpoint_writer::CheckpointWriter::new(
                wal_clone,
                task_id_str,
                0,
                roz_worker::checkpoint_writer::DEFAULT_CHECKPOINT_INTERVAL,
                cancel_clone,
            );
            writer.run(task_ckpt_rx).await;
        });
    } else {
        // Drain the receiver so the channel does not back up when no WAL
        // is available. Without this the `task_ckpt_tx` would fill the
        // bounded channel and subsequent `try_send` from future agent-loop
        // wiring would drop triggers silently.
        tokio::spawn(async move {
            let mut rx = task_ckpt_rx;
            while rx.recv().await.is_some() {
                // Drop; no WAL available to checkpoint against.
            }
        });
    }
    // Plan 24-13 Task 3: clone the per-task sender into a
    // `ChannelCheckpointSignal` and hand it to the AgentLoop below so the
    // agent loop emits `CheckpointTrigger::ToolCallStarted` /
    // `ToolCallCompleted` / `ApprovalReceived` at the three D-08 locked
    // transitions. The original binding is held live for the task lifetime
    // so the receiver never observes a disconnected channel.
    let task_ckpt_tx_for_agent = task_ckpt_tx.clone();
    // Hold the per-task sender live for the duration of the task. Dropping
    // it + cancelling `task_ckpt_cancel` at task exit drains the receiver
    // cleanly.
    let _task_ckpt_sender_hold = task_ckpt_tx;

    // Plan 24-13 Task 3 (refactored to testable helper in 24-14 Task 0):
    // mpsc→broadcast forwarder for SessionEvent SafetyViolation emission
    // from the pre-dispatch gate. The existing `emit_violation_event` helper
    // (dispatch.rs:506) takes `mpsc::Sender<SessionEvent>` — bridging here
    // preserves that signature (and its existing tests) while routing into
    // the broadcast fan-out the resume subscriber (Plan 24-12 Task 4) also
    // publishes on.
    //
    // EventEnvelope shape mirrors the existing 24-12 RecoveryPending emit
    // path (main.rs around line 1741) so both paths produce identical
    // envelope metadata and operators see a unified stream.
    let (session_mpsc_tx, session_mpsc_rx) = tokio::sync::mpsc::channel::<roz_core::session::event::SessionEvent>(64);
    roz_worker::session_event_forwarder::spawn_session_event_forwarder(session_mpsc_rx, session_event_tx.clone());

    let model = match roz_worker::model_factory::build_model(&task_config, None) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "failed to build model for task, aborting");
            publish_task_status(
                &task_nats,
                signing_ctx.as_ref(),
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
    dispatcher.register_with_category(
        Box::new(roz_agent::tools::execute_code::ExecuteCodeTool),
        roz_core::tools::ToolCategory::CodeSandbox,
    );
    let guards: Vec<Box<dyn roz_agent::safety::SafetyGuard>> = vec![Box::new(
        roz_agent::safety::guards::VelocityLimiter::new(task_config.max_velocity.unwrap_or(1.5)),
    )];
    let safety = roz_agent::safety::SafetyStack::new(guards);

    // Spawn Copper controller for OodaReAct mode.
    //
    // Worker task invocations currently carry the canonical control contract
    // but not a compiled EmbodimentRuntime or runtime-owned rollout policy, so
    // the worker must stay on the execution boundary only.
    //
    // Plan 24-12 Task 3: `spawn_with_policy` replaces `spawn_execution_only`
    // so the chassis-level `HotCopperPolicy` (updated by the policy push
    // subscriber) and the shared `TelemetryBackpressure` atom (written by
    // the WAL-aware telemetry publisher) reach the running task graph.
    let mut copper_handle = match invocation.mode {
        roz_nats::dispatch::ExecutionMode::OodaReAct => {
            let max_velocity = task_config.max_velocity.unwrap_or(1.5);
            let handle = roz_worker::copper_handle::CopperHandle::spawn_with_policy(
                max_velocity,
                copper_hot_policy.clone(),
                telemetry_backpressure.shared(),
            );
            tracing::info!("copper controller spawned for OodaReAct task with hot policy + shared backpressure");
            // Phase 26-12 OBS-01: install a clone of the controller state
            // pointer so the worker-wide 10 Hz telemetry loop (main()) can
            // observe `ControllerState.entities` and publish
            // `TelemetryUpdate.end_effector_pose`. The pointer is cleared
            // at task shutdown (e-stop drop + final shutdown) so telemetry
            // does not continue to read stale state from a prior task.
            shared_copper_state.store(Some(std::sync::Arc::clone(handle.state())));
            Some(handle)
        }
        roz_nats::dispatch::ExecutionMode::React => None,
    };

    // Phase 26.8 D-08 lift: MavlinkBackend is now constructed once at
    // worker-boot scope (see main() below, search for `let mavlink_backend:`)
    // and threaded into `spawn_session_relay` so `handle_edge_session` can
    // drive the ulog finalize hook. The former per-task `_mavlink_backend`
    // binding here was unused (Phase 27 D-19 integration deferred to a
    // later plan), so removal is safe.

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

    // DEBT-03 / MEM-04: build turn-flush BEFORE the SessionRuntime so the same
    // PgPool can back both the write-behind turn emitter AND the
    // PostgresMemoryStore that feeds the frozen memory snapshot (MEM-05).
    // `mut` because MEM-03 takes `fact_rx` by value below.
    let mut turn_flush = roz_worker::turn_flush::build_turn_flush(&task_config).await;

    // Phase 24 FS-01: pre-dispatch policy gate. Runs enforce_invocation against
    // the worker's active policy BEFORE the agent loop starts. On reject/halt,
    // write a safety-violation audit row, signal `SafetyStop` back to Restate,
    // and return early. On stale-cache (D-01) write a `policy_stale` warning
    // audit row and continue.
    //
    // Plan 24-12 Task 1: the gate now uses the module-level `PolicyCache` +
    // `HotPolicy` threaded in from `main()` (updated by the policy push
    // subscriber) — NOT a fresh permissive default. Declared velocity
    // fields come from the invocation payload per Plan 24-12 Task 1
    // (`TaskInvocation.declared_max_{linear,angular}_{m,rad}_per_s`).
    {
        let gate_start = std::time::Instant::now();
        let decision = roz_worker::dispatch::pre_dispatch_check(
            policy_cache.as_ref(),
            hot_policy.as_ref(),
            &invocation,
            invocation.declared_max_linear_m_per_s,
            invocation.declared_max_angular_rad_per_s,
        )
        .await;
        let gate_elapsed = gate_start.elapsed();
        if gate_elapsed > Duration::from_millis(10) {
            tracing::warn!(
                gate_ms = gate_elapsed.as_millis() as u64,
                task_id = %task_id,
                "pre-dispatch policy gate exceeded 10 ms budget"
            );
        }

        // D-01: stale-cache audit + continue.
        if decision.stale {
            if let (Some(pool), Ok(tenant_uuid)) = (turn_flush.pool.as_ref(), invocation.tenant_id.parse::<Uuid>()) {
                roz_worker::dispatch::write_safety_audit(
                    pool,
                    tenant_uuid,
                    Some(invocation.host_id),
                    Some(task_id),
                    Some(decision.policy_id),
                    "policy_stale",
                    "warning",
                    "worker-policy-cache",
                    serde_json::json!({
                        "declared_policy_id": invocation.safety_policy_id,
                        "note": "cache miss on declared policy_id; fell back to HotPolicy per D-01",
                    }),
                )
                .await;
            } else {
                tracing::warn!(
                    task_id = %task_id,
                    policy_id = %decision.policy_id,
                    "policy_stale audit skipped (no pool or unparseable tenant_id)"
                );
            }
        }

        // Violation branches: audit + SafetyStop + return.
        match decision.outcome {
            roz_worker::dispatch::PreDispatchOutcome::Allow => {
                tracing::debug!(task_id = %task_id, policy_id = %decision.policy_id, "pre-dispatch gate: allow");
            }
            roz_worker::dispatch::PreDispatchOutcome::Clamp { clamped_details } => {
                tracing::info!(
                    task_id = %task_id,
                    policy_id = %decision.policy_id,
                    details = %clamped_details,
                    "pre-dispatch gate: clamp (declared params projected to policy limits)"
                );
                if let (Some(pool), Ok(tenant_uuid)) = (turn_flush.pool.as_ref(), invocation.tenant_id.parse::<Uuid>())
                {
                    roz_worker::dispatch::write_safety_audit(
                        pool,
                        tenant_uuid,
                        Some(invocation.host_id),
                        Some(task_id),
                        Some(decision.policy_id),
                        "safety_violation",
                        roz_worker::dispatch::severity_for_action("clamp"),
                        "worker-pre-dispatch",
                        clamped_details.clone(),
                    )
                    .await;
                }
                // Plan 24-13 Task 3: emit SessionEvent::SafetyViolation on the
                // Clamp branch AFTER the audit write. Operators see the
                // violation on the session event stream; auditors see it in
                // the roz_safety_audit_log row. Clamp retains the declared
                // policy_id; violation_kind is "limit_exceeded" because the
                // clamp projection triggers on a velocity/acceleration/etc.
                // limit breach.
                roz_worker::dispatch::emit_violation_event(
                    &session_mpsc_tx,
                    decision.policy_id,
                    "limit_exceeded",
                    "clamp",
                    clamped_details,
                );
            }
            roz_worker::dispatch::PreDispatchOutcome::Reject(ref err)
            | roz_worker::dispatch::PreDispatchOutcome::Halt(ref err) => {
                let violation_kind = roz_worker::dispatch::enforcement_error_kind(err);
                let enforcement_action = match &decision.outcome {
                    roz_worker::dispatch::PreDispatchOutcome::Halt(_) => "halt",
                    _ => "reject",
                };
                let details = serde_json::json!({
                    "violation_kind": violation_kind,
                    "enforcement_action": enforcement_action,
                    "error": err.to_string(),
                });
                if let (Some(pool), Ok(tenant_uuid)) = (turn_flush.pool.as_ref(), invocation.tenant_id.parse::<Uuid>())
                {
                    roz_worker::dispatch::write_safety_audit(
                        pool,
                        tenant_uuid,
                        Some(invocation.host_id),
                        Some(task_id),
                        Some(decision.policy_id),
                        "safety_violation",
                        roz_worker::dispatch::severity_for_action(enforcement_action),
                        "worker-pre-dispatch",
                        details.clone(),
                    )
                    .await;
                }
                // Plan 24-13 Task 3: emit SessionEvent::SafetyViolation on the
                // Reject / Halt branches AFTER the audit write and BEFORE the
                // early return. Closes VERIFICATION.md gap 3 (FS-01 SC#1 —
                // violations emit SafetyViolation session event).
                roz_worker::dispatch::emit_violation_event(
                    &session_mpsc_tx,
                    decision.policy_id,
                    violation_kind,
                    enforcement_action,
                    details.clone(),
                );
                tracing::warn!(
                    task_id = %task_id,
                    policy_id = %decision.policy_id,
                    violation_kind,
                    enforcement_action,
                    error = %err,
                    "pre-dispatch gate rejected invocation"
                );
                publish_task_status(
                    &task_nats,
                    signing_ctx.as_ref(),
                    &TaskStatusEvent {
                        task_id,
                        status: "failed".into(),
                        detail: Some(format!("policy violation ({violation_kind}): {err}")),
                        host_id: Some(invocation.host_id),
                    },
                )
                .await;
                let result = roz_worker::dispatch::build_task_result(
                    task_id,
                    TaskTerminalStatus::SafetyStop,
                    Err(roz_agent::error::AgentError::Safety(err.to_string())),
                );
                if let Err(sig_err) =
                    roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await
                {
                    tracing::error!(error = %sig_err, "failed to signal policy-violation result to Restate");
                }
                return;
            }
        }
    }

    // MEM-03: spawn the fact extractor when pool + aux-LLM + fact_rx are all
    // available. Fall-open on any missing dep — the emitter's try_send on the
    // fact channel logs & drops when the receiver is gone.
    if let (Some(pool), Some(fact_rx), Some(aux)) = (
        turn_flush.pool.clone(),
        turn_flush.fact_rx.take(),
        roz_agent::aux_llm::GeminiFlashAuxLlm::from_env(),
    ) {
        let fact_cancel = task_sidecars_cancel.clone();
        let aux_arc: std::sync::Arc<dyn roz_agent::aux_llm::AuxLlm> = std::sync::Arc::new(aux);
        let cfg = roz_agent::agent_loop::fact_extractor::FactExtractorConfig {
            observed_peer_id: invocation.tenant_id.clone(),
            ..Default::default()
        };
        tokio::spawn(async move {
            roz_agent::agent_loop::fact_extractor::run_fact_extractor_task(fact_rx, pool, aux_arc, cfg, fact_cancel)
                .await;
        });
    } else if turn_flush.pool.is_some() {
        tracing::info!("MEM-03: fact extraction disabled (ROZ_GEMINI_API_KEY not set)");
    }

    // MEM-04 / MEM-05: wire the Postgres-backed MemoryStore when a pool is
    // available; fall back to the in-memory store on fail-closed paths
    // (local/dev without ROZ_DATABASE_URL). The snapshot is frozen at
    // construction time inside `SessionRuntime::new_with_memory_store`.
    let memory_store: std::sync::Arc<dyn roz_agent::memory_store::MemoryStore> = match &turn_flush.pool {
        Some(pool) => std::sync::Arc::new(roz_agent::memory_store::PostgresMemoryStore::new(pool.clone())),
        None => std::sync::Arc::new(roz_agent::memory_store::InMemoryMemoryStore::new()),
    };

    // Phase 18 SKILL-05 / PLAN-08: load the frozen tier-0 skill snapshot once
    // per session under tenant RLS (mirrors cloud handle_start in
    // crates/roz-server/src/grpc/agent.rs). Fail-open: empty Vec when the
    // worker has no DB pool, when set_tenant_context fails, or when the
    // tenant_id from the invocation isn't a UUID.
    let frozen_skills: Vec<roz_db::skills::SkillSummary> =
        if let (Some(pool), Ok(tenant_uuid)) = (turn_flush.pool.as_ref(), invocation.tenant_id.parse::<Uuid>()) {
            match pool.begin().await {
                Ok(mut db_tx) => {
                    if let Err(err) = roz_db::set_tenant_context(&mut *db_tx, &tenant_uuid).await {
                        tracing::warn!(
                            error = %err,
                            tenant_id = %tenant_uuid,
                            "skills snapshot set_tenant_context failed; continuing with empty snapshot"
                        );
                        Vec::new()
                    } else {
                        match roz_db::skills::list_recent(&mut *db_tx, 20).await {
                            Ok(rows) => {
                                let _ = db_tx.commit().await;
                                rows
                            }
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    tenant_id = %tenant_uuid,
                                    "skills snapshot list_recent failed; continuing with empty snapshot"
                                );
                                Vec::new()
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        tenant_id = %tenant_uuid,
                        "skills snapshot tx begin failed; continuing with empty snapshot"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

    let mut session_runtime = SessionRuntime::new_with_memory_store(&session_config, memory_store).await;
    // Phase 18 SKILL-05 / PLAN-08: install the frozen tier-0 skill snapshot
    // (loaded above) so every turn's AssemblyContext.skill_entries reads from
    // the same stable Vec. Mid-session writes do NOT mutate this prompt
    // snapshot — the agent uses `skills_list` for live discovery and
    // `skill_view` for live body/version loads until the next session.
    session_runtime.set_skill_snapshot(frozen_skills);
    let approval_runtime = session_runtime.approval_handle();
    extensions.insert(session_runtime.event_emitter());

    if let Some(parent_task_id) = invocation.parent_task_id {
        // Child task: consume inbound resolutions from parent, relay outbound requests up.
        tokio::spawn(consume_team_approval_events(
            task_js.clone(),
            parent_task_id,
            task_id,
            approval_runtime.clone(),
            task_sidecars_cancel.clone(),
        ));
        tokio::spawn(relay_worker_approval_events(
            session_runtime.subscribe_events(),
            task_js.clone(),
            parent_task_id,
            task_id,
            task_sidecars_cancel.clone(),
        ));
    } else {
        // Orchestrator task: relay outbound requests and consume own resolutions.
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

    // MEM-07 + PLAN-17-07 Task 3 + Phase 18 PLAN-08: register the four memory
    // tools (Phase 17) and four skill tools (Phase 18). Workers default to
    // `can_write_memory: false` AND `can_write_skills: false` per RESEARCH
    // OQ #3 — workers are NOT the canonical skill-write entry point; the CLI
    // (PLAN-09) and cloud server are.
    dispatcher.register_phase17_memory_tools();
    dispatcher.register_phase18_skill_tools();

    // Workers run on behalf of a single authenticated tenant; permission is
    // derived from the invocation's trust posture. Default deny until explicit
    // upgrade. `can_write_skills` is forced false here regardless of any future
    // can_write_memory upgrade so an accidental delegation_scope flip cannot
    // grant skill-write rights to workers (T-18-08-02 mitigation).
    // TODO(phase 18+): compute `can_write_memory` from delegation_scope /
    // trust_posture once owner-trust is modeled at the worker level.
    extensions.insert(roz_core::auth::Permissions {
        can_write_skills: false,
        ..roz_core::auth::Permissions::default()
    });
    match invocation.tenant_id.parse::<Uuid>() {
        Ok(tenant_uuid) => {
            extensions.insert(roz_core::auth::AuthIdentity::Worker {
                worker_id: task_config.worker_id.clone(),
                tenant_id: roz_core::auth::TenantId::new(tenant_uuid),
                host_id: invocation.host_id.to_string(),
            });
        }
        Err(error) => {
            tracing::warn!(
                tenant_id = %invocation.tenant_id,
                %error,
                "worker auth identity unavailable: tenant_id is not a valid UUID"
            );
        }
    }

    // MEM-07 + PLAN-17-07 Task 3: inject PgPool for the four Pure tools.
    // Worker may run without a DB (local/dev mode). When unset, the tools
    // return a typed "PgPool extension missing" error to the model — no panic.
    if let Some(ref pool) = turn_flush.pool {
        extensions.insert(pool.clone());
    }

    // Phase 18 SKILL-01 / PLAN-08: inject Arc<dyn ObjectStore> when the
    // operator has configured ROZ_SKILL_STORE_ROOT. When unset, the
    // skill_read_file tool returns "ObjectStore extension missing" — workers
    // typically rely on the cloud server as the canonical skill-bundle origin.
    if let Some(skill_root) = task_config.resolved_skill_store_root() {
        match std::fs::create_dir_all(&skill_root) {
            Ok(()) => match object_store::local::LocalFileSystem::new_with_prefix(&skill_root) {
                Ok(fs) => {
                    let store: std::sync::Arc<dyn object_store::ObjectStore> = std::sync::Arc::new(fs);
                    extensions.insert(store);
                    tracing::info!(
                        skill_store_root = %skill_root.display(),
                        "worker skill object store registered"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        skill_store_root = %skill_root.display(),
                        "failed to construct LocalFileSystem object store; skill_read_file tool will return errors"
                    );
                }
            },
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    skill_store_root = %skill_root.display(),
                    "failed to create ROZ_SKILL_STORE_ROOT; skill_read_file tool will return errors"
                );
            }
        }
    }

    // Plan 24-13 Task 3: wire a `ChannelCheckpointSignal` (wrapping the per-
    // task `task_ckpt_tx` cloned at the top of `execute_task`) into the
    // AgentLoop via the additive `with_checkpoint_signal` builder (Plan
    // 24-13 Task 2). Closes VERIFICATION.md gap 8-remaining (FS-03 SC#1 —
    // checkpoint writer event-driven triggers have production emitters).
    let checkpoint_signal: std::sync::Arc<dyn roz_core::checkpoint_signal::CheckpointSignal> = std::sync::Arc::new(
        roz_worker::checkpoint_writer::ChannelCheckpointSignal::new(task_ckpt_tx_for_agent),
    );
    let agent = AgentLoop::new(model, dispatcher, safety, spatial)
        .with_extensions(extensions)
        .with_approval_runtime(approval_runtime)
        .with_turn_emitter_opt(turn_flush.emitter.clone())
        .with_checkpoint_signal(checkpoint_signal);
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
        signing_ctx.as_ref(),
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
                // Phase 26-12 OBS-01: clear the shared pose pointer so the
                // worker-wide telemetry loop stops reading the (now-dead)
                // controller state.
                shared_copper_state.store(None);
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
        signing_ctx.as_ref(),
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
        // Phase 26-12 OBS-01: clear the shared pose pointer so the
        // worker-wide telemetry loop stops reading the (now-dead)
        // controller state once this task ends.
        shared_copper_state.store(None);
    }

    // DEBT-03: drain the write-behind flush task before returning so any
    // queued turns are persisted and the task does not leak.
    turn_flush.drain().await;
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

    // Telemetry publisher spawned LATER, after signing bootstrap, so it can
    // route through the signed publish path (Phase 23 FS-04). See
    // "Phase 23: spawn telemetry publisher after signing bootstrap" below.

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

    // =================================================================
    // Phase 24 shared state — hoisted above the watchdog construction so
    // the policy-sourced `on_expire` callback (Plan 24-12 Task 2) can
    // capture the module-level `HotPolicy` Arc before the watchdog spawns.
    // The Arc wrappers also allow Plan 24-12 Task 1 to thread these into
    // `execute_task` for the pre-dispatch gate instead of constructing a
    // fresh permissive default per invocation.
    // =================================================================
    let phase24_cancel = CancellationToken::new();
    let policy_cache = std::sync::Arc::new(roz_worker::policy_cache::PolicyCache::new());
    let hot_policy = std::sync::Arc::new(roz_worker::policy_cache::HotPolicy::permissive());
    let copper_hot_policy = roz_copper::policy::new_hot_policy();
    // Telemetry backpressure is wrapped in an `Arc` so the worker's
    // telemetry publisher (writes via `TelemetryBackpressure::update`) and
    // `CopperHandle::spawn_with_policy` (reads the same atom via
    // `shared()`) see one pointee. Plan 24-12 Task 3 threads this into
    // `execute_task`.
    let telemetry_backpressure = std::sync::Arc::new(roz_worker::telemetry_backpressure::TelemetryBackpressure::new());
    let telemetry_drop_counter = std::sync::Arc::new(roz_worker::telemetry::DropCounter::new());
    let telemetry_append_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Phase 26-12 OBS-01: shared pointer to the currently-active copper
    // `ControllerState`. `None` when no OodaReAct task is running (or when
    // the running task did not spawn a CopperHandle). `execute_task` stores
    // a clone of `handle.state()` here when it spawns a CopperHandle, and
    // clears it on task completion. The worker-wide 10 Hz telemetry loop at
    // the bottom of `main()` reads from this pointer to populate
    // `TelemetryUpdate.end_effector_pose` from the first entity with both a
    // position and an orientation.
    let shared_copper_state: std::sync::Arc<
        arc_swap::ArcSwapOption<arc_swap::ArcSwap<roz_copper::channels::ControllerState>>,
    > = std::sync::Arc::new(arc_swap::ArcSwapOption::empty());

    // Spawn idle watchdog — fires if no NATS message arrives within 30s.
    // Plan 24-12 Task 2: the `on_expire` callback reads the live
    // `HotPolicy.deadman_timers.on_expire` action and logs it. Phase 25
    // replaces the log body with an actual MAVLink command dispatch.
    let deadman_callback = roz_worker::command_watchdog::build_deadman_callback(hot_policy.clone());
    let watchdog = Arc::new(roz_worker::command_watchdog::CommandWatchdog::with_on_expire(
        Duration::from_secs(30),
        deadman_callback,
    ));
    let watchdog_cancel = CancellationToken::new();
    let wd = watchdog.clone();
    let wd_cancel = watchdog_cancel.clone();
    tokio::spawn(async move { wd.run(wd_cancel).await });
    tracing::info!("idle watchdog active (30s deadline, policy-sourced on_expire)");

    // Load embodiment runtime from robot.toml if configured (D-01, D-02)
    let embodiment_runtime: Option<roz_core::embodiment::embodiment_runtime::EmbodimentRuntime> =
        config.robot_toml.as_ref().and_then(|toml_path| {
            match roz_core::manifest::EmbodimentManifest::load(std::path::Path::new(toml_path)) {
                Ok(manifest) => {
                    if let Some(rt) = manifest.embodiment_runtime() {
                        tracing::info!(path = %toml_path, digest = %rt.model.model_digest, "loaded embodiment runtime from manifest");
                        Some(rt)
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

    // Phase 23 Plan 23-08: signing context, hoisted here so the invoke
    // subscribe loop below can verify inbound server→worker envelopes.
    // Populated by the provision/rotate/load path; stays `None` under D-12
    // rollout when `ROZ_ENCRYPTION_KEY` is unset.
    let mut signing_ctx: Option<WorkerSigningContext> = None;
    // Hold onto the key provider + data dir for in-loop `force_rotate` on
    // server-key rotation (D-15 bounded refetch).
    let mut signing_rotate_ctx: Option<(
        std::sync::Arc<roz_core::key_provider::StaticKeyProvider>,
        std::path::PathBuf,
    )> = None;
    // Phase 24 FS-02 / FS-03: the shared WalStore, tenant id, and signing-ctx
    // Arc wrapper are hoisted here so the Phase 24 subsystems spawned below
    // (checkpoint_writer, telemetry_replay, reconnect publisher) can share
    // the same pointee as the signing context. All three stay `None` when
    // signing bootstrap is skipped (D-12 rollout / no ROZ_ENCRYPTION_KEY);
    // the subsystems guard on `Some` before spawning.
    let mut worker_wal: Option<std::sync::Arc<roz_worker::wal::WalStore>> = None;
    let mut worker_tenant: Option<Uuid> = None;
    let mut signing_ctx_shared: Option<std::sync::Arc<WorkerSigningContext>> = None;
    // Phase 26.8 D-08 lift: hoist host_id up here so the boot-scope
    // `construct_mavlink_backend` call below (before `spawn_session_relay`)
    // has the registered host UUID. Populated by the Ok arm of `register_host`
    // below; stays `None` when registration is skipped (empty API key) or
    // fails — matching `worker_tenant` hoisting semantics.
    let mut worker_host_id: Option<Uuid> = None;

    // Register with server and bootstrap Phase 23 device signing key.
    //
    // `register_host` now returns `HostIdentity { host_id, tenant_id }` —
    // both are needed by `bootstrap_device_key` because `tenant_id` is a
    // signed field in every `roz-sig-v1` envelope.
    //
    // D-09: device-key bootstrap is a hard-stop gate. If the server is
    // reachable but returns an error for provision/rotate, or if the
    // on-disk key is present but corrupt/undecryptable, the worker exits
    // with EX_CONFIG (78) so systemd and ops dashboards page immediately.
    // Host registration failure (the outer `register_host` Err arm) stays
    // on the existing log-and-continue path so workers without API access
    // can still come up in local/test modes.
    if !config.api_key.is_empty() {
        match roz_worker::registration::register_host(&http, &config.api_url, &config.api_key, &config.worker_id).await
        {
            Ok(identity) => {
                tracing::info!(
                    host_id = %identity.host_id,
                    tenant_id = %identity.tenant_id,
                    "registered with server"
                );
                // Phase 26.8 D-08 lift: capture the resolved host UUID so the
                // boot-scope `construct_mavlink_backend` call below (before
                // `spawn_session_relay`) has the sysid-derivation input.
                worker_host_id = Some(identity.host_id);
                // Upload embodiment runtime if available (D-04: log-and-continue; runtime now passed per STRM-02)
                if let Some(ref rt) = embodiment_runtime {
                    match roz_worker::registration::upload_embodiment(
                        &http,
                        &config.api_url,
                        &config.api_key,
                        identity.host_id,
                        &rt.model,
                        Some(rt),
                    )
                    .await
                    {
                        Ok(()) => tracing::info!(host_id = %identity.host_id, "embodiment runtime uploaded"),
                        Err(e) => {
                            tracing::warn!(host_id = %identity.host_id, error = %e, "embodiment upload failed");
                        }
                    }
                }

                // Phase 23: provision / load / rotate the device signing key.
                // Only attempt when ROZ_ENCRYPTION_KEY is configured —
                // without it, the worker cannot encrypt the seed at rest.
                // Missing key in non-signed-dispatch environments is a
                // log-and-continue (the worker still runs; 23-08 will gate
                // signed publishes on `_signing_material` being Some).
                match roz_core::key_provider::StaticKeyProvider::from_env() {
                    Ok(provider) => {
                        let key_provider = std::sync::Arc::new(provider);
                        let dir = roz_worker::signing_key::data_dir();
                        match roz_worker::registration::bootstrap_device_key(
                            &http,
                            &config.api_url,
                            &config.api_key,
                            &key_provider,
                            identity,
                            &dir,
                        )
                        .await
                        {
                            Ok(material) => {
                                tracing::info!(
                                    host_id = %identity.host_id,
                                    key_version = material.key_version,
                                    "device signing key ready"
                                );
                                // Plan 23-08 Task 3: construct the
                                // WorkerSigningContext so the invoke subscribe
                                // loop below can verify inbound server→worker
                                // envelopes. The signing WAL path is
                                // worker-specific so that `next_seq`'s SQLite
                                // counter survives restarts (per
                                // wal::tests::next_seq_survives_reopen).
                                let wal_path = dir.join(format!("wal-{}.db", config.worker_id));
                                match roz_worker::wal::WalStore::open(wal_path.to_str().unwrap_or(":memory:")) {
                                    Ok(wal) => {
                                        // Phase 24: hoist the Arc<WalStore> + tenant so the
                                        // subsystems spawned after registration can share the
                                        // same WAL pointee and tenant context.
                                        let wal_arc = std::sync::Arc::new(wal);
                                        worker_wal = Some(wal_arc.clone());
                                        worker_tenant = Some(identity.tenant_id);
                                        let ctx = WorkerSigningContext::new(
                                            std::sync::Arc::new(RwLock::new(material)),
                                            wal_arc,
                                        );
                                        signing_ctx_shared = Some(std::sync::Arc::new(ctx.clone()));
                                        signing_ctx = Some(ctx);
                                        signing_rotate_ctx = Some((key_provider.clone(), dir.clone()));
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            error = %e,
                                            host_id = %identity.host_id,
                                            wal_path = %wal_path.display(),
                                            "failed to open WAL for signing; hard-stop (exit 78 EX_CONFIG, D-09)"
                                        );
                                        std::process::exit(78);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = ?e,
                                    host_id = %identity.host_id,
                                    "device signing key bootstrap failed; hard-stop (exit 78 EX_CONFIG, D-09)"
                                );
                                std::process::exit(78);
                            }
                        }
                    }
                    Err(e) => {
                        // D-12 rollout window: a KEK-less worker cannot encrypt
                        // its device key at rest, so we skip Phase 23
                        // enrollment. Any server running with
                        // SIGNED_DISPATCH_ENFORCEMENT=strict will reject this
                        // worker's publishes in-flight. Surface this loudly so
                        // ops dashboards page before messages start dropping.
                        tracing::warn!(
                            error = %e,
                            host_id = %identity.host_id,
                            signed_dispatch = "disabled",
                            "ROZ_ENCRYPTION_KEY not configured; signed dispatch DISABLED for this worker — \
                             production enforcement (SIGNED_DISPATCH_ENFORCEMENT=strict) will reject publishes"
                        );
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "host registration failed"),
        }
    }

    // Phase 23 FS-04 / Plan 24-12 Task 3 / Phase 26-12 OBS-01: spawn
    // telemetry publisher (10 Hz).
    //
    // **Wire format (Phase 26-12 OBS-01):** payload is a prost-encoded
    // `roz.v1.TelemetryUpdate` (not serde_json). Same NATS subject
    // (`telemetry.{worker_id}.state`); the server's MCAP ingest
    // (ingest_cloud.rs Plan 26-05) and gRPC relay (agent.rs Plan 26-12 Task 2)
    // both decode via `prost::Message::decode`. Legacy JSON frames from
    // pre-migration builds are silently dropped on the server side
    // (debug-log-and-continue, no panic).
    //
    // When signing AND a WAL are both configured, route through
    // `publish_state_proto_signed_with_buffer` so NATS-outage frames are
    // buffered to the WAL and replayed on reconnect (FS-02). The WAL treats
    // payloads as opaque bytes, so `telemetry_replay.rs` needs no change —
    // it re-signs and re-publishes stored protobuf payloads verbatim. When
    // only signing is configured, fall back to the plain signed publish.
    // When signing is disabled (D-12 rollout) use the unsigned path.
    //
    // `correlation_id` for telemetry is a stable worker-lifetime UUID so
    // the server's verifier can scope replay protection consistently across
    // the continuous telemetry stream.
    //
    // `end_effector_pose` is populated from the shared copper state pointer
    // (`shared_copper_state`). When an OodaReAct task is executing with a
    // `CopperHandle`, that task installs a clone of its `handle.state()`
    // Arc into this pointer so the worker-wide telemetry loop can observe
    // the live `ControllerState.entities[0]`. Between tasks (or for non-
    // OodaReAct invocations) the pointer is `None` and the pose is
    // published as absent — matching the pre-26-12 behavior where joints/
    // sensors/pose were always empty because `main()` had no copper
    // visibility.
    let telem_nats = nats.clone();
    let telem_worker_id = config.worker_id.clone();
    let telem_signing_ctx = signing_ctx.clone();
    let telem_correlation_id = Uuid::new_v4();
    let telem_wal = worker_wal.clone();
    let telem_bp = telemetry_backpressure.clone();
    let telem_drop = telemetry_drop_counter.clone();
    let telem_append = telemetry_append_counter.clone();
    let telem_copper_state = shared_copper_state.clone();
    tokio::spawn(async move {
        use prost::Message as _;
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;

            // Build `roz.v1.TelemetryUpdate` from the live copper state.
            // `ControllerState.entities[0]` is the pose source per the
            // existing spatial-bridge pattern. When no task is running, or
            // the running task has no copper, the pointer is `None` and
            // `end_effector_pose` is `None` — matching the pre-26-12
            // emission (empty pose) for those conditions.
            let end_effector_pose = telem_copper_state.load_full().and_then(|arc| {
                let state = arc.load();
                let entity = state.entities.first()?;
                let pos = entity.position?;
                let quat_wxyz = entity.orientation?;
                Some(roz_worker::roz_v1::Pose {
                    x: pos[0],
                    y: pos[1],
                    z: pos[2],
                    // `roz_core::spatial::EntityState.orientation` is
                    // `[w, x, y, z]`; `roz.v1.Pose` fields are
                    // `(qx, qy, qz, qw)`. Reorder explicitly at the
                    // assignment site.
                    qx: quat_wxyz[1],
                    qy: quat_wxyz[2],
                    qz: quat_wxyz[3],
                    qw: quat_wxyz[0],
                })
            });

            #[allow(clippy::cast_precision_loss)]
            let ts_secs = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;

            let update = roz_worker::roz_v1::TelemetryUpdate {
                host_id: telem_worker_id.clone(),
                timestamp: ts_secs,
                joint_states: Vec::new(),
                end_effector_pose,
                sensor_readings: std::collections::BTreeMap::new(),
            };
            let payload = update.encode_to_vec();

            let publish_result = match (telem_signing_ctx.as_ref(), telem_wal.as_ref()) {
                (Some(ctx), Some(wal)) => {
                    roz_worker::telemetry::publish_state_proto_signed_with_buffer(
                        &telem_nats,
                        ctx,
                        &telem_worker_id,
                        telem_correlation_id,
                        &payload,
                        wal,
                        &telem_bp,
                        &telem_drop,
                        &telem_append,
                    )
                    .await
                }
                (Some(ctx), None) => {
                    // Signing enabled but no WAL — fall back to plain signed publish (no buffering).
                    roz_worker::telemetry::publish_state_proto_signed(
                        &telem_nats,
                        ctx,
                        &telem_worker_id,
                        telem_correlation_id,
                        &payload,
                    )
                    .await
                }
                (None, _) => roz_worker::telemetry::publish_state_proto(&telem_nats, &telem_worker_id, &payload).await,
            };
            if let Err(e) = publish_result {
                tracing::trace!(error = %e, "telemetry publish failed");
            }
        }
    });

    // =================================================================
    // Phase 24 FS-01 / FS-02 / FS-03 subsystem wiring (Plan 24-09 Task 3)
    // =================================================================
    //
    // Every subsystem below is guarded on `signing_ctx_shared.is_some()`
    // because each new subject rides the Phase 23 signed envelope (D-12).
    // When signing is disabled (D-12 rollout / no ROZ_ENCRYPTION_KEY) the
    // subsystems log-and-skip; the worker still runs on the pre-Phase-24
    // code paths.
    //
    // Shared resources instantiated regardless of signing:
    // - Policy caches (30 s TTL moka + ArcSwap hot pointer) — consumers log
    //   a permissive default until the first `roz.policy.{worker_id}` push.
    // - Copper hot policy pointer — ready for a subscribe-driven store().
    //
    // NOTE: `phase24_cancel`, `policy_cache`, `hot_policy`, `copper_hot_policy`,
    // `telemetry_backpressure`, `telemetry_drop_counter`, and
    // `telemetry_append_counter` are constructed **above** the watchdog
    // block (Plan 24-12 Task 2 hoist) so the deadman callback can capture
    // the `HotPolicy` Arc clone. The subscribers below reuse them via
    // `.clone()` on the Arc wrappers.

    // Plan 15-06 (D-11, D-23, D-24) / Plan 24-12 Task 4: worker-level
    // SessionEvent bus so transport-health transitions AND the resume
    // subscriber's `SessionEvent::RecoveryPending` emission (FS-03 SC#5)
    // share one broadcast channel. Hoisted above the Phase 24 block so
    // the resume subscriber in the `if let (Some(ctx_shared), ...)` body
    // can clone the sender into its closure. Capacity 64 matches the
    // EdgeHealthAggregator signal channel.
    let (session_event_tx, _session_event_rx_keepalive): (
        tokio::sync::broadcast::Sender<roz_core::session::event::EventEnvelope>,
        tokio::sync::broadcast::Receiver<roz_core::session::event::EventEnvelope>,
    ) = tokio::sync::broadcast::channel(64);

    if let (Some(ctx_shared), Some(wal_arc), Some(tenant_uuid)) =
        (signing_ctx_shared.as_ref(), worker_wal.as_ref(), worker_tenant)
    {
        // -----------------------------------------------------------------
        // Checkpoint-trigger channel hoisted above the policy subscriber so
        // the push subscriber can emit `CheckpointTrigger::DegradationChange`
        // on every policy swap (Plan 24-12 Task 5). The boot-time
        // `CheckpointWriter` below drains `ckpt_rx`; the policy subscriber
        // clones `ckpt_tx` into its closure. The outer `ckpt_tx` binding is
        // held live for the worker's lifetime so the receiver never sees a
        // disconnected channel when no active producer is sending.
        // -----------------------------------------------------------------
        let (ckpt_tx, ckpt_rx) = roz_worker::checkpoint_writer::checkpoint_writer_channel(
            roz_worker::checkpoint_writer::DEFAULT_CHANNEL_CAPACITY,
        );

        // -----------------------------------------------------------------
        // Policy push subscriber on `roz.policy.{worker_id}` (D-04).
        // -----------------------------------------------------------------
        {
            let subscribe_nats = nats.clone();
            let subscribe_ctx = ctx_shared.clone();
            let subscribe_cache = policy_cache.clone();
            let subscribe_hot = hot_policy.clone();
            let subscribe_copper_hot = copper_hot_policy.clone();
            let subscribe_worker_id = config.worker_id.clone();
            let subscribe_cancel = phase24_cancel.clone();
            let subscribe_ckpt_tx = ckpt_tx.clone();
            tokio::spawn(async move {
                let subject = match roz_nats::Subjects::policy(&subscribe_worker_id) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "invalid worker_id for policy subject; skipping push subscriber");
                        return;
                    }
                };
                let mut sub = match subscribe_nats.subscribe(subject.clone()).await {
                    Ok(sub) => sub,
                    Err(e) => {
                        tracing::warn!(error = %e, subject = %subject, "failed to subscribe to policy push");
                        return;
                    }
                };
                tracing::info!(subject = %subject, "policy push subscriber ready");
                loop {
                    tokio::select! {
                        maybe_msg = futures::StreamExt::next(&mut sub) => {
                            let Some(msg) = maybe_msg else {
                                tracing::warn!("policy push subscription ended");
                                return;
                            };
                            // Phase 26.3 D-06: extract server's trace on first line so
                            // the rest of the closure (signature verify, policy apply)
                            // runs under the server's span.
                            if let Some(ref headers) = msg.headers {
                                roz_nats::trace::extract_and_link_parent(headers);
                            }
                            let header = msg
                                .headers
                                .as_ref()
                                .and_then(|h| h.get(roz_core::signing::HEADER_NAME).map(|v| v.to_string()));
                            if let Err(e) =
                                subscribe_ctx.verify_inbound_worker(header.as_deref(), &msg.payload)
                            {
                                tracing::warn!(error = %e, "policy push signature rejected");
                                continue;
                            }
                            let row: roz_db::safety_policies::SafetyPolicyRow =
                                match serde_json::from_slice(&msg.payload) {
                                    Ok(r) => r,
                                    Err(e) => {
                                        tracing::warn!(error = %e, "policy push parse failed");
                                        continue;
                                    }
                                };
                            // Plan 24-14 Task 2: apply fan-out extracted into
                            // `apply_policy_push` so the e2e test in Task 3
                            // can drive the same code path with a real NATS
                            // container. The signing-verify + row-parse
                            // branches remain inline above because they are
                            // loop-scoped (`subscribe_ctx` + payload error
                            // vocabulary).
                            if let Err(e) = roz_worker::policy_enforcement::apply_policy_push(
                                &row,
                                &subscribe_cache,
                                &subscribe_hot,
                                &subscribe_copper_hot,
                                Some(&subscribe_ckpt_tx),
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "policy v1 validation failed");
                                continue;
                            }
                            tracing::info!(
                                policy_id = %row.id,
                                version = row.version,
                                "policy push applied (cache + hot + copper hot + degradation trigger)"
                            );
                        }
                        () = subscribe_cancel.cancelled() => return,
                    }
                }
            });
        }

        // -----------------------------------------------------------------
        // Clear-failsafe subscriber on `cmd.{worker_id}.clear_failsafe` (D-02).
        // -----------------------------------------------------------------
        {
            let cf_nats = nats.clone();
            let cf_worker_id = config.worker_id.clone();
            let cf_ctx = ctx_shared.clone();
            let cf_watchdog = watchdog.clone();
            let cf_cancel = phase24_cancel.clone();
            tokio::spawn(async move {
                if let Err(e) = roz_worker::clear_failsafe::run_clear_failsafe_subscriber(
                    cf_nats,
                    cf_worker_id,
                    cf_ctx,
                    cf_watchdog,
                    cf_cancel,
                )
                .await
                {
                    tracing::error!(error = %e, "clear_failsafe subscriber failed");
                }
            });
        }

        // -----------------------------------------------------------------
        // Resume-instruction subscriber on `roz.tasks.{worker_id}` (D-10,
        // Plan 24-12 Task 4). The server publishes one `ResumeInstruction`
        // per in-flight task on reconnect; the worker verifies the signed
        // envelope, parses the instruction, and runs the D-11 recovery
        // gate via `handle_resume_instruction`. On `SafeStateWait` we emit
        // a `SessionEvent::RecoveryPending` on the session broadcast bus so
        // operators see the state transition (FS-03 SC#5).
        // -----------------------------------------------------------------
        {
            let rt_nats = nats.clone();
            let rt_ctx = ctx_shared.clone();
            let rt_wal = wal_arc.clone();
            let rt_session_tx = session_event_tx.clone();
            let rt_worker_id = config.worker_id.clone();
            let rt_cancel = phase24_cancel.clone();
            tokio::spawn(async move {
                let subject = match roz_nats::Subjects::worker_tasks(&rt_worker_id) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "invalid worker_id for tasks subject; skipping resume subscriber");
                        return;
                    }
                };
                let mut sub = match rt_nats.subscribe(subject.clone()).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, %subject, "failed to subscribe to resume instructions");
                        return;
                    }
                };
                tracing::info!(%subject, "resume-instruction subscriber ready");
                loop {
                    tokio::select! {
                        maybe_msg = futures::StreamExt::next(&mut sub) => {
                            let Some(msg) = maybe_msg else {
                                tracing::warn!("resume-instruction subscription ended");
                                return;
                            };
                            // Phase 26.3 D-06: extract server's trace on first line
                            // so resume-instruction handling stitches under the
                            // originating server span.
                            if let Some(ref headers) = msg.headers {
                                roz_nats::trace::extract_and_link_parent(headers);
                            }
                            let header = msg
                                .headers
                                .as_ref()
                                .and_then(|h| h.get(roz_core::signing::HEADER_NAME).map(|v| v.to_string()));
                            if let Err(e) = rt_ctx.verify_inbound_worker(header.as_deref(), &msg.payload) {
                                tracing::warn!(error = %e, "resume instruction signature rejected");
                                continue;
                            }
                            let instruction: roz_core::reconnect::ResumeInstruction =
                                match serde_json::from_slice(&msg.payload) {
                                    Ok(i) => i,
                                    Err(e) => {
                                        tracing::warn!(error = %e, "resume instruction parse failed");
                                        continue;
                                    }
                                };
                            let now = chrono::Utc::now().timestamp();
                            match roz_worker::reconnect_handshake::handle_resume_instruction(
                                &instruction,
                                &rt_wal,
                                now,
                            ) {
                                Ok(Some(event)) => {
                                    // Wrap in EventEnvelope — shape copied
                                    // from the 15-06 EdgeTransportDegraded
                                    // emission path so the broadcast
                                    // channel's payload shape matches.
                                    let envelope = roz_core::session::event::EventEnvelope {
                                        event_id: roz_core::session::event::EventId::new(),
                                        correlation_id: roz_core::session::event::CorrelationId::new(),
                                        parent_event_id: None,
                                        timestamp: chrono::Utc::now(),
                                        event,
                                        trace_id: None,
                                        span_id: None,
                                    };
                                    if let Err(e) = rt_session_tx.send(envelope) {
                                        tracing::debug!(
                                            error = %e,
                                            "RecoveryPending event dropped (no live receivers)"
                                        );
                                    }
                                }
                                Ok(None) => {
                                    tracing::debug!(task_id = %instruction.task_id, "resume instruction handled without RecoveryPending");
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "handle_resume_instruction failed");
                                }
                            }
                        }
                        () = rt_cancel.cancelled() => return,
                    }
                }
            });
        }

        // -----------------------------------------------------------------
        // Boot-time checkpoint writer (D-08). The per-task periodic writer
        // lives inside `execute_task` (Plan 24-12 Task 5) with a real
        // `periodic_task_id = task_id.to_string()`. This boot-time writer
        // has `periodic_task_id=""` — it only drains event-driven triggers
        // (such as `CheckpointTrigger::DegradationChange` emitted by the
        // policy push subscriber above) so system-wide degradation events
        // are captured even while no task is active.
        //
        // The `ckpt_tx` / `ckpt_rx` channel pair is declared above so the
        // push subscriber can clone `ckpt_tx` into its closure. The outer
        // `ckpt_tx` binding is dropped immediately after the writer spawn
        // below — the subscriber's cloned sender keeps the channel live
        // for the duration of the spawned subscriber loop.
        // -----------------------------------------------------------------
        {
            let cw_wal = wal_arc.clone();
            let cw_cancel = phase24_cancel.clone();
            tokio::spawn(async move {
                let writer = roz_worker::checkpoint_writer::CheckpointWriter::new(
                    cw_wal,
                    "",
                    0,
                    roz_worker::checkpoint_writer::DEFAULT_CHECKPOINT_INTERVAL,
                    cw_cancel,
                );
                writer.run(ckpt_rx).await;
            });
        }
        // The `ckpt_tx` itself is now live-held by the policy push
        // subscriber's `.clone()` inside its tokio::spawn closure — which
        // runs until `phase24_cancel` fires. The boot-time receiver never
        // observes a disconnected channel as long as the policy subscriber
        // is active. Drop the outer binding here so the Plan 24-09
        // keepalive placeholder is gone.
        drop(ckpt_tx);

        // -----------------------------------------------------------------
        // Telemetry replay loop (FS-02). Fires each time `reconnect_tx`
        // signals that NATS is healthy again.
        // -----------------------------------------------------------------
        let (reconnect_tx, reconnect_rx) = tokio::sync::mpsc::channel::<()>(4);
        {
            let tr_wal = wal_arc.clone();
            let tr_ctx = ctx_shared.clone();
            let tr_nats = nats.clone();
            let tr_worker_id = config.worker_id.clone();
            let tr_cancel = phase24_cancel.clone();
            tokio::spawn(async move {
                let replay = std::sync::Arc::new(roz_worker::telemetry_replay::TelemetryReplay::new(tr_wal, tr_ctx));
                if let Err(e) = roz_worker::telemetry_replay::run_telemetry_replay(
                    replay,
                    tr_nats,
                    tr_worker_id,
                    reconnect_rx,
                    tr_cancel,
                )
                .await
                {
                    tracing::error!(error = %e, "telemetry replay task failed");
                }
            });
        }

        // -----------------------------------------------------------------
        // 1 Hz health heartbeat on `roz.health.{worker_id}` (FS-01 — report
        // only, never triggers motion).
        // -----------------------------------------------------------------
        {
            let hh_nats = nats.clone();
            let hh_ctx = ctx_shared.clone();
            let hh_worker_id = config.worker_id.clone();
            let hh_cancel = phase24_cancel.clone();
            tokio::spawn(async move {
                let subject = match roz_nats::Subjects::health(&hh_worker_id) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "invalid worker_id for health subject");
                        return;
                    }
                };
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let payload = serde_json::to_vec(&serde_json::json!({
                                "worker_id": hh_worker_id,
                                "ts": chrono::Utc::now().to_rfc3339(),
                            }))
                            .unwrap_or_default();
                            let correlation = Uuid::new_v4();
                            match hh_ctx.sign_outbound_worker(correlation, &payload) {
                                Ok(header) => {
                                    if let Err(e) = roz_nats::dispatch::publish_signed(
                                        &hh_nats,
                                        subject.clone(),
                                        payload,
                                        &header,
                                    )
                                    .await
                                    {
                                        tracing::trace!(error = %e, "health heartbeat publish failed");
                                    }
                                }
                                Err(e) => {
                                    tracing::trace!(error = %e, "health heartbeat signing failed");
                                }
                            }
                        }
                        () = hh_cancel.cancelled() => return,
                    }
                }
            });
        }

        // -----------------------------------------------------------------
        // One-shot worker-online publish on boot (FS-03, D-10). A richer
        // async-nats reconnect-callback wiring is deferred to Phase 27's
        // SITL CI (see Plan 24-09 deviations). On boot we publish a
        // snapshot with sentinel `last_checkpoint_id=None` + `last_wal_seq=0`
        // so the server's handshake handler can fail-closed-abort any
        // workflow the worker does not have a fresh checkpoint for.
        // -----------------------------------------------------------------
        let worker_uuid_opt = match Uuid::parse_str(&config.worker_id) {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    worker_id = %config.worker_id,
                    "worker_id is not a UUID; skipping worker_online publish"
                );
                None
            }
        };
        if let Some(worker_uuid) = worker_uuid_opt {
            let online_nats = nats.clone();
            let online_ctx = ctx_shared.clone();
            let online_reconnect = reconnect_tx.clone();
            tokio::spawn(async move {
                let snapshot = WorkerOnlineSnapshot {
                    worker_id: worker_uuid,
                    tenant_id: tenant_uuid,
                    last_checkpoint_id: None,
                    last_wal_seq: 0,
                    tasks_in_progress: vec![],
                };
                if let Err(e) =
                    roz_worker::reconnect_handshake::publish_worker_online(&online_nats, &online_ctx, &snapshot).await
                {
                    tracing::warn!(
                        error = %e,
                        "worker_online publish failed (will retry on next reconnect)"
                    );
                } else {
                    tracing::info!("worker_online snapshot published");
                }
                // Kick the replay loop regardless — buffered frames are
                // worth draining even if the server didn't acknowledge the
                // snapshot this time.
                if let Err(e) = online_reconnect.send(()).await {
                    tracing::debug!(error = %e, "telemetry replay kick dropped (channel closed)");
                }
            });
        } else {
            // Still kick replay drain on boot so any buffered frames leave
            // before signing rolls another rotation.
            let _ = reconnect_tx.try_send(());
        }
    } else {
        tracing::info!(
            "Phase 24 subsystems skipped: signing bootstrap not completed (D-12 rollout or no ROZ_ENCRYPTION_KEY)"
        );
    }

    // D-03 + D-04 + C-03 + C-07: open the Zenoh edge-transport session after
    // NATS connect and host registration. Failure is non-fatal — worker falls
    // back to NATS-only. Plan 15-05 consumes `zenoh_session.clone()` below to
    // construct the ZenohSessionTransport; plan 15-06 will consume the same
    // binding for health monitors without re-plumbing.
    #[cfg(feature = "zenoh")]
    let zenoh_session: Option<zenoh::Session> =
        match roz_zenoh::session::open_session(config.zenoh_config_path.as_deref()).await {
            Ok(sess) => {
                tracing::info!(
                    mode = "peer",
                    robot_id = %config.worker_id,
                    "zenoh edge transport ready",
                );
                Some(sess)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "zenoh edge transport unavailable; continuing without it",
                );
                None
            }
        };

    // Plan 15-05 (D-19, D-22): compose ZenohSessionTransport when --features zenoh
    // AND ROZ_DEVICE_SIGNING_KEY resolves. All other branches log a warn! and
    // fall back to NATS-only event publish (graceful degradation).
    #[cfg(feature = "zenoh")]
    let zenoh_transport: Option<roz_zenoh::session::ZenohSessionTransport> = match (
        zenoh_session.clone(),
        config
            .device_signing_key
            .as_deref()
            .map(roz_zenoh::envelope::load_signing_key),
    ) {
        (Some(session), Some(Ok(key))) => {
            let signing_key = std::sync::Arc::new(key);
            match roz_zenoh::session::ZenohSessionTransport::open(session, signing_key, config.worker_id.clone()).await
            {
                Ok(t) => {
                    tracing::info!(robot_id = %config.worker_id, "zenoh edge transport ready (signed)");
                    Some(t)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ZenohSessionTransport open failed; NATS only");
                    None
                }
            }
        }
        (Some(_), None) => {
            tracing::warn!("ROZ_DEVICE_SIGNING_KEY unset; signed Zenoh relay disabled, NATS only (D-22)");
            None
        }
        (Some(_), Some(Err(e))) => {
            tracing::warn!(error = %e, "ROZ_DEVICE_SIGNING_KEY invalid; NATS only");
            None
        }
        (None, _) => {
            // zenoh_session was None (15-01 failed to open) — already warned there.
            None
        }
    };

    // Plan 15-06 (D-11, D-23, D-24): wire the three-mechanism transport health
    // system — EdgeHealthAggregator, heartbeat publisher, liveliness monitor,
    // subsystem freshness monitor — plus the state-transition -> SessionEvent
    // emitter. All cfg-gated on `--features zenoh`.
    //
    // C-03: consumes `zenoh_session: Option<zenoh::Session>` directly from the
    // binding introduced by 15-01 Task 3. No dependency on 15-05's transport
    // struct — health monitors share the same zenoh Session (Session::clone is
    // cheap per D-03) but do not require the signed Zenoh session to be
    // running.
    #[cfg(feature = "zenoh")]
    {
        use std::collections::HashMap;

        use roz_core::edge_health::EdgeHealthAggregator;

        let (aggregator, health_rx, health_handle) = EdgeHealthAggregator::new(64);

        // Drive the aggregator loop.
        tokio::spawn(aggregator.run());

        // Plan 15-06 C-03 acceptance criterion pins this expression verbatim —
        // we consume 15-01's `zenoh_session: Option<zenoh::Session>` binding
        // directly, independent of plan 15-05's transport struct.
        #[expect(
            clippy::option_as_ref_cloned,
            reason = "plan 15-06 acceptance criterion pins `zenoh_session.as_ref().cloned()` verbatim (C-03)"
        )]
        if let Some(session_clone) = zenoh_session.as_ref().cloned() {
            match roz_zenoh::health::spawn_heartbeat_publisher(
                session_clone.clone(),
                config.worker_id.clone(),
                health_rx.clone(),
                roz_zenoh::health::HEARTBEAT_CADENCE,
            )
            .await
            {
                Ok(_h) => tracing::info!("transport health heartbeat publisher started"),
                Err(e) => tracing::warn!(error = %e, "transport health heartbeat publisher failed"),
            }

            if let Err(e) =
                roz_zenoh::health::spawn_liveliness_monitor(session_clone.clone(), health_handle.clone()).await
            {
                tracing::warn!(error = %e, "liveliness monitor failed to start");
            }

            // Four edge-state-bus summary topics tracked per D-23.
            let subsystems: HashMap<&'static str, String> = HashMap::from([
                ("telemetry_summary", "roz/*/telemetry/summary".to_string()),
                ("controller_evidence", "roz/*/controller/evidence".to_string()),
                ("safety_interventions", "roz/*/safety/interventions".to_string()),
                ("perception_availability", "roz/*/perception/availability".to_string()),
            ]);
            if let Err(e) = roz_zenoh::health::spawn_subsystem_freshness_monitor(
                session_clone,
                subsystems,
                health_handle.clone(),
                roz_zenoh::health::SUBSYSTEM_FRESHNESS,
            )
            .await
            {
                tracing::warn!(error = %e, "freshness monitor failed to start");
            }
        }

        // State-transition watcher: emits SessionEvent::EdgeTransportDegraded
        // on the worker-level broadcast bus whenever the aggregator's
        // EdgeTransportHealth value changes.
        let mut health_rx_emitter = health_rx.clone();
        let session_event_tx_for_health = session_event_tx.clone();
        tokio::spawn(async move {
            let mut last = health_rx_emitter.borrow().clone();
            while health_rx_emitter.changed().await.is_ok() {
                let current = health_rx_emitter.borrow_and_update().clone();
                if current != last {
                    let affected_capabilities = match &current {
                        roz_core::edge_health::EdgeTransportHealth::Degraded { affected } => affected.clone(),
                        _ => Vec::new(),
                    };
                    let event = roz_core::session::event::SessionEvent::EdgeTransportDegraded {
                        transport: "zenoh".to_string(),
                        health: current.clone(),
                        affected_capabilities,
                    };
                    let envelope = roz_core::session::event::EventEnvelope {
                        event_id: roz_core::session::event::EventId::new(),
                        correlation_id: roz_core::session::event::CorrelationId::new(),
                        parent_event_id: None,
                        timestamp: chrono::Utc::now(),
                        event,
                        trace_id: None,
                        span_id: None,
                    };
                    // `.send` returns Err only when there are no live receivers;
                    // the _session_event_rx_keepalive at worker main keeps at
                    // least one receiver alive so this branch is informational.
                    if let Err(e) = session_event_tx_for_health.send(envelope) {
                        tracing::warn!(error = %e, "no SessionEvent receivers for EdgeTransportDegraded");
                    }
                    tracing::info!(?current, "edge transport health transition");
                    last = current;
                }
            }
        });
    }

    // Plan 24-13 Task 3: `session_event_tx` is now consumed per-task by
    // the execute_task dispatch loop (cloned into `task_session_event_tx`
    // in the subscribe loop below), so the previous feature-gated
    // `drop(session_event_tx)` used for the zenoh-off build is no longer
    // needed — the cloned sender keeps the broadcast alive for the worker
    // lifetime. The `_session_event_rx_keepalive` binding at the channel
    // construction site still keeps the receiver side from being dropped
    // when no subscribers are attached yet.

    // Plan 15-10 (ZEN-05 gap closure): instantiate EdgeStateBusRunner and
    // ZenohCoordinator so the edge-horizontal subsystems have live worker
    // call sites. Per VERIFICATION.md gap report: SC-5 names "sensor sharing"
    // (EdgeStateBusRunner) and "pose coordination" (ZenohCoordinator) — both
    // primitives existed in roz-zenoh but had zero references from this
    // crate. This block closes that gap.
    //
    // Handles are retained in a local binding so publishers and liveliness
    // tokens live for the worker lifetime. Downstream plans will wire live
    // producers (spatial bridge -> publish_pose loop; telemetry aggregator
    // -> publish(&TELEMETRY_SUMMARY, ...)) into these handles.
    #[cfg(feature = "zenoh")]
    #[expect(
        clippy::option_as_ref_cloned,
        reason = "plan 15-10 reuses the 15-06 C-03 convention for consuming zenoh_session"
    )]
    let _edge_transport_handles: Option<roz_worker::zenoh_edge::EdgeTransportHandles> =
        match zenoh_session.as_ref().cloned() {
            Some(session) => match roz_worker::zenoh_edge::start_edge_subsystems(session, &config.worker_id).await {
                Ok(handles) => {
                    tracing::info!(
                        robot_id = %config.worker_id,
                        "edge state bus + coordinator wired",
                    );
                    Some(handles)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "edge state bus / coordinator startup failed; continuing without",
                    );
                    None
                }
            },
            None => None,
        };

    // Spawn edge agent session relay (handles gRPC sessions relayed via NATS).
    let relay_nats = nats.clone();
    let relay_worker_id = config.worker_id.clone();
    let relay_config = config.clone();
    let relay_estop_rx = estop_rx.clone();
    let relay_camera_mgr = camera_manager.clone();
    // Phase 23 FS-04 (plan 23-12): thread the worker signing context into the
    // session relay so authenticity-bearing runtime_checkpoint + event-envelope
    // publishes carry a roz-sig-v1 header. `None` falls back to the D-12
    // unsigned legacy path (pre-rollout workers).
    let relay_signing_ctx = signing_ctx.clone();
    // C-01 narrowed (plan 15-04 + 15-05): `event_transport` is the single
    // abstracted publish seam. When `--features zenoh` AND the device signing
    // key resolves, we wrap NatsSessionTransport + ZenohSessionTransport in a
    // DualPublishTransport (NATS primary, Zenoh best-effort secondary per D-19).
    // Otherwise the inline Phase 13 NATS path runs verbatim (D-18 byte-stable).
    let nats_event_transport = roz_worker::transport_nats::NatsSessionTransport::new(nats.clone());
    #[cfg(feature = "zenoh")]
    let event_transport: Option<std::sync::Arc<dyn roz_core::transport::SessionTransport>> = match zenoh_transport {
        Some(zt) => {
            tracing::info!("session relay using DualPublishTransport (NATS primary + Zenoh secondary)");
            Some(std::sync::Arc::new(roz_core::transport::DualPublishTransport::new(
                nats_event_transport,
                zt,
            )))
        }
        None => Some(std::sync::Arc::new(nats_event_transport)),
    };
    #[cfg(not(feature = "zenoh"))]
    let event_transport: Option<std::sync::Arc<dyn roz_core::transport::SessionTransport>> =
        Some(std::sync::Arc::new(nats_event_transport));
    // Phase 26.7 D-16: optional ArtifactServiceClient for copper finalize.
    // connect_lazy avoids blocking boot on server reachability; soft-fail
    // per D-16 means a missing or never-connected client just produces a
    // finalize_copper_archive warn-log, never a session-end block.
    let artifact_client: Option<
        roz_worker::roz_v1::artifact_service_client::ArtifactServiceClient<tonic::transport::Channel>,
    > = match tonic::transport::Channel::from_shared(config.api_url.clone()) {
        Ok(endpoint) => Some(roz_worker::roz_v1::artifact_service_client::ArtifactServiceClient::new(
            endpoint.connect_lazy(),
        )),
        Err(e) => {
            tracing::warn!(
                error = %e,
                api_url = %config.api_url,
                "failed to build artifact gRPC channel; copper archival will skip uploads"
            );
            None
        }
    };
    // Phase 26.8 D-08: lift MavlinkBackend to worker-boot scope so session-relay
    // can thread a handle into handle_edge_session (which runs under
    // spawn_session_relay, NOT per-task). Absence = None = ulog finalize is a
    // silent no-op (the common case for non-FC deployments).
    //
    // BREAKING CHANGE from Phase 25 per-task scope: one UDP bind / serial open
    // per worker lifetime. Phase 25 regression tests (mavlink_backend_null_key,
    // qgc_coexistence, log_fanout) remain green — no observable regression.
    //
    // Construction is gated on `worker_host_id` being resolved by
    // `register_host` above. Without a host UUID, `construct_mavlink_backend`
    // cannot derive the MAVLink sysid deterministically (main.rs:97-101), so
    // we skip construction with a debug-log rather than burning a random UUID.
    let mavlink_backend: Option<std::sync::Arc<roz_mavlink::MavlinkBackend>> = if let Some(hid) = worker_host_id {
        construct_mavlink_backend(&config.mavlink, hid).await
    } else {
        tracing::debug!(
            "worker_host_id unresolved (no API registration) — skipping boot-scope MavlinkBackend construction"
        );
        None
    };
    let mavlink_backend_for_relay = mavlink_backend.clone();
    tokio::spawn(async move {
        if let Err(e) = roz_worker::session_relay::spawn_session_relay(
            relay_nats,
            relay_worker_id,
            relay_config,
            relay_estop_rx,
            relay_camera_mgr,
            event_transport,
            relay_signing_ctx,
            artifact_client,
            mavlink_backend_for_relay,
        )
        .await
        {
            tracing::error!(error = %e, "session relay exited");
        }
    });
    // The non-cloned `mavlink_backend` remains available in the main() scope
    // in case any later boot step wants it. Suppress unused-variable noise
    // without taking a Drop path — this Arc is a weak keepalive.
    let _ = &mavlink_backend;

    // Subscribe to task invocations
    let worker_id = &config.worker_id;
    let subject = format!("invoke.{worker_id}.>");
    let mut sub = nats.subscribe(subject.clone()).await?;
    tracing::info!(subject, "subscribed to invocations, waiting for tasks");

    let restate_url = config.restate_url.clone();

    // Worker-local counter of inbound signature verification failures.
    // Bumped on every dropped message; tracing is the surface until a
    // first-class metrics stack lands (scope of a later plan). Atomic because
    // it's read via `.load()` in tests and incremented from the single
    // subscribe loop today.
    let inbound_verify_failures = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    while let Some(msg) = sub.next().await {
        watchdog.pet();

        // Phase 26.3 D-06 — SC6-critical: extract server's trace context on the
        // FIRST line (before e-stop check, signature verify, deserialize) so the
        // `worker.execute_task` span created at ~:2633 inherits the server's
        // `task_dispatch` span. Without this line, SC6 (cross-process trace
        // stitch) fails.
        if let Some(ref headers) = msg.headers {
            roz_nats::trace::extract_and_link_parent(headers);
        }

        // 1. E-stop short-circuit (pre-existing). Must come BEFORE signature
        //    verify per RESEARCH.md integration-point pitfall 2: a tripped
        //    e-stop means we reject the message regardless of signature
        //    validity to avoid racing the e-stop path with crypto work.
        if *estop_rx.borrow() {
            tracing::error!("E-STOP active — rejecting task invocation");
            continue;
        }

        tracing::info!(
            subject = %msg.subject,
            bytes = msg.payload.len(),
            "received invocation"
        );

        // 2. Phase 23 FS-04: signature verify.
        //
        // If signing is enabled for this worker, verify the `roz-sig-v1`
        // header before any serde_json::from_slice on the payload — any
        // tamper or unsigned-dispatch attack is caught here.
        //
        // D-15 bounded refetch: on first verify failure, attempt one
        // `force_rotate` + retry. Covers the case where the server rotated
        // its outbound key and this worker still has the stale cached copy.
        if let Some(ctx) = signing_ctx.as_ref() {
            let header_value = msg
                .headers
                .as_ref()
                .and_then(|h| h.get(HEADER_NAME))
                .map(async_nats::HeaderValue::as_str);

            if let Err(err) = ctx.verify_inbound_worker(header_value, &msg.payload) {
                // Attempt one bounded refetch + retry for signature failures
                // that look like a server-key rotation.
                let retry_ok = matches!(
                    err,
                    WorkerSigningError::Signature(SignatureError::InvalidSignature)
                        | WorkerSigningError::UnknownServerKeyVersion(_)
                ) && signing_rotate_ctx.is_some()
                    && {
                        let (key_provider, dir) = signing_rotate_ctx.as_ref().expect("checked above");
                        let current = ctx.material.read().clone();
                        match roz_worker::signing_key::force_rotate(&current, dir, &http, &config.api_url, key_provider)
                            .await
                        {
                            Ok(new_mat) => {
                                *ctx.material.write() = new_mat;
                                ctx.verify_inbound_worker(header_value, &msg.payload).is_ok()
                            }
                            Err(rotate_err) => {
                                tracing::warn!(
                                    error = %rotate_err,
                                    "force_rotate after inbound verify failure failed"
                                );
                                false
                            }
                        }
                    };

                if !retry_ok {
                    inbound_verify_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::error!(
                        err = ?err,
                        subject = %msg.subject,
                        total_failures = inbound_verify_failures.load(std::sync::atomic::Ordering::Relaxed),
                        "inbound dispatch signature verification failed; dropping message"
                    );
                    continue;
                }
            }
        }

        // 3. Deserialize — safe because (when signing is enabled) the payload
        //    bytes have been bound by a verified signature.
        let invocation: TaskInvocation = match serde_json::from_slice(&msg.payload) {
            Ok(inv) => inv,
            Err(e) => {
                tracing::error!(error = %e, "failed to deserialize TaskInvocation");
                continue;
            }
        };

        // Phase 26.3 D-09 + reviewer HIGH #4: header-wins migration with real
        // body-fallback. Headers are the canonical path (the extract call at the
        // top of this `while let` already ran `extract_and_link_parent` on the
        // subscribe closure's first line when headers were present). This branch
        // is the rolling-deploy fallback: old server (pre-26.3) that still sends
        // body-only traceparent → we parse it and set_parent the worker's span so
        // cross-process stitching still works until the entire fleet is on 26.3+.
        if let Some(ref tp) = invocation.traceparent {
            roz_nats::trace::extract_and_link_parent_from_traceparent(tp);
            tracing::debug!(
                traceparent = %tp,
                task_id = %invocation.task_id,
                "legacy body traceparent parsed + set_parent (rolling-deploy fallback — prefer NATS headers)"
            );
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
        // Phase 23 FS-04: clone the per-worker signing context into the
        // per-task spawn so every publish_task_status inside execute_task
        // goes through the signed path (plan 23-08 Task 2 integration).
        let task_signing_ctx = signing_ctx.clone();

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
                task_signing_ctx.as_ref(),
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
        // Plan 24-12 Tasks 1/3/5: clone the module-level shared state into
        // each spawned task so the pre-dispatch gate, the copper handle, and
        // the per-task checkpoint writer all see the live worker state.
        // Plan 24-13 Task 3: clone the broadcast session_event_tx so the
        // per-task mpsc→broadcast forwarder inside execute_task can publish
        // SessionEvent::SafetyViolation on the same fan-out stream used by
        // the resume subscriber.
        let task_policy_cache = policy_cache.clone();
        let task_hot_policy = hot_policy.clone();
        let task_copper_hot_policy = copper_hot_policy.clone();
        let task_telemetry_bp = telemetry_backpressure.clone();
        let task_worker_wal = worker_wal.clone();
        let task_session_event_tx = session_event_tx.clone();
        // Phase 26-12 OBS-01: clone the worker-wide copper state pointer
        // into each spawned task; when the task spawns a `CopperHandle`,
        // it installs a pose snapshot here so the worker-wide 10 Hz
        // telemetry loop can publish `end_effector_pose`.
        let task_shared_copper_state = shared_copper_state.clone();
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
                    task_signing_ctx,
                    task_policy_cache,
                    task_hot_policy,
                    task_copper_hot_policy,
                    task_telemetry_bp,
                    task_worker_wal,
                    task_session_event_tx,
                    task_shared_copper_state,
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
            declared_max_linear_m_per_s: None,
            declared_max_angular_rad_per_s: None,
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
