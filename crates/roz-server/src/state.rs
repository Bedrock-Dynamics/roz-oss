use crate::middleware::rate_limit::KeyedRateLimiter;
use sqlx::PgPool;
use std::sync::Arc;

/// Model provider configuration loaded from environment variables.
#[derive(Clone)]
#[allow(dead_code)]
pub struct ModelConfig {
    pub gateway_url: String,
    pub api_key: String,
    pub default_model: String,
    pub timeout_secs: u64,
    /// PAIG proxy provider name for Anthropic/Claude models.
    ///
    /// Set via `ROZ_ANTHROPIC_PROVIDER` (default: `"anthropic"`).
    /// Override to use a custom BYOK provider (e.g. `"claude-roz"`).
    pub anthropic_provider: String,
    /// Direct Anthropic API key (`sk-ant-...`). When set, bypasses the PAIG gateway.
    ///
    /// Set via `ROZ_ANTHROPIC_API_KEY`.
    pub direct_api_key: Option<String>,
}

/// Shared application state threaded through every axum handler.
#[derive(Clone)]
#[allow(dead_code)]
pub struct AppState {
    pub pool: PgPool,
    pub rate_limiter: Arc<KeyedRateLimiter>,
    /// Public base URL for constructing verification URIs (e.g., `http://localhost:8080`).
    pub base_url: String,
    /// Restate ingress URL for starting workflows and sending signals.
    pub restate_ingress_url: String,
    /// Shared HTTP client for outbound requests (e.g., Restate ingress).
    pub http_client: reqwest::Client,
    /// NATS operator seed for signing account JWTs. `None` disables NATS provisioning.
    pub operator_seed: Option<String>,
    /// NATS client for publishing task invocations. `None` when NATS is unavailable (tests, dev).
    pub nats_client: Option<async_nats::Client>,
    /// Model provider config for creating LLM model instances in gRPC sessions.
    pub model_config: ModelConfig,
}
