//! Edge agent session relay — subscribes to NATS session requests and runs local agent loops.
//!
//! When `agent_placement` is Edge, the server relays gRPC session messages to the worker
//! via NATS. This module handles the worker side: subscribing to
//! `session.{worker_id}.*.request`, spawning a per-session `AgentLoop`, and publishing
//! responses back on `session.{worker_id}.{session_id}.response`.
//!
//! Messages use JSON envelopes for debuggability (not protobuf binary).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::constitution::build_constitution;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::safety::SafetyStack;
use roz_nats::subjects::Subjects;

use crate::camera::CameraManager;
use crate::config::WorkerConfig;

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

    // Subscribe to this specific session's requests.
    let request_subject = Subjects::session_request(worker_id, session_id)?;
    let mut session_sub = nats.subscribe(request_subject).await?;

    // Send SessionStarted response.
    let started = serde_json::json!({
        "type": "session_started",
        "session_id": session_id,
        "model": config.model_name,
    });
    nats.publish(response_subject.clone(), serde_json::to_vec(&started)?.into())
        .await?;

    tracing::info!(session_id, model = %config.model_name, "edge session started");

    // Build the agent model using shared factory.
    let model = crate::model_factory::build_model(config)?;

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let mut extensions = roz_agent::dispatch::Extensions::new();

    // Register camera perception tools when cameras are available.
    if let Some(ref cam_mgr) = camera_manager {
        extensions.insert(cam_mgr.clone());
        let shared_vision_config = Arc::new(tokio::sync::RwLock::new(roz_core::edge::vision::VisionConfig::default()));
        extensions.insert(shared_vision_config);
        dispatcher.register_with_category(
            Box::new(crate::camera::perception::CaptureFrameTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(crate::camera::perception::ListCamerasTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(crate::camera::perception::SetVisionStrategyTool),
            roz_core::tools::ToolCategory::Pure,
        );
        tracing::info!(session_id, "camera perception tools registered for edge session");
    }

    let guards: Vec<Box<dyn roz_agent::safety::SafetyGuard>> = vec![Box::new(
        roz_agent::safety::guards::VelocityLimiter::new(config.max_velocity.unwrap_or(1.5)),
    )];
    let safety = SafetyStack::new(guards);
    let spatial: Box<dyn roz_agent::spatial_provider::SpatialContextProvider> =
        Box::new(crate::camera::snapshot::CameraSpatialProvider::new());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

    // Parse execution mode from start_msg (E8: use extracted function).
    // Default to OodaReAct for edge sessions (workers with physical capabilities).
    let session_mode = parse_edge_session_mode(&start_msg, config.max_velocity.is_some());
    let constitution = build_constitution(session_mode);

    tracing::info!(session_id, ?session_mode, "edge session mode resolved");

    // Process subsequent messages on this session's dedicated subscription.
    while let Some(msg) = session_sub.next().await {
        if *estop_rx.borrow() {
            tracing::error!(session_id, "E-STOP received — terminating edge session");
            let error = serde_json::json!({"type": "error", "message": "E-STOP activated"});
            if let Ok(payload) = serde_json::to_vec(&error) {
                let _ = nats.publish(response_subject.clone(), payload.into()).await;
            }
            break;
        }

        let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
            tracing::warn!(session_id, "edge session: failed to deserialize message");
            continue;
        };

        let msg_type = envelope["type"].as_str().unwrap_or("");

        match msg_type {
            "user_message" => {
                let user_text = envelope["text"].as_str().unwrap_or("").to_string();

                let agent_cancel = CancellationToken::new();
                let input = AgentInput {
                    task_id: session_id.to_string(),
                    tenant_id: "edge".to_string(),
                    system_prompt: vec![constitution.clone()],
                    user_message: user_text,
                    max_cycles: 20,
                    max_tokens: 8192,
                    max_context_tokens: 200_000,
                    mode: session_mode,
                    tool_choice: None,
                    response_schema: None,
                    streaming: false,
                    history: Vec::new(),
                    phases: Vec::new(),
                    cancellation_token: Some(agent_cancel.clone()),
                    control_mode: roz_core::safety::ControlMode::for_remote(),
                };

                // Spawn keepalive task for long agent turns.
                let keepalive_nats = nats.clone();
                let keepalive_subject = response_subject.clone();
                let keepalive_cancel = CancellationToken::new();
                let kc = keepalive_cancel.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(5));
                    loop {
                        tokio::select! {
                            _ = interval.tick() => {
                                let msg = serde_json::json!({"type": "keepalive"});
                                if let Ok(payload) = serde_json::to_vec(&msg) {
                                    let _ = keepalive_nats
                                        .publish(keepalive_subject.clone(), payload.into())
                                        .await;
                                }
                            }
                            () = kc.cancelled() => return,
                        }
                    }
                });

                let mut estop_rx = estop_rx.clone();
                let agent_result = tokio::select! {
                    result = agent.run(input) => result,
                    _ = estop_rx.changed() => {
                        if *estop_rx.borrow() {
                            agent_cancel.cancel(); // cooperative cancel first
                            keepalive_cancel.cancel();
                            tracing::error!(session_id, "E-STOP fired during agent execution — aborting turn");
                            let error = serde_json::json!({"type": "error", "message": "E-STOP activated during execution"});
                            if let Ok(payload) = serde_json::to_vec(&error) {
                                let _ = nats.publish(response_subject.clone(), payload.into()).await;
                            }
                            break;
                        }
                        continue;
                    }
                };

                keepalive_cancel.cancel();

                match agent_result {
                    Ok(output) => {
                        // Send text response (may be None if agent produced no text).
                        if let Some(ref text) = output.final_response {
                            let text_delta = serde_json::json!({
                                "type": "text_delta",
                                "text": text,
                            });
                            nats.publish(response_subject.clone(), serde_json::to_vec(&text_delta)?.into())
                                .await?;
                        }

                        // Send turn complete with usage.
                        let turn_complete = serde_json::json!({
                            "type": "turn_complete",
                            "input_tokens": output.total_usage.input_tokens,
                            "output_tokens": output.total_usage.output_tokens,
                            "stop_reason": "end_turn",
                        });
                        nats.publish(response_subject.clone(), serde_json::to_vec(&turn_complete)?.into())
                            .await?;
                    }
                    Err(e) => {
                        tracing::error!(session_id, error = %e, "edge session agent error");
                        let error_msg = serde_json::json!({
                            "type": "error",
                            "message": e.to_string(),
                        });
                        nats.publish(response_subject.clone(), serde_json::to_vec(&error_msg)?.into())
                            .await?;
                    }
                }
            }
            "cancel_session" => {
                tracing::info!(session_id, "edge session cancelled by server");
                break;
            }
            _ => {
                tracing::debug!(msg_type, session_id, "unhandled edge session message type");
            }
        }
    }

    tracing::info!(session_id, "edge session ended");
    Ok(())
}

/// Parse the agent loop mode from a `start_session` message.
///
/// Explicit `"mode"` field takes precedence. When absent, defaults to
/// `OodaReAct` if the worker has physical capabilities (`has_physical`),
/// otherwise `React`.
fn parse_edge_session_mode(start_msg: &serde_json::Value, has_physical: bool) -> AgentLoopMode {
    match start_msg["mode"].as_str() {
        Some("react") => AgentLoopMode::React,
        Some("ooda_react") => AgentLoopMode::OodaReAct,
        _ => {
            if has_physical {
                AgentLoopMode::OodaReAct
            } else {
                AgentLoopMode::React
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse_edge_session_mode(&msg, true), AgentLoopMode::React);
        assert_eq!(parse_edge_session_mode(&msg, false), AgentLoopMode::React);
    }

    #[test]
    fn parse_edge_session_mode_explicit_ooda() {
        let msg = serde_json::json!({"type": "start_session", "mode": "ooda_react"});
        assert_eq!(parse_edge_session_mode(&msg, false), AgentLoopMode::OodaReAct);
    }

    #[test]
    fn parse_edge_session_mode_absent_defaults_by_physical() {
        let msg = serde_json::json!({"type": "start_session"});
        // Physical worker defaults to OodaReAct
        assert_eq!(parse_edge_session_mode(&msg, true), AgentLoopMode::OodaReAct);
        // Non-physical worker defaults to React
        assert_eq!(parse_edge_session_mode(&msg, false), AgentLoopMode::React);
    }

    #[test]
    fn parse_edge_session_mode_unknown_value_treated_as_absent() {
        let msg = serde_json::json!({"type": "start_session", "mode": "turbo"});
        assert_eq!(parse_edge_session_mode(&msg, true), AgentLoopMode::OodaReAct);
        assert_eq!(parse_edge_session_mode(&msg, false), AgentLoopMode::React);
    }
}
