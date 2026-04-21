use axum::extract::FromRef;

use crate::auth::RestAuth;
use crate::config::SignedDispatchEnforcement;
use crate::middleware::rate_limit::KeyedRateLimiter;
use ed25519_dalek::VerifyingKey;
use moka::future::Cache;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

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
    /// PAIG proxy provider name for Gemini models (D-10).
    ///
    /// Set via `ROZ_GEMINI_PROVIDER` (default: `"google-vertex"` per D-10 — matches the
    /// verified PAIG path at `/proxy/google-vertex/v1beta1/...` in
    /// `crates/roz-agent/src/model/gemini.rs`).
    pub gemini_provider: String,
    /// Direct Gemini API key. When set, `MediaBackend`s bypass the PAIG gateway
    /// (D-11 degradation path).
    ///
    /// Set via `ROZ_GEMINI_API_KEY`.
    pub gemini_direct_api_key: Option<String>,
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
    /// Pluggable REST auth provider. OSS uses `ApiKeyAuth`, cloud injects Clerk JWT support.
    pub auth: Arc<dyn RestAuth>,
    /// Pluggable usage metering. OSS uses `NoOpMeter`, cloud injects billing logic.
    pub meter: Arc<dyn roz_agent::meter::UsageMeter>,
    /// Device trust policy loaded at startup from `ROZ_TRUST_*` env vars.
    ///
    /// Shared immutable reference — cloned per handler via `Arc`. Enforced by
    /// `crate::trust::check_host_trust` in BOTH the REST `routes::tasks::create`
    /// and gRPC `grpc::tasks::create_task` paths BEFORE Restate workflow
    /// creation / NATS publish. Fail-closed: see `trust::load_trust_policy_from_env`
    /// for defaults.
    pub trust_policy: Arc<roz_core::device_trust::evaluator::TrustPolicy>,
    /// Phase 18 SKILL-01: pluggable object store for skill bundled assets.
    ///
    /// Constructed once at boot from `LocalFileSystem::new_with_prefix(skill_store_root)`
    /// (`crates/roz-server/src/main.rs`) and injected into `SkillsServiceImpl` plus
    /// the `Arc<dyn ObjectStore>` extension on every `AgentLoop` `ToolContext`.
    /// Cloud backends (S3/GCS/Azure) live behind Cargo features per CONTEXT D-01.
    pub object_store: Arc<dyn object_store::ObjectStore>,
    /// Phase 19 Plan 11: registry of OpenAI-compat endpoints for `create_model`.
    ///
    /// Concrete struct (NOT a trait object) per 19-CONTEXT §Area 1 — cloud will
    /// replace or wrap this struct when it needs async DB-backed per-tenant
    /// resolution. OSS loads from `ROZ_ENDPOINTS_CONFIG` at boot, or defaults
    /// to [`roz_core::EndpointRegistry::empty()`] when unset.
    pub endpoint_registry: Arc<roz_core::EndpointRegistry>,
    /// Phase 19 Plan 11: AES-256-GCM key provider for at-rest credential
    /// encryption/decryption. OSS defaults to
    /// [`roz_core::key_provider::StaticKeyProvider`] backed by `ROZ_ENCRYPTION_KEY`,
    /// falling back to [`roz_openai::auth::null_key::NullKeyProvider`] when the
    /// env var is unset (endpoints with `auth_mode='api_key'` are rejected at
    /// bootstrap in that case).
    pub key_provider: Arc<dyn roz_core::key_provider::KeyProvider>,
    /// Phase 20 Plan 05: shared in-process MCP registry used by the control
    /// plane and later session-start tool exposure.
    pub mcp_registry: Arc<roz_mcp::Registry>,
    /// Phase 20 Plan 07: cross-RPC session coordinator for MCP OAuth
    /// approval-style flows.
    pub session_bus: Arc<crate::grpc::session_bus::SessionBus>,
    /// Phase 23 Plan 23-04 (D-11): LRU cache for per-device verifying keys
    /// used during inbound-signature verification. Keyed by
    /// `(tenant_id, host_id, key_version)`; value is the 32-byte Ed25519
    /// verifying key material held as [`VerifyingKey`]. Capacity capped at
    /// 10 000 entries (~400 KB resident — mitigates T-23-15 memory-exhaustion
    /// DoS) and TTL 60 s so that rotation / revocation visibility lag never
    /// exceeds one minute for passive expiry. Revocation clears affected
    /// entries synchronously (implemented in 23-06).
    pub verifying_key_cache: Cache<(Uuid, Uuid, u32), VerifyingKey>,
    /// Phase 23 Plan 23-04 (D-12): rollout gate for two-direction signed
    /// dispatch. Read from `SIGNED_DISPATCH_ENFORCEMENT` at boot; defaults
    /// to [`SignedDispatchEnforcement::Audit`] in development and
    /// [`SignedDispatchEnforcement::Strict`] everywhere else per the
    /// Phase 23 Planner Discretion Q6.
    pub signed_dispatch_enforcement: SignedDispatchEnforcement,
    /// Phase 26 OBS-01: registry of per-session `WriterActor` command senders.
    ///
    /// Keyed by `session_id`; the value is an
    /// [`tokio::sync::mpsc::Sender<WriteCommand>`] owned by the per-session
    /// MCAP writer. The SIGTERM drain (Plan 26-07) iterates this map and
    /// sends `Finalize { Shutdown }` so every open archive transitions to
    /// `finalized` + is flushed/closed cleanly on shutdown. Shape mirrors
    /// the existing `TelemetryDedup` (`Arc<Mutex<HashMap>>`) rather than
    /// introducing a new `DashMap` workspace dep; contention on 10–100
    /// active-session entries is negligible and never on the hot write path.
    pub active_writers: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                Uuid,
                tokio::sync::mpsc::Sender<crate::observability::mcap_archive::WriteCommand>,
            >,
        >,
    >,
    /// Phase 26 OBS-01: broadcast sink for `TaskLifecycleEvent`.
    ///
    /// Populated by the 3 UPDATE sites in `crates/roz-db/src/tasks.rs`
    /// (Plan 26-08); each per-session `WriterActor` subscribes once at
    /// session start and emits `WriteCommand::Event { channel:
    /// TaskLifecycle }` for every transition. Bounded (1024) — catastrophic
    /// backlog implies the archive is already compromised.
    pub task_lifecycle_sink: crate::observability::task_lifecycle::TaskLifecycleSink,
    /// Phase 26 OBS-02: pre-loaded Foxglove + `roz.v1` schema descriptor bytes.
    ///
    /// Decoded once at server boot from the `FileDescriptorSet` output of
    /// `build.rs` (roz-server's Foxglove + roz.v1 descriptor bins); cloned
    /// by-value into each per-session `WriterActor::open` so
    /// `mcap::Writer::add_schema` has the subset FDS bytes for all six
    /// registered channels.
    pub schema_descriptors: crate::observability::schema_registry::SchemaDescriptors,
    /// Phase 26 D-01: base directory for MCAP archives (`ROZ_MCAP_DIR`).
    ///
    /// Absolute, canonicalized path enforced as the root for the
    /// path-traversal defence-in-depth check in `WriterActor::open`
    /// (T-26-40). Created at boot if missing.
    pub mcap_dir: std::path::PathBuf,
}

impl FromRef<AppState> for PgPool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}
