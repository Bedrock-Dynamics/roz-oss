use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::WorldState;
use roz_core::tools::ToolCall;
use serde_json::Value;

use crate::safety::SafetyGuard;

/// Param names recognised as scalar velocity values.
const VELOCITY_PARAM_NAMES: &[&str] = &["velocity_ms", "vel", "speed", "velocity"];

/// Limits the velocity parameter of tool calls to a maximum value.
///
/// Checks multiple parameter names (`velocity_ms`, `vel`, `speed`, `velocity`)
/// and per-joint velocity arrays (`joint_velocities`).
/// Non-velocity tools (none of these fields present) always pass.
pub struct VelocityLimiter {
    max_velocity_ms: f64,
}

impl VelocityLimiter {
    pub const fn new(max_velocity_ms: f64) -> Self {
        Self { max_velocity_ms }
    }

    /// Returns the effective maximum velocity scaled by sensor confidence.
    ///
    /// Confidence is clamped to `[0.1, 1.0]` so the robot never fully stops
    /// due to low confidence alone (use degradation levels for that).
    #[must_use]
    pub fn max_velocity_for_confidence(&self, confidence: f64) -> f64 {
        self.max_velocity_ms * confidence.clamp(0.1, 1.0)
    }

    /// Find the first matching scalar velocity param name present in `params`.
    fn find_scalar_param(params: &Value) -> Option<(&'static str, f64)> {
        for &name in VELOCITY_PARAM_NAMES {
            if let Some(v) = params.get(name).and_then(Value::as_f64) {
                return Some((name, v));
            }
        }
        None
    }
}

#[async_trait]
impl SafetyGuard for VelocityLimiter {
    fn name(&self) -> &'static str {
        "velocity_limiter"
    }

    async fn check(&self, action: &ToolCall, _state: &WorldState) -> SafetyVerdict {
        let mut clamped = action.clone();

        // --- scalar velocity check (multiple param names) ---
        let scalar_modified = if let Some((param_name, velocity)) = Self::find_scalar_param(&action.params)
            && velocity > self.max_velocity_ms
        {
            clamped.params[param_name] = Value::from(self.max_velocity_ms);
            true
        } else {
            false
        };

        // --- per-joint velocity array check ---
        let joints_modified = if let Some(joints) = action.params.get("joint_velocities").and_then(Value::as_array) {
            let mut any_clamped = false;
            let clamped_joints: Vec<Value> = joints
                .iter()
                .map(|v| {
                    if let Some(vel) = v.as_f64()
                        && vel.abs() > self.max_velocity_ms
                    {
                        any_clamped = true;
                        return Value::from(vel.signum() * self.max_velocity_ms);
                    }
                    v.clone()
                })
                .collect();

            if any_clamped {
                clamped.params["joint_velocities"] = Value::from(clamped_joints);
            }
            any_clamped
        } else {
            false
        };

        if scalar_modified || joints_modified {
            SafetyVerdict::Modify {
                reason: format!("velocity exceeds limit of {:.1} m/s, clamped", self.max_velocity_ms),
                clamped,
            }
        } else {
            SafetyVerdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_state() -> WorldState {
        WorldState::default()
    }

    #[tokio::test]
    async fn allows_safe_velocity() {
        let guard = VelocityLimiter::new(5.0);
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"velocity_ms": 3.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn clamps_excessive_velocity() {
        let guard = VelocityLimiter::new(5.0);
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"velocity_ms": 15.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        match result {
            SafetyVerdict::Modify { clamped, reason } => {
                assert_eq!(clamped.params["velocity_ms"].as_f64().unwrap(), 5.0);
                assert!(reason.contains("velocity"));
            }
            other => panic!("expected Modify, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn allows_velocity_at_exact_limit() {
        let guard = VelocityLimiter::new(5.0);
        let action = ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"velocity_ms": 5.0}),
        };
        let result = guard.check(&action, &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[test]
    fn confidence_scales_max_velocity() {
        let guard = VelocityLimiter::new(10.0);

        // Full confidence: no scaling
        assert!((guard.max_velocity_for_confidence(1.0) - 10.0).abs() < f64::EPSILON);

        // Half confidence: half speed
        assert!((guard.max_velocity_for_confidence(0.5) - 5.0).abs() < f64::EPSILON);

        // Below minimum clamp (0.1): floors at 10%
        assert!((guard.max_velocity_for_confidence(0.0) - 1.0).abs() < f64::EPSILON);
        assert!((guard.max_velocity_for_confidence(-0.5) - 1.0).abs() < f64::EPSILON);

        // Above 1.0: clamped to 1.0
        assert!((guard.max_velocity_for_confidence(2.0) - 10.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn allows_non_velocity_tools() {
        let guard = VelocityLimiter::new(5.0);
        let action = ToolCall {
            id: String::new(),
            tool: "read_sensor".to_string(),
            params: json!({"sensor_id": "temp_1"}),
        };
        let result = guard.check(&action, &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn checks_joint_velocities_array() {
        let guard = VelocityLimiter::new(2.0);
        let action = ToolCall {
            id: String::new(),
            tool: "move_joints".to_string(),
            params: json!({"joint_velocities": [1.0, -3.0, 0.5, 5.0]}),
        };
        let result = guard.check(&action, &empty_state()).await;
        match result {
            SafetyVerdict::Modify { clamped, reason } => {
                let joints = clamped.params["joint_velocities"].as_array().unwrap();
                // 1.0 stays, -3.0 clamped to -2.0, 0.5 stays, 5.0 clamped to 2.0
                assert_eq!(joints[0].as_f64().unwrap(), 1.0);
                assert_eq!(joints[1].as_f64().unwrap(), -2.0);
                assert_eq!(joints[2].as_f64().unwrap(), 0.5);
                assert_eq!(joints[3].as_f64().unwrap(), 2.0);
                assert!(reason.contains("velocity"));
            }
            other => panic!("expected Modify, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn matches_alternate_param_names() {
        let guard = VelocityLimiter::new(5.0);

        for param_name in &["vel", "speed", "velocity"] {
            let action = ToolCall {
                id: String::new(),
                tool: "move".to_string(),
                params: json!({(*param_name): 10.0}),
            };
            let result = guard.check(&action, &empty_state()).await;
            match result {
                SafetyVerdict::Modify { clamped, reason } => {
                    assert_eq!(
                        clamped.params[param_name].as_f64().unwrap(),
                        5.0,
                        "param '{param_name}' should be clamped"
                    );
                    assert!(reason.contains("velocity"));
                }
                other => panic!("expected Modify for param '{param_name}', got {:?}", other),
            }
        }
    }
}
