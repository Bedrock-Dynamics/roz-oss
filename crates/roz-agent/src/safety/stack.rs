use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::SpatialContext;
use roz_core::tools::ToolCall;

/// A safety guard that checks a tool call against spatial context.
#[async_trait]
pub trait SafetyGuard: Send + Sync {
    /// Human-readable name for this guard.
    fn name(&self) -> &'static str;

    /// Evaluate a tool call in the given spatial context.
    async fn check(&self, action: &ToolCall, state: &SpatialContext) -> SafetyVerdict;
}

/// The result of running a tool call through the full safety stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyResult {
    /// The action passed all guards (possibly modified along the way).
    Approved(ToolCall),
    /// A guard blocked the action.
    Blocked { guard: String, reason: String },
    /// A guard requires human confirmation before proceeding.
    NeedsHuman { reason: String, timeout_secs: u64 },
}

/// An ordered stack of safety guards. Guards are evaluated in order;
/// the first Block or `RequireConfirmation` short-circuits evaluation.
pub struct SafetyStack {
    guards: Vec<Box<dyn SafetyGuard>>,
}

impl SafetyStack {
    pub fn new(guards: Vec<Box<dyn SafetyGuard>>) -> Self {
        Self { guards }
    }

    /// Run the action through every guard in order.
    /// A `Block` or `RequireConfirmation` verdict short-circuits evaluation.
    /// A `Modify` verdict replaces the action for subsequent guards.
    pub async fn evaluate(&self, action: &ToolCall, state: &SpatialContext) -> SafetyResult {
        let mut current = action.clone();
        for guard in &self.guards {
            match guard.check(&current, state).await {
                SafetyVerdict::Allow => {}
                SafetyVerdict::Modify { clamped, .. } => {
                    current = clamped;
                }
                SafetyVerdict::Block { reason } => {
                    return SafetyResult::Blocked {
                        guard: guard.name().to_string(),
                        reason,
                    };
                }
                SafetyVerdict::RequireConfirmation { reason, timeout_secs } => {
                    return SafetyResult::NeedsHuman { reason, timeout_secs };
                }
            }
        }
        SafetyResult::Approved(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Mock guards
    // -----------------------------------------------------------------------

    struct AlwaysAllow;

    #[async_trait]
    impl SafetyGuard for AlwaysAllow {
        fn name(&self) -> &'static str {
            "always_allow"
        }
        async fn check(&self, _action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
            SafetyVerdict::Allow
        }
    }

    struct AlwaysBlock {
        reason: String,
    }

    #[async_trait]
    impl SafetyGuard for AlwaysBlock {
        fn name(&self) -> &'static str {
            "always_block"
        }
        async fn check(&self, _action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
            SafetyVerdict::Block {
                reason: self.reason.clone(),
            }
        }
    }

    /// A guard that doubles the velocity_ms param (to prove modification propagates).
    struct DoubleVelocity;

    #[async_trait]
    impl SafetyGuard for DoubleVelocity {
        fn name(&self) -> &'static str {
            "double_velocity"
        }
        async fn check(&self, action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
            let mut clamped = action.clone();
            if let Some(v) = clamped.params.get("velocity_ms").and_then(|v| v.as_f64()) {
                clamped.params["velocity_ms"] = json!(v * 2.0);
            }
            SafetyVerdict::Modify {
                clamped,
                reason: "doubled velocity".to_string(),
            }
        }
    }

    /// A guard that records what velocity_ms value it saw (for verifying modification propagation).
    struct RecordVelocity {
        seen: std::sync::Mutex<Option<f64>>,
    }

    impl RecordVelocity {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(None),
            }
        }
        fn seen_value(&self) -> Option<f64> {
            *self.seen.lock().unwrap()
        }
    }

    #[async_trait]
    impl SafetyGuard for RecordVelocity {
        fn name(&self) -> &'static str {
            "record_velocity"
        }
        async fn check(&self, action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
            if let Some(v) = action.params.get("velocity_ms").and_then(|v| v.as_f64()) {
                *self.seen.lock().unwrap() = Some(v);
            }
            SafetyVerdict::Allow
        }
    }

    struct RequireConfirm;

    #[async_trait]
    impl SafetyGuard for RequireConfirm {
        fn name(&self) -> &'static str {
            "require_confirm"
        }
        async fn check(&self, _action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
            SafetyVerdict::RequireConfirmation {
                reason: "needs human approval".to_string(),
                timeout_secs: 30,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_action() -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"velocity_ms": 3.0}),
        }
    }

    fn empty_state() -> SpatialContext {
        SpatialContext::default()
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn empty_stack_allows_everything() {
        let stack = SafetyStack::new(vec![]);
        let action = make_action();
        let result = stack.evaluate(&action, &empty_state()).await;
        assert_eq!(result, SafetyResult::Approved(action));
    }

    #[tokio::test]
    async fn single_allow_guard_returns_approved() {
        let stack = SafetyStack::new(vec![Box::new(AlwaysAllow)]);
        let action = make_action();
        let result = stack.evaluate(&action, &empty_state()).await;
        assert_eq!(result, SafetyResult::Approved(action));
    }

    #[tokio::test]
    async fn single_block_guard_returns_blocked() {
        let stack = SafetyStack::new(vec![Box::new(AlwaysBlock {
            reason: "too dangerous".to_string(),
        })]);
        let action = make_action();
        let result = stack.evaluate(&action, &empty_state()).await;
        assert_eq!(
            result,
            SafetyResult::Blocked {
                guard: "always_block".to_string(),
                reason: "too dangerous".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn block_short_circuits_before_later_guards() {
        // AlwaysBlock should prevent RecordVelocity from ever running
        let recorder = std::sync::Arc::new(RecordVelocity::new());

        // We can't use Arc directly with Box<dyn SafetyGuard>, so use a wrapper
        struct ArcRecorder(std::sync::Arc<RecordVelocity>);

        #[async_trait]
        impl SafetyGuard for ArcRecorder {
            fn name(&self) -> &'static str {
                self.0.name()
            }
            async fn check(&self, action: &ToolCall, state: &SpatialContext) -> SafetyVerdict {
                self.0.check(action, state).await
            }
        }

        let stack = SafetyStack::new(vec![
            Box::new(AlwaysBlock {
                reason: "blocked".to_string(),
            }),
            Box::new(ArcRecorder(recorder.clone())),
        ]);

        let action = make_action();
        let result = stack.evaluate(&action, &empty_state()).await;

        // Should be blocked
        assert!(matches!(result, SafetyResult::Blocked { .. }));
        // RecordVelocity should never have been called
        assert_eq!(recorder.seen_value(), None);
    }

    #[tokio::test]
    async fn modify_guard_changes_action_for_later_guards() {
        let recorder = std::sync::Arc::new(RecordVelocity::new());

        struct ArcRecorder(std::sync::Arc<RecordVelocity>);

        #[async_trait]
        impl SafetyGuard for ArcRecorder {
            fn name(&self) -> &'static str {
                self.0.name()
            }
            async fn check(&self, action: &ToolCall, state: &SpatialContext) -> SafetyVerdict {
                self.0.check(action, state).await
            }
        }

        let stack = SafetyStack::new(vec![
            Box::new(DoubleVelocity), // 3.0 -> 6.0
            Box::new(ArcRecorder(recorder.clone())),
        ]);

        let action = make_action(); // velocity_ms: 3.0
        let result = stack.evaluate(&action, &empty_state()).await;

        // The final result should have doubled velocity
        match &result {
            SafetyResult::Approved(tc) => {
                assert_eq!(tc.params["velocity_ms"].as_f64().unwrap(), 6.0);
            }
            _ => panic!("expected Approved, got {:?}", result),
        }

        // The recorder should have seen the doubled velocity
        assert_eq!(recorder.seen_value(), Some(6.0));
    }

    #[tokio::test]
    async fn require_confirmation_returns_needs_human() {
        let stack = SafetyStack::new(vec![Box::new(RequireConfirm)]);
        let action = make_action();
        let result = stack.evaluate(&action, &empty_state()).await;
        assert_eq!(
            result,
            SafetyResult::NeedsHuman {
                reason: "needs human approval".to_string(),
                timeout_secs: 30,
            }
        );
    }
}
