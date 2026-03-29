use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig};

use crate::tui::proto::roz_v1::{
    SessionRequest, StartSession, UserMessage, agent_service_client::AgentServiceClient, session_request,
    session_response,
};
use crate::tui::provider::{AgentEvent, ProviderConfig};

/// Run a long-lived gRPC session against Roz Cloud.
///
/// Unlike BYOK providers (per-turn), Cloud maintains a persistent bidirectional
/// stream. The server runs the agent loop and executes tools server-side.
pub async fn stream_session(
    config: &ProviderConfig,
    msg_rx: async_channel::Receiver<String>,
    event_tx: async_channel::Sender<AgentEvent>,
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

    // Send StartSession
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: String::new(),
                host_id: config.host.clone(),
                model: Some(config.model.clone()),
                ..Default::default()
            })),
        })
        .await?;

    // Start bidirectional stream
    let response = client.stream_session(ReceiverStream::new(req_rx)).await?;
    let mut stream = response.into_inner();

    // Spawn forwarder: user messages → gRPC requests
    tokio::spawn({
        let req_tx = req_tx.clone();
        async move {
            while let Ok(text) = msg_rx.recv().await {
                let _ = req_tx
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
                let params = t.parameters.map(|s| format_struct(&s)).unwrap_or_default();
                AgentEvent::ToolRequest {
                    id: t.tool_call_id,
                    name: t.tool_name,
                    params,
                }
            }
            session_response::Response::TurnComplete(c) => {
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
    // Simple key=value display
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
