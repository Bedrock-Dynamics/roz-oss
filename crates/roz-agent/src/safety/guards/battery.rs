use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::WorldState;
use roz_core::tools::ToolCall;

use crate::safety::SafetyGuard;

/// Checks battery level from spatial context entity properties.
///
/// - Above `warning_pct` -> Allow
/// - Between `critical_pct` and `warning_pct` -> Modify (force return-to-launch)
/// - Below `critical_pct` -> Block
/// - No battery data in context -> Allow (assume non-battery system)
pub struct BatteryGuard {
    warning_pct: f64,
    critical_pct: f64,
}

impl BatteryGuard {
    pub const fn new(warning_pct: f64, critical_pct: f64) -> Self {
        Self {
            warning_pct,
            critical_pct,
        }
    }
}

#[async_trait]
impl SafetyGuard for BatteryGuard {
    fn name(&self) -> &'static str {
        "battery"
    }

    async fn check(&self, _action: &ToolCall, state: &WorldState) -> SafetyVerdict {
        // Find battery_pct in any entity's properties
        let battery_pct = state
            .entities
            .iter()
            .find_map(|e| e.properties.get("battery_pct").and_then(serde_json::Value::as_f64));

        let Some(pct) = battery_pct else {
            return SafetyVerdict::Allow;
        };

        if pct < self.critical_pct {
            SafetyVerdict::Block {
                reason: format!(
                    "battery critically low at {pct:.0}% (threshold: {:.0}%)",
                    self.critical_pct
                ),
            }
        } else if pct < self.warning_pct {
            SafetyVerdict::Modify {
                clamped: ToolCall {
                    id: String::new(),
                    tool: "return_to_launch".to_string(),
                    params: serde_json::json!({}),
                },
                reason: format!(
                    "battery low at {pct:.0}% (warning threshold: {:.0}%), forcing return to launch",
                    self.warning_pct
                ),
            }
        } else {
            SafetyVerdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::spatial::EntityState;
    use serde_json::json;
    use std::collections::HashMap;

    fn context_with_battery(pct: f64) -> WorldState {
        let mut properties = HashMap::new();
        properties.insert("battery_pct".to_string(), json!(pct));
        WorldState {
            entities: vec![EntityState {
                id: "drone_1".to_string(),
                kind: "drone".to_string(),
                position: Some([0.0, 0.0, 10.0]),
                orientation: None,
                velocity: None,
                properties,
                timestamp_ns: None,
                frame_id: "world".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn make_action() -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 5.0, "y": 5.0}),
        }
    }

    #[tokio::test]
    async fn full_battery_allows() {
        let guard = BatteryGuard::new(30.0, 15.0);
        let result = guard.check(&make_action(), &context_with_battery(80.0)).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn warning_threshold_forces_rtl() {
        let guard = BatteryGuard::new(30.0, 15.0);
        let result = guard.check(&make_action(), &context_with_battery(25.0)).await;
        match result {
            SafetyVerdict::Modify { clamped, reason } => {
                // Should force RTL (return_to_launch tool)
                assert_eq!(clamped.tool, "return_to_launch");
                assert!(reason.contains("battery") || reason.contains("low"));
            }
            other => panic!("expected Modify, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn critical_threshold_blocks() {
        let guard = BatteryGuard::new(30.0, 15.0);
        let result = guard.check(&make_action(), &context_with_battery(10.0)).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("battery") || reason.contains("critical"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn no_battery_data_allows() {
        let guard = BatteryGuard::new(30.0, 15.0);
        let ctx = WorldState::default();
        let result = guard.check(&make_action(), &ctx).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn battery_exactly_at_warning_allows() {
        let guard = BatteryGuard::new(30.0, 15.0);
        // At exactly the warning threshold, should still allow (not below)
        let result = guard.check(&make_action(), &context_with_battery(30.0)).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn battery_exactly_at_critical_triggers_warning() {
        let guard = BatteryGuard::new(30.0, 15.0);
        // At exactly critical threshold, pct < warning is true but pct < critical is false,
        // so it falls into the warning range (Modify), not block.
        let result = guard.check(&make_action(), &context_with_battery(15.0)).await;
        match result {
            SafetyVerdict::Modify { .. } => {} // warning-level, forces RTL
            other => panic!("expected Modify at exact critical boundary, got {:?}", other),
        }
    }
}
