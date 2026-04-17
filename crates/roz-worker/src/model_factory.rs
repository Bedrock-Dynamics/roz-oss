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
