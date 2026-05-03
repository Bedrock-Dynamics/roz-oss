//! Shared model construction — used by both task execution (`main.rs`) and
//! edge session relay (`session_relay.rs`).
//!
//! Eliminates duplicated `create_model` + `FallbackModel` wiring.
//!
//! # Phase 19 Plan 11 limitation
//!
//! The worker currently passes `EndpointRegistry::empty()` and a nil tenant id
//! to `create_model`. Worker paths today only use `claude-*` / `gemini-*`
//! prefixes, so the registry is unused on the happy path. If a worker is later
//! asked to run an `openai-compat:<name>` model, resolution will fail with
//! `AgentError::UnknownEndpoint` until the worker is taught to receive the
//! endpoint registry from the server (T-19-11-03 accepted risk).

use std::sync::Arc;

use roz_core::auth::TenantId;
use roz_core::model_endpoint::EndpointRegistry;
use uuid::Uuid;

use crate::config::WorkerConfig;

/// Build the agent model from worker config, wrapping in `FallbackModel` when configured.
pub fn build_model(
    config: &WorkerConfig,
    model_name_override: Option<&str>,
) -> anyhow::Result<Box<dyn roz_agent::model::Model>> {
    let primary_model_name = model_name_override
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&config.model_name);

    #[cfg(feature = "test-fixtures")]
    if primary_model_name == "test-flight-command" {
        return Ok(Box::new(ScriptedFlightCommandModel::new()));
    }

    // Worker-side OSS fallback: empty registry + nil tenant. See module docs.
    let registry: Arc<EndpointRegistry> = Arc::new(EndpointRegistry::empty());
    let tenant_id = TenantId::new(Uuid::nil());
    let primary = roz_agent::model::create_model(
        primary_model_name,
        &config.gateway_url,
        &config.gateway_api_key,
        config.model_timeout_secs,
        &config.anthropic_provider,
        config.anthropic_api_key.as_deref(),
        &tenant_id,
        registry.clone(),
    )?;

    if let Some(ref fallback_name) = config.fallback_model {
        match roz_agent::model::create_model(
            fallback_name,
            &config.gateway_url,
            &config.gateway_api_key,
            config.model_timeout_secs,
            &config.anthropic_provider,
            config.anthropic_api_key.as_deref(),
            &tenant_id,
            registry,
        ) {
            Ok(fallback) => {
                tracing::info!(fallback_model = %fallback_name, "model fallback configured");
                Ok(Box::new(roz_agent::model::FallbackModel::new(primary, fallback)))
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to create fallback model, proceeding without");
                Ok(primary)
            }
        }
    } else {
        Ok(primary)
    }
}

#[cfg(feature = "test-fixtures")]
struct ScriptedFlightCommandModel {
    call_count: std::sync::atomic::AtomicU32,
}

#[cfg(feature = "test-fixtures")]
impl ScriptedFlightCommandModel {
    const fn new() -> Self {
        Self {
            call_count: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn latest_user_text(req: &roz_agent::model::CompletionRequest) -> String {
        req.messages
            .iter()
            .rev()
            .find_map(|message| {
                if !matches!(message.role, roz_agent::model::MessageRole::User) {
                    return None;
                }
                message.text()
            })
            .unwrap_or_default()
    }

    fn command_input(req: &roz_agent::model::CompletionRequest) -> serde_json::Value {
        let text = Self::latest_user_text(req);
        if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
            && start <= end
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text[start..=end])
        {
            return value;
        }

        let lower = text.to_ascii_lowercase();
        let command = [
            "takeoff",
            "land",
            "rtl",
            "return_to_launch",
            "arm",
            "disarm",
            "set_mode",
            "goto",
        ]
        .into_iter()
        .find(|candidate| lower.contains(candidate))
        .unwrap_or("arm");
        let mut input = serde_json::json!({ "command": command });
        if command == "takeoff" {
            input["altitude_m"] = serde_json::json!(5.0);
        }
        input
    }

    fn latest_flight_command_result(req: &roz_agent::model::CompletionRequest) -> Option<String> {
        req.messages.iter().rev().find_map(|message| {
            message.parts.iter().rev().find_map(|part| match part {
                roz_agent::model::ContentPart::ToolResult { name, content, .. } if name == "flight_command" => {
                    Some(content.clone())
                }
                _ => None,
            })
        })
    }

    const fn usage() -> roz_agent::model::TokenUsage {
        roz_agent::model::TokenUsage {
            input_tokens: 4,
            output_tokens: 4,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }
}

#[cfg(feature = "test-fixtures")]
#[async_trait::async_trait]
impl roz_agent::model::Model for ScriptedFlightCommandModel {
    fn capabilities(&self) -> Vec<roz_agent::model::ModelCapability> {
        vec![roz_agent::model::ModelCapability::TextReasoning]
    }

    async fn complete(
        &self,
        req: &roz_agent::model::CompletionRequest,
    ) -> Result<roz_agent::model::CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        let call = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 0 {
            Ok(roz_agent::model::CompletionResponse {
                parts: vec![roz_agent::model::ContentPart::ToolUse {
                    id: "toolu_flight_command_1".to_string(),
                    name: "flight_command".to_string(),
                    input: Self::command_input(req),
                }],
                stop_reason: roz_agent::model::StopReason::ToolUse,
                usage: Self::usage(),
            })
        } else {
            let text = Self::latest_flight_command_result(req)
                .map(|result| format!("flight command result: {result}"))
                .unwrap_or_else(|| "flight command dispatched".to_string());
            Ok(roz_agent::model::CompletionResponse {
                parts: vec![roz_agent::model::ContentPart::Text { text }],
                stop_reason: roz_agent::model::StopReason::EndTurn,
                usage: Self::usage(),
            })
        }
    }
}
