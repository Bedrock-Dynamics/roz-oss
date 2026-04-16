use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_core::tools::ToolCall;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig};

use crate::tui::convert::{struct_to_value, value_to_struct};
use crate::tui::proto::roz_v1::{
    self, SessionRequest, StartSession, UserMessage, agent_service_client::AgentServiceClient, session_request,
    session_response,
};
use crate::tui::provider::{AgentEvent, ProviderConfig};
use crate::tui::tools;

struct CanonicalToolRequest {
    tool_call_id: String,
    tool_name: String,
    parameters: serde_json::Value,
    #[cfg_attr(not(test), allow(dead_code))]
    timeout_ms: u32,
}

/// Options for local tool execution in cloud mode.
pub struct LocalToolOpts {
    /// Tool schemas to register with the cloud session (proto format).
    pub proto_schemas: Vec<roz_v1::ToolSchema>,
    /// Local dispatcher for executing tools client-side.
    pub dispatcher: ToolDispatcher,
    /// Shared extensions for tool context (e.g. `cmd_tx`, `ControlInterfaceManifest`).
    pub extensions: Extensions,
    /// Kept alive to prevent the Copper controller thread from halting on drop.
    pub _copper_handle: Option<roz_copper::handle::CopperHandle>,
}

/// Convert `roz_core::tools::ToolSchema` to the proto `ToolSchema` message.
///
/// Uses the actual `ToolCategory` from the dispatcher instead of hardcoding
/// all tools as `Physical`. Pure tools (e.g. `get_robot_state`, `read_file`)
/// are tagged `ToolCategoryPure` so the server can optimise dispatch.
fn core_schema_to_proto(
    schema: &roz_core::tools::ToolSchema,
    category: roz_core::tools::ToolCategory,
) -> roz_v1::ToolSchema {
    let proto_category = match category {
        roz_core::tools::ToolCategory::Physical => roz_v1::ToolCategoryHint::ToolCategoryPhysical,
        roz_core::tools::ToolCategory::Pure => roz_v1::ToolCategoryHint::ToolCategoryPure,
        roz_core::tools::ToolCategory::CodeSandbox => roz_v1::ToolCategoryHint::ToolCategoryCodeSandbox,
    };
    roz_v1::ToolSchema {
        name: schema.name.clone(),
        description: schema.description.clone(),
        parameters_schema: Some(value_to_struct(schema.parameters.clone())),
        timeout_ms: 30_000,
        category: proto_category.into(),
    }
}

/// Build `LocalToolOpts` from the unified tool set, optionally spawning Copper.
///
/// Always returns a valid `LocalToolOpts` -- CLI built-ins are always present,
/// daemon tools from the embodiment manifest are added when available, and the
/// Copper WASM pipeline is spawned when `[daemon.websocket]` + `[channels]`
/// are present.
pub fn build_local_tool_opts(project_dir: &std::path::Path) -> LocalToolOpts {
    let all = tools::build_all_tools_with_copper(project_dir);
    let proto_schemas = all
        .schemas
        .iter()
        .map(|(schema, category)| core_schema_to_proto(schema, *category))
        .collect();
    LocalToolOpts {
        proto_schemas,
        dispatcher: all.dispatcher,
        extensions: all.extensions,
        _copper_handle: all.copper_handle,
    }
}

/// Load project context for the cloud agent session.
///
/// Returns the embodiment-manifest system prompt and AGENTS.md / ROBOT.md content
/// as separate string blocks for the `StartSession.project_context` field.
fn load_cloud_project_context() -> Vec<String> {
    let project_dir = std::path::Path::new(".");
    let mut ctx = vec![];
    // Embodiment system prompt from embodiment.toml (legacy robot.toml fallback accepted)
    if let Ok(manifest) = roz_core::manifest::EmbodimentManifest::load_from_project_dir(project_dir) {
        let prompt = manifest.to_system_prompt();
        if !prompt.is_empty() {
            ctx.push(prompt);
        }
    }
    // AGENTS.md / ROBOT.md
    if let Some(project_ctx) = crate::tui::context::load_project_context() {
        ctx.push(project_ctx);
    }
    ctx
}

/// Execute a tool locally and send the result back to the server via gRPC.
///
/// Spawned as a tokio task for each incoming `ToolRequest`.
async fn execute_tool_locally(
    tool_request: CanonicalToolRequest,
    dispatcher: Arc<Mutex<ToolDispatcher>>,
    extensions: Extensions,
    req_tx: tokio::sync::mpsc::Sender<SessionRequest>,
    event_tx: async_channel::Sender<AgentEvent>,
) {
    let tool_name = tool_request.tool_name;
    let tool_call_id = tool_request.tool_call_id;

    // Security: log when the cloud agent triggers a Physical tool locally.
    // Physical tools have real-world side effects (actuation, file writes, code
    // execution) and must always be auditable. Check the dispatcher's category
    // instead of maintaining a hardcoded list.
    {
        let d = dispatcher.lock().await;
        if d.category(&tool_name) == roz_core::tools::ToolCategory::Physical {
            tracing::info!(tool = %tool_name, "cloud agent executing physical tool locally");
        }
    }

    let params_json = tool_request.parameters;

    let ctx = tools::default_context_with(extensions);
    let call = ToolCall {
        id: tool_call_id.clone(),
        tool: tool_name.clone(),
        params: params_json,
    };

    let start = Instant::now();
    let tool_result = {
        let d = dispatcher.lock().await;
        d.dispatch(&call, &ctx).await
    };
    let duration_ms = start.elapsed().as_millis();

    let success = tool_result.is_success();
    let result_text = if success {
        serde_json::to_string(&tool_result.output).unwrap_or_default()
    } else {
        tool_result.error.unwrap_or_else(|| "unknown error".to_string())
    };

    // Display the result in the TUI
    let _ = event_tx
        .send(AgentEvent::ToolResultDisplay {
            name: tool_name,
            content: result_text.clone(),
            is_error: !success,
        })
        .await;

    // Send the result back to the server
    #[allow(clippy::cast_possible_truncation)]
    let _ = req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::ToolResult(roz_v1::ToolResult {
                tool_call_id,
                success,
                result: result_text,
                exit_code: None,
                truncated: false,
                duration_ms: Some(duration_ms as i64),
            })),
        })
        .await;
}

/// Run a long-lived gRPC session against Roz Cloud.
///
/// Unlike BYOK providers (per-turn), Cloud maintains a persistent bidirectional
/// stream. The server runs the agent loop; tools registered via `LocalToolOpts`
/// execute client-side and their results are sent back on the gRPC stream.
#[allow(clippy::too_many_lines)]
pub async fn stream_session(
    config: &ProviderConfig,
    msg_rx: async_channel::Receiver<String>,
    event_tx: async_channel::Sender<AgentEvent>,
    local_tools: LocalToolOpts,
) -> anyhow::Result<()> {
    let api_key = config
        .api_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No Roz Cloud credentials. Run `roz auth login`."))?;

    // Connect with TLS
    let tls = ClientTlsConfig::new().with_native_roots();
    let channel = Channel::from_shared(config.api_url.clone())?
        .tls_config(tls)?
        .connect()
        .await?;

    // Create client with auth interceptor
    let auth_value: tonic::metadata::MetadataValue<_> = format!("Bearer {api_key}").parse()?;
    let mut client = AgentServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
        req.metadata_mut().insert("authorization", auth_value.clone());
        Ok(req)
    });

    // Create request stream
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(32);

    // Extract tool schemas for StartSession
    let tool_schemas = local_tools.proto_schemas.clone();

    // Load project context for the cloud agent (embodiment manifest + AGENTS.md / ROBOT.md)
    let project_context = load_cloud_project_context();

    // Send StartSession with registered tool schemas and project context
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: String::new(),
                host_id: config.host.clone(),
                model: Some(config.model.clone()),
                tools: tool_schemas,
                project_context,
                ..Default::default()
            })),
        })
        .await?;

    // Start bidirectional stream
    let response = client.stream_session(ReceiverStream::new(req_rx)).await?;
    let mut stream = response.into_inner();

    // Keep a handle to check whether the message channel has been closed
    // (non-interactive mode closes the sender after queuing the prompt).
    let msg_closed = msg_rx.clone();

    // Spawn forwarder: user messages -> gRPC requests.
    // The forwarder gets its own clone; when msg_rx closes the loop exits
    // and this clone is dropped.
    let forwarder_tx = req_tx.clone();
    tokio::spawn({
        async move {
            while let Ok(text) = msg_rx.recv().await {
                let _ = forwarder_tx
                    .send(SessionRequest {
                        request: Some(session_request::Request::UserMessage(UserMessage {
                            content: text,
                            ..Default::default()
                        })),
                    })
                    .await;
            }
        }
    });

    // Wrap the original sender in Option so we can drop it after the final
    // TurnComplete in non-interactive mode.  Tool execution tasks clone from
    // this; interactive mode keeps it alive for the entire session.
    let mut req_tx = Some(req_tx);

    // Wrap the dispatcher in an Arc for sharing with tool execution tasks
    let dispatcher = Arc::new(Mutex::new(local_tools.dispatcher));
    let extensions = local_tools.extensions;

    // Receive and map server events
    while let Some(resp) = stream.message().await? {
        let Some(response) = resp.response else {
            continue;
        };
        let session_response::Response::SessionEvent(event) = &response else {
            continue;
        };
        let event = event.clone();

        if let Some(tool_request) = session_event_to_tool_request(&event) {
            let params_display = format_json_value(&tool_request.parameters);

            event_tx
                .send(AgentEvent::ToolRequest {
                    id: tool_request.tool_call_id.clone(),
                    name: tool_request.tool_name.clone(),
                    params: params_display,
                })
                .await?;

            if let Some(ref tx) = req_tx {
                tokio::spawn(execute_tool_locally(
                    tool_request,
                    dispatcher.clone(),
                    extensions.clone(),
                    tx.clone(),
                    event_tx.clone(),
                ));
            }

            continue;
        }

        let Some(event) = session_event_to_agent_event(&event) else {
            continue;
        };

        if matches!(event, AgentEvent::TurnComplete { .. }) && msg_closed.is_closed() {
            req_tx.take();
        }
        event_tx.send(event).await?;
    }

    Ok(())
}

fn session_event_to_agent_event(event: &roz_v1::SessionEventEnvelope) -> Option<AgentEvent> {
    let typed = event.typed_event.as_ref()?;
    match typed {
        roz_v1::session_event_envelope::TypedEvent::SessionStarted(payload) => Some(AgentEvent::Connected {
            model: payload.model_name.clone().unwrap_or_default(),
        }),
        roz_v1::session_event_envelope::TypedEvent::SessionRejected(payload) => {
            Some(AgentEvent::Error(payload.message.clone()))
        }
        roz_v1::session_event_envelope::TypedEvent::SessionFailed(payload) => {
            Some(AgentEvent::Error(if payload.failure.is_empty() {
                "session failed".to_string()
            } else {
                format!("session failed: {}", payload.failure)
            }))
        }
        roz_v1::session_event_envelope::TypedEvent::TextDelta(payload) => {
            Some(AgentEvent::TextDelta(payload.content.clone()))
        }
        roz_v1::session_event_envelope::TypedEvent::ThinkingDelta(payload) => {
            Some(AgentEvent::ThinkingDelta(payload.content.clone()))
        }
        roz_v1::session_event_envelope::TypedEvent::TurnFinished(payload) => Some(AgentEvent::TurnComplete {
            input_tokens: payload.input_tokens,
            output_tokens: payload.output_tokens,
            stop_reason: payload.stop_reason.clone(),
        }),
        roz_v1::session_event_envelope::TypedEvent::ToolCallStarted(payload) => Some(AgentEvent::ToolRequest {
            id: payload.call_id.clone(),
            name: payload.tool_name.clone(),
            params: String::new(),
        }),
        roz_v1::session_event_envelope::TypedEvent::ToolCallFinished(payload) => Some(AgentEvent::ToolResultDisplay {
            name: payload.tool_name.clone(),
            content: payload.result_summary.clone(),
            is_error: false,
        }),
        roz_v1::session_event_envelope::TypedEvent::SkillLoaded(payload) => Some(AgentEvent::ToolResultDisplay {
            name: "skill_loaded".into(),
            content: format!("skill loaded: {} v{}", payload.name, payload.version),
            is_error: false,
        }),
        roz_v1::session_event_envelope::TypedEvent::SkillCrystallized(payload) => Some(AgentEvent::ToolResultDisplay {
            name: "skill_crystallized".into(),
            content: format!(
                "skill crystallized: {} v{} ({})",
                payload.name, payload.version, payload.source
            ),
            is_error: false,
        }),
        roz_v1::session_event_envelope::TypedEvent::ToolUnavailable(payload) => {
            Some(AgentEvent::Error(if payload.reason.is_empty() {
                format!("tool unavailable: {}", payload.tool_name)
            } else {
                format!("tool unavailable: {} ({})", payload.tool_name, payload.reason)
            }))
        }
        roz_v1::session_event_envelope::TypedEvent::ApprovalRequested(payload) => {
            let mut content = if payload.action.is_empty() {
                "approval requested".to_string()
            } else if payload.reason.is_empty() {
                format!("approval requested: {}", payload.action)
            } else {
                format!("approval requested: {} ({})", payload.action, payload.reason)
            };
            if payload.timeout_secs > 0 {
                let _ = write!(content, " [timeout={}s]", payload.timeout_secs);
            }
            Some(AgentEvent::ToolResultDisplay {
                name: "approval_requested".into(),
                content,
                is_error: false,
            })
        }
        roz_v1::session_event_envelope::TypedEvent::ApprovalResolved(payload) => {
            let outcome = payload.outcome.clone().map_or(serde_json::Value::Null, struct_to_value);
            let (content, is_error) = format_approval_outcome(&payload.approval_id, &outcome);
            Some(AgentEvent::ToolResultDisplay {
                name: "approval_resolved".into(),
                content,
                is_error,
            })
        }
        roz_v1::session_event_envelope::TypedEvent::ControllerRolledBack(payload) => {
            Some(AgentEvent::Error(if payload.reason.is_empty() {
                format!("controller rolled back: {}", payload.artifact_id)
            } else {
                format!("controller rolled back: {}", payload.reason)
            }))
        }
        roz_v1::session_event_envelope::TypedEvent::SafetyIntervention(payload) => Some(AgentEvent::Error(format!(
            "safety intervention: {} ({})",
            payload.channel, payload.kind
        ))),
        roz_v1::session_event_envelope::TypedEvent::EdgeTransportDegraded(payload) => {
            Some(AgentEvent::Error(format!("edge degraded: {}", payload.transport)))
        }
        roz_v1::session_event_envelope::TypedEvent::SafePauseEntered(payload) => {
            let reason = if payload.reason.is_empty() {
                "safe pause entered".to_string()
            } else {
                format!("safe pause: {}", payload.reason)
            };
            Some(AgentEvent::Error(reason))
        }
        _ => None,
    }
}

fn session_event_to_tool_request(event: &roz_v1::SessionEventEnvelope) -> Option<CanonicalToolRequest> {
    let roz_v1::session_event_envelope::TypedEvent::ToolCallRequested(payload) = event.typed_event.as_ref()? else {
        return None;
    };

    Some(CanonicalToolRequest {
        tool_call_id: payload.call_id.clone(),
        tool_name: payload.tool_name.clone(),
        parameters: payload
            .parameters
            .clone()
            .map_or_else(|| serde_json::json!({}), struct_to_value),
        timeout_ms: payload.timeout_ms,
    })
}

fn format_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn format_approval_outcome(approval_id: &str, outcome: &serde_json::Value) -> (String, bool) {
    let approval_type = outcome.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
    match approval_type {
        "approved" => (format!("approval resolved: {approval_id} approved"), false),
        "denied" => {
            let reason = outcome
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no reason provided");
            (format!("approval denied: {approval_id} ({reason})"), true)
        }
        "modified" => {
            let fields = outcome
                .get("modifications")
                .and_then(serde_json::Value::as_array)
                .map(|mods| {
                    mods.iter()
                        .filter_map(|modification| modification.get("field").and_then(serde_json::Value::as_str))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if fields.is_empty() {
                (format!("approval modified: {approval_id}"), false)
            } else {
                (
                    format!("approval modified: {approval_id} [{}]", fields.join(", ")),
                    false,
                )
            }
        }
        "partial_approval" => (format!("approval partially granted: {approval_id}"), false),
        _ => (format!("approval resolved: {approval_id}"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::tools::{ToolCategory, ToolSchema};
    use serde_json::json;

    fn typed_envelope(
        event_id: &str,
        correlation_id: &str,
        event_type: &str,
        typed_event: roz_v1::session_event_envelope::TypedEvent,
    ) -> roz_v1::SessionEventEnvelope {
        roz_v1::SessionEventEnvelope {
            event_id: event_id.into(),
            correlation_id: correlation_id.into(),
            parent_event_id: None,
            timestamp: None,
            event_type: event_type.into(),
            typed_event: Some(typed_event),
        }
    }

    fn test_schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.to_string(),
            description: format!("Test tool {name}"),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn core_schema_to_proto_maps_physical_category() {
        let schema = test_schema("move_to");
        let proto = core_schema_to_proto(&schema, ToolCategory::Physical);
        assert_eq!(proto.name, "move_to");
        assert_eq!(
            proto.category,
            i32::from(roz_v1::ToolCategoryHint::ToolCategoryPhysical)
        );
    }

    #[test]
    fn core_schema_to_proto_maps_pure_category() {
        let schema = test_schema("get_robot_state");
        let proto = core_schema_to_proto(&schema, ToolCategory::Pure);
        assert_eq!(proto.name, "get_robot_state");
        assert_eq!(proto.category, i32::from(roz_v1::ToolCategoryHint::ToolCategoryPure));
    }

    #[test]
    fn core_schema_to_proto_maps_code_sandbox_category() {
        let schema = test_schema("execute_code");
        let proto = core_schema_to_proto(&schema, ToolCategory::CodeSandbox);
        assert_eq!(proto.name, "execute_code");
        assert_eq!(
            proto.category,
            i32::from(roz_v1::ToolCategoryHint::ToolCategoryCodeSandbox)
        );
    }

    /// Regression test: previously all tools were hardcoded to Physical.
    #[test]
    fn build_local_tool_opts_preserves_categories() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("embodiment.toml"),
            r#"
[robot]
name = "test"
description = "test"

[channels]
robot_id = "test"
robot_class = "test"
control_rate_hz = 50

[[channels.commands]]
name = "head_pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

[[channels.states]]
name = "head_pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

[daemon]
base_url = "http://localhost:8000"

[daemon.get_state]
method = "GET"
path = "/api/state/full"

[daemon.move_to]
method = "POST"
path = "/api/move/goto"
body = '{"pitch": {{head_pitch}}, "duration": {{duration}}}'
"#,
        )
        .unwrap();

        let opts = build_local_tool_opts(dir.path());

        let get_state = opts.proto_schemas.iter().find(|s| s.name == "get_robot_state").unwrap();
        assert_eq!(
            get_state.category,
            i32::from(roz_v1::ToolCategoryHint::ToolCategoryPure),
            "get_robot_state should be Pure"
        );

        let move_to = opts.proto_schemas.iter().find(|s| s.name == "move_to").unwrap();
        assert_eq!(
            move_to.category,
            i32::from(roz_v1::ToolCategoryHint::ToolCategoryPhysical),
            "move_to should be Physical"
        );

        let read_file = opts.proto_schemas.iter().find(|s| s.name == "read_file").unwrap();
        assert_eq!(
            read_file.category,
            i32::from(roz_v1::ToolCategoryHint::ToolCategoryPure),
            "read_file should be Pure"
        );

        let execute_code = opts.proto_schemas.iter().find(|s| s.name == "execute_code").unwrap();
        assert_eq!(
            execute_code.category,
            i32::from(roz_v1::ToolCategoryHint::ToolCategoryCodeSandbox),
            "execute_code should be CodeSandbox"
        );
    }

    #[test]
    fn session_event_to_agent_event_maps_session_failed() {
        let event = typed_envelope(
            "evt-1",
            "corr-1",
            "session_failed",
            roz_v1::session_event_envelope::TypedEvent::SessionFailed(roz_v1::SessionFailedPayload {
                failure: "controller_trap".into(),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Error(message)) if message == "session failed: controller_trap"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_session_started() {
        let event = typed_envelope(
            "evt-start",
            "corr-start",
            "session_started",
            roz_v1::session_event_envelope::TypedEvent::SessionStarted(roz_v1::SessionStartedPayload {
                session_id: "sess-1".into(),
                mode: "server_canonical".into(),
                blueprint_version: "1.0".into(),
                model_name: Some("claude-sonnet-4-6".into()),
                permissions: vec![],
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Connected { model }) if model == "claude-sonnet-4-6"
        ));
    }

    #[test]
    fn session_event_to_agent_event_ignores_non_terminal_events() {
        let event = typed_envelope(
            "evt-2",
            "corr-2",
            "turn_started",
            roz_v1::session_event_envelope::TypedEvent::TurnStarted(roz_v1::TurnStartedPayload { turn_index: 2 }),
        );

        assert!(session_event_to_agent_event(&event).is_none());
    }

    #[test]
    fn session_event_to_agent_event_maps_tool_call_finished() {
        let event = typed_envelope(
            "evt-3",
            "corr-3",
            "tool_call_finished",
            roz_v1::session_event_envelope::TypedEvent::ToolCallFinished(roz_v1::ToolCallFinishedPayload {
                call_id: "toolu_123".into(),
                tool_name: "read_file".into(),
                result_summary: "read 18 lines".into(),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::ToolResultDisplay { name, content, is_error: false })
                if name == "read_file" && content == "read 18 lines"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_skill_loaded() {
        let event = typed_envelope(
            "evt-skill-loaded",
            "corr-skill-loaded",
            "skill_loaded",
            roz_v1::session_event_envelope::TypedEvent::SkillLoaded(roz_v1::SkillLoadedPayload {
                name: "warehouse-skill".into(),
                version: "0.1.0".into(),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::ToolResultDisplay { name, content, is_error: false })
                if name == "skill_loaded" && content == "skill loaded: warehouse-skill v0.1.0"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_skill_crystallized() {
        let event = typed_envelope(
            "evt-skill-crystallized",
            "corr-skill-crystallized",
            "skill_crystallized",
            roz_v1::session_event_envelope::TypedEvent::SkillCrystallized(roz_v1::SkillCrystallizedPayload {
                name: "warehouse-skill".into(),
                version: "0.2.0".into(),
                source: "local".into(),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::ToolResultDisplay { name, content, is_error: false })
                if name == "skill_crystallized"
                    && content == "skill crystallized: warehouse-skill v0.2.0 (local)"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_tool_unavailable() {
        let event = typed_envelope(
            "evt-unavailable",
            "corr-unavailable",
            "tool_unavailable",
            roz_v1::session_event_envelope::TypedEvent::ToolUnavailable(roz_v1::ToolUnavailablePayload {
                tool_name: "promote_controller".into(),
                reason: "not_registered".into(),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Error(message))
                if message == "tool unavailable: promote_controller (not_registered)"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_approval_requested() {
        let event = typed_envelope(
            "evt-approval-request",
            "corr-approval-request",
            "approval_requested",
            roz_v1::session_event_envelope::TypedEvent::ApprovalRequested(roz_v1::ApprovalRequestedPayload {
                approval_id: "apr-1".into(),
                action: "move_arm".into(),
                reason: "needs human approval".into(),
                timeout_secs: 30,
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::ToolResultDisplay { name, content, is_error: false })
                if name == "approval_requested"
                    && content == "approval requested: move_arm (needs human approval) [timeout=30s]"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_approval_resolved_modified() {
        let outcome = json!({
            "type": "modified",
            "modifications": [
                {"field": "speed", "old_value": "1.0", "new_value": "0.25"}
            ]
        });
        let event = typed_envelope(
            "evt-approval-resolved",
            "corr-approval-resolved",
            "approval_resolved",
            roz_v1::session_event_envelope::TypedEvent::ApprovalResolved(roz_v1::ApprovalResolvedPayload {
                approval_id: "apr-1".into(),
                outcome: Some(crate::tui::convert::value_to_struct(outcome)),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::ToolResultDisplay { name, content, is_error: false })
                if name == "approval_resolved"
                    && content == "approval modified: apr-1 [speed]"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_approval_resolved_denied() {
        let outcome = json!({
            "type": "denied",
            "reason": "too close to operator"
        });
        let event = typed_envelope(
            "evt-approval-denied",
            "corr-approval-denied",
            "approval_resolved",
            roz_v1::session_event_envelope::TypedEvent::ApprovalResolved(roz_v1::ApprovalResolvedPayload {
                approval_id: "apr-2".into(),
                outcome: Some(crate::tui::convert::value_to_struct(outcome)),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::ToolResultDisplay { name, content, is_error: true })
                if name == "approval_resolved"
                    && content == "approval denied: apr-2 (too close to operator)"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_text_delta() {
        let event = typed_envelope(
            "evt-text",
            "corr-text",
            "text_delta",
            roz_v1::session_event_envelope::TypedEvent::TextDelta(roz_v1::TextDeltaPayload {
                content: "hello".into(),
                message_id: Some("msg-1".into()),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(mapped, Some(AgentEvent::TextDelta(content)) if content == "hello"));
    }

    #[test]
    fn session_event_to_agent_event_prefers_typed_text_delta() {
        let event = roz_v1::SessionEventEnvelope {
            event_id: "evt-text-typed".into(),
            correlation_id: "corr-text-typed".into(),
            parent_event_id: None,
            timestamp: None,
            event_type: "text_delta".into(),
            typed_event: Some(roz_v1::session_event_envelope::TypedEvent::TextDelta(
                roz_v1::TextDeltaPayload {
                    content: "typed".into(),
                    message_id: None,
                },
            )),
        };

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(mapped, Some(AgentEvent::TextDelta(content)) if content == "typed"));
    }

    #[test]
    fn session_event_to_agent_event_prefers_typed_tool_unavailable() {
        let event = roz_v1::SessionEventEnvelope {
            event_id: "evt-unavailable-typed".into(),
            correlation_id: "corr-unavailable-typed".into(),
            parent_event_id: None,
            timestamp: None,
            event_type: "tool_unavailable".into(),
            typed_event: Some(roz_v1::session_event_envelope::TypedEvent::ToolUnavailable(
                roz_v1::ToolUnavailablePayload {
                    tool_name: "promote_controller".into(),
                    reason: "not_registered".into(),
                },
            )),
        };

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Error(message))
                if message == "tool unavailable: promote_controller (not_registered)"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_typed_safety_intervention() {
        let event = roz_v1::SessionEventEnvelope {
            event_id: "evt-safety".into(),
            correlation_id: "corr-safety".into(),
            parent_event_id: None,
            timestamp: None,
            event_type: "safety_intervention".into(),
            typed_event: Some(roz_v1::session_event_envelope::TypedEvent::SafetyIntervention(
                roz_v1::SafetyInterventionPayload {
                    channel: "joint_1".into(),
                    raw_value: 4.2,
                    clamped_value: 1.1,
                    kind: "velocity_clamp".into(),
                    reason: "limit exceeded".into(),
                },
            )),
        };

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Error(message))
                if message == "safety intervention: joint_1 (velocity_clamp)"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_typed_controller_rollback() {
        let event = roz_v1::SessionEventEnvelope {
            event_id: "evt-rollback".into(),
            correlation_id: "corr-rollback".into(),
            parent_event_id: None,
            timestamp: None,
            event_type: "controller_rolled_back".into(),
            typed_event: Some(roz_v1::session_event_envelope::TypedEvent::ControllerRolledBack(
                roz_v1::ControllerRolledBackPayload {
                    artifact_id: "ctrl-2".into(),
                    restored_id: "ctrl-1".into(),
                    reason: "candidate divergence exceeded limit".into(),
                },
            )),
        };

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Error(message))
                if message == "controller rolled back: candidate divergence exceeded limit"
        ));
    }

    #[test]
    fn session_event_to_agent_event_maps_session_rejected() {
        let event = typed_envelope(
            "evt-reject",
            "corr-reject",
            "session_rejected",
            roz_v1::session_event_envelope::TypedEvent::SessionRejected(roz_v1::SessionRejectedPayload {
                code: "turn_rejected".into(),
                message: "turn already in progress".into(),
                retryable: false,
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Error(message)) if message == "turn already in progress"
        ));
    }

    #[test]
    fn session_event_to_tool_request_maps_requested_tool() {
        let event = typed_envelope(
            "evt-tool",
            "corr-tool",
            "tool_call_requested",
            roz_v1::session_event_envelope::TypedEvent::ToolCallRequested(roz_v1::ToolCallRequestedPayload {
                call_id: "toolu_123".into(),
                tool_name: "read_file".into(),
                parameters: Some(crate::tui::convert::value_to_struct(json!({"path": "README.md"}))),
                timeout_ms: 1500,
            }),
        );

        let request = session_event_to_tool_request(&event).expect("tool request should parse");
        assert_eq!(request.tool_call_id, "toolu_123");
        assert_eq!(request.tool_name, "read_file");
        assert_eq!(request.timeout_ms, 1500);
        assert_eq!(request.parameters["path"], "README.md");
    }

    #[test]
    fn session_event_to_agent_event_maps_safe_pause() {
        let event = typed_envelope(
            "evt-4",
            "corr-4",
            "safe_pause_entered",
            roz_v1::session_event_envelope::TypedEvent::SafePauseEntered(roz_v1::SafePauseEnteredPayload {
                reason: "operator estop".into(),
                robot_state: "running".into(),
            }),
        );

        let mapped = session_event_to_agent_event(&event);
        assert!(matches!(
            mapped,
            Some(AgentEvent::Error(message)) if message == "safe pause: operator estop"
        ));
    }
}
