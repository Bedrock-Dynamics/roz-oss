//! Shared model construction — used by both task execution (`main.rs`) and
//! edge session relay (`session_relay.rs`).
//!
//! Eliminates duplicated `create_model` + `FallbackModel` wiring.

use crate::config::WorkerConfig;

/// Build the agent model from worker config, wrapping in `FallbackModel` when configured.
pub fn build_model(config: &WorkerConfig) -> anyhow::Result<Box<dyn roz_agent::model::Model>> {
    let primary = roz_agent::model::create_model(
        &config.model_name,
        &config.gateway_url,
        &config.gateway_api_key,
        config.model_timeout_secs,
        &config.anthropic_provider,
        config.anthropic_api_key.as_deref(),
    )?;

    if let Some(ref fallback_name) = config.fallback_model {
        match roz_agent::model::create_model(
            fallback_name,
            &config.gateway_url,
            &config.gateway_api_key,
            config.model_timeout_secs,
            &config.anthropic_provider,
            config.anthropic_api_key.as_deref(),
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
