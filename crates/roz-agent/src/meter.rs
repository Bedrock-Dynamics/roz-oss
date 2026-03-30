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
    HardLimited { plan: String, period_end: DateTime<Utc> },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
    use crate::dispatch::ToolDispatcher;
    use crate::model::types::*;
    use crate::safety::SafetyStack;
    use crate::spatial_provider::MockSpatialContextProvider;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Mock meter that records every `check_budget` and `record_usage` call.
    struct MockMeter {
        check_count: Arc<Mutex<u32>>,
        record_count: Arc<Mutex<u32>>,
        recorded_events: Arc<Mutex<Vec<UsageRecord>>>,
    }

    impl MockMeter {
        fn new() -> (Self, Arc<Mutex<u32>>, Arc<Mutex<u32>>, Arc<Mutex<Vec<UsageRecord>>>) {
            let check_count = Arc::new(Mutex::new(0));
            let record_count = Arc::new(Mutex::new(0));
            let recorded_events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    check_count: Arc::clone(&check_count),
                    record_count: Arc::clone(&record_count),
                    recorded_events: Arc::clone(&recorded_events),
                },
                check_count,
                record_count,
                recorded_events,
            )
        }
    }

    #[async_trait::async_trait]
    impl UsageMeter for MockMeter {
        async fn check_budget(&self, _tenant_id: &str) -> BudgetStatus {
            *self.check_count.lock().unwrap() += 1;
            BudgetStatus::WithinBudget
        }

        async fn record_usage(&self, record: UsageRecord) -> anyhow::Result<()> {
            *self.record_count.lock().unwrap() += 1;
            self.recorded_events.lock().unwrap().push(record);
            Ok(())
        }
    }

    fn simple_response(text: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::Text { text: text.to_string() }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 20,
                ..Default::default()
            },
        }
    }

    fn build_input(user_message: &str) -> AgentInput {
        AgentInput {
            task_id: "test-meter-1".to_string(),
            tenant_id: "tenant-abc".to_string(),
            model_name: "mock-model".to_string(),
            system_prompt: vec!["You are a test agent.".to_string()],
            user_message: user_message.to_string(),
            max_cycles: 10,
            max_tokens: 4096,
            max_context_tokens: 200_000,
            mode: AgentLoopMode::React,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: false,
            history: vec![],
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        }
    }

    #[tokio::test]
    async fn agent_loop_calls_meter() {
        let (mock_meter, check_count, record_count, recorded_events) = MockMeter::new();

        let model = Box::new(MockModel::new(
            vec![ModelCapability::TextReasoning],
            vec![simple_response("Hello!")],
        ));
        let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
        let safety = SafetyStack::new(vec![]);
        let spatial = Box::new(MockSpatialContextProvider::empty());

        let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_meter(Arc::new(mock_meter));

        let input = build_input("Say hello");
        let output = agent.run(input).await.expect("agent loop should complete");

        // Sanity: the loop ran one cycle and produced a response.
        assert_eq!(output.cycles, 1);
        assert_eq!(output.final_response.as_deref(), Some("Hello!"));

        // Budget was checked at least once (before the LLM call).
        let checks = *check_count.lock().unwrap();
        assert!(checks >= 1, "check_budget should be called at least once, got {checks}");

        // Usage was recorded at least once (after the LLM response).
        let records = *record_count.lock().unwrap();
        assert!(
            records >= 1,
            "record_usage should be called at least once, got {records}"
        );

        // Verify the recorded UsageRecord has the expected shape.
        let events = recorded_events.lock().unwrap();
        let first = &events[0];
        assert_eq!(first.resource_type, "ai_tokens");
        assert_eq!(first.tenant_id, "tenant-abc");
        assert_eq!(first.model.as_deref(), Some("mock-model"));
        assert_eq!(first.input_tokens, Some(50));
        assert_eq!(first.output_tokens, Some(20));
        assert!(first.quantity > 0, "quantity should be positive (sum of tokens)");
    }
}
