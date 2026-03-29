use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Budget check result. Determines whether a tenant can proceed.
#[derive(Debug, Clone)]
pub enum BudgetStatus {
    /// Within included plan allocation.
    WithinBudget,
    /// Exceeded included allocation, but overage is allowed (paid/team plans).
    InOverage { plan: String },
    /// Hard-limited, cannot proceed (free plan, no payment method).
    HardLimited {
        plan: String,
        period_end: DateTime<Utc>,
    },
}

impl BudgetStatus {
    /// Returns `true` if the tenant is hard-limited and must stop.
    pub const fn is_hard_limited(&self) -> bool {
        matches!(self, Self::HardLimited { .. })
    }
}

/// Record of a single billable action. Created after each LLM call or resource use.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    /// String form of tenant UUID (matches `AgentInput.tenant_id`).
    pub tenant_id: String,
    pub session_id: Uuid,
    /// Resource type: `"ai_tokens"`, `"sim_time"`, `"storage"`, `"ci_run"`.
    pub resource_type: String,
    pub model: Option<String>,
    /// Resource-specific quantity: tokens, seconds, bytes, count.
    pub quantity: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    /// Unique key preventing double-counting: `"{task_id}:{cycle}"`.
    /// (`task_id` = `session_id` in gRPC handler context.)
    pub idempotency_key: String,
}

/// Trait for usage metering and budget enforcement.
///
/// roz-cloud implements this with real billing logic.
/// Default: [`NoOpMeter`] (unlimited usage for self-hosted).
///
/// Same pattern as `RestAuth` / `GrpcAuth`.
///
/// NOTE: `check_budget` takes `&str` (not `TenantId`) because `AgentInput.tenant_id`
/// is a `String` in the agent loop. The cloud implementation parses to UUID internally.
#[async_trait::async_trait]
pub trait UsageMeter: Send + Sync + 'static {
    /// Check if the tenant has remaining budget. Called before each LLM turn.
    /// `tenant_id` is the string form of the tenant UUID.
    async fn check_budget(&self, tenant_id: &str) -> BudgetStatus;

    /// Record usage after a billable action. Called after each LLM call.
    async fn record_usage(&self, record: UsageRecord) -> anyhow::Result<()>;
}

/// No-op meter for self-hosted / OSS deployments. All usage is unlimited.
pub struct NoOpMeter;

#[async_trait::async_trait]
impl UsageMeter for NoOpMeter {
    async fn check_budget(&self, _tenant_id: &str) -> BudgetStatus {
        BudgetStatus::WithinBudget
    }

    async fn record_usage(&self, _record: UsageRecord) -> anyhow::Result<()> {
        Ok(())
    }
}
