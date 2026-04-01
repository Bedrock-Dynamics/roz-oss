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

/// Options for local tool execution in cloud mode.
pub struct LocalToolOpts {
    /// Tool schemas to register with the cloud session (proto format).
    pub proto_schemas: Vec<roz_v1::ToolSchema>,
    /// Local dispatcher for executing tools client-side.
    pub dispatcher: ToolDispatcher,
    /// Shared extensions for tool context (e.g. `cmd_tx`, `ChannelManifest`).
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
/// daemon tools from `robot.toml` are added when available, and the Copper WASM
/// pipeline is spawned when `[daemon.websocket]` + `[channels]` are present.
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
/// Returns the robot.toml system prompt and AGENTS.md / ROBOT.md content
/// as separate string blocks for the `StartSession.project_context` field.
fn load_cloud_project_context() -> Vec<String> {
    let project_dir = std::path::Path::new(".");
    let mut ctx = vec![];
    // Robot system prompt from robot.toml
    let robot_toml = project_dir.join("robot.toml");
    if let Ok(manifest) = roz_core::manifest::RobotManifest::load(&robot_toml) {
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
    tool_request: roz_v1::ToolRequest,
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

    let params_json = tool_request
        .parameters
        .map_or_else(|| serde_json::Value::Object(serde_json::Map::new()), struct_to_value);

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

    // Load project context for the cloud agent (robot.toml + AGENTS.md / ROBOT.md)
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
        let event = match response {
            session_response::Response::SessionStarted(s) => AgentEvent::Connected { model: s.model },
            session_response::Response::TextDelta(d) => AgentEvent::TextDelta(d.content),
            session_response::Response::ThinkingDelta(d) => AgentEvent::ThinkingDelta(d.content),
            session_response::Response::ToolRequest(t) => {
                let params_display = t.parameters.as_ref().map(format_struct).unwrap_or_default();

                // Send the display event so the TUI shows the tool call
                event_tx
                    .send(AgentEvent::ToolRequest {
                        id: t.tool_call_id.clone(),
                        name: t.tool_name.clone(),
                        params: params_display,
                    })
                    .await?;

                // Execute locally (clone the sender if still available)
                if let Some(ref tx) = req_tx {
                    tokio::spawn(execute_tool_locally(
                        t,
                        dispatcher.clone(),
                        extensions.clone(),
                        tx.clone(),
                        event_tx.clone(),
                    ));
                }

                // ToolRequest was already sent above; don't emit a second event.
                continue;
            }
            session_response::Response::TurnComplete(c) => {
                // If no more user messages are coming (non-interactive mode),
                // drop the last sender so the server sees the client stream
                // end and closes its side -- preventing a deadlock.
                // Interactive mode: msg_rx stays open, sender stays alive.
                if msg_closed.is_closed() {
                    req_tx.take();
                }

                let usage = c.usage.unwrap_or_default();
                AgentEvent::TurnComplete {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    stop_reason: c.stop_reason,
                }
            }
            session_response::Response::Error(e) => AgentEvent::Error(e.message),
            session_response::Response::ActivityUpdate(a) => {
                // Could map to UI state changes
                if a.state == "waiting_approval" {
                    // Future: trigger safety approval UI
                }
                continue;
            }
            _ => continue,
        };
        event_tx.send(event).await?;
    }

    Ok(())
}

/// Format a prost Struct as a compact JSON-like string for display.
fn format_struct(s: &prost_types::Struct) -> String {
    s.fields
        .iter()
        .map(|(k, v)| format!("{k}: {}", format_value(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_value(v: &prost_types::Value) -> String {
    use prost_types::value::Kind;
    match &v.kind {
        Some(Kind::StringValue(s)) => format!("\"{s}\""),
        Some(Kind::NumberValue(n)) => format!("{n}"),
        Some(Kind::BoolValue(b)) => format!("{b}"),
        Some(Kind::NullValue(_)) => "null".to_string(),
        _ => "...".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::tools::{ToolCategory, ToolSchema};
    use serde_json::json;

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

    /// Regression test: previously all tools were hardcoded to Physical.
    #[test]
    fn build_local_tool_opts_preserves_categories() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("robot.toml"),
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
    }
}
