use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// SafetyLevel
// ---------------------------------------------------------------------------

/// Escalating safety severity levels for robotic operations.
/// Ordered from least to most severe; supports `Ord` for comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyLevel {
    Normal = 0,
    Warning = 1,
    ReducedMode = 2,
    ProtectiveStop = 3,
    EmergencyStop = 4,
}

impl SafetyLevel {
    /// Returns the more severe of the two safety levels.
    #[must_use]
    pub fn escalate(self, other: Self) -> Self {
        std::cmp::max(self, other)
    }
}

// ---------------------------------------------------------------------------
// SafetyVerdict
// ---------------------------------------------------------------------------

/// The outcome of a safety check on a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SafetyVerdict {
    Allow,
    Modify {
        clamped: crate::tools::ToolCall,
        reason: String,
    },
    Block {
        reason: String,
    },
    RequireConfirmation {
        reason: String,
        timeout_secs: u64,
    },
}

// ---------------------------------------------------------------------------
// OddConstraint (Operational Design Domain constraint)
// ---------------------------------------------------------------------------

/// A single constraint within an Operational Design Domain (ODD).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OddConstraint {
    Range { min: f64, max: f64 },
    Threshold { max: f64 },
    Boolean { required: bool },
}

impl OddConstraint {
    /// Check a numeric value against this constraint.
    pub fn check(&self, value: f64) -> bool {
        match self {
            Self::Range { min, max } => value >= *min && value <= *max,
            Self::Threshold { max } => value <= *max,
            Self::Boolean { .. } => true,
        }
    }

    /// Check a boolean value against this constraint.
    pub const fn check_bool(&self, value: bool) -> bool {
        match self {
            Self::Boolean { required } => value == *required,
            _ => true,
        }
    }
}

// ---------------------------------------------------------------------------
// OperationalDesignDomain
// ---------------------------------------------------------------------------

/// Defines the environmental and operational boundaries within which
/// the system is designed to operate safely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationalDesignDomain {
    pub wind_speed_max_ms: Option<f64>,
    pub temperature_range_c: Option<(f64, f64)>,
    pub visibility_min_m: Option<f64>,
    pub gps_hdop_max: Option<f64>,
    pub battery_min_pct: Option<f64>,
    pub comms_latency_max_ms: Option<u64>,
    pub custom: HashMap<String, OddConstraint>,
}

// ---------------------------------------------------------------------------
// ControlMode
// ---------------------------------------------------------------------------

/// Control mode for robot sessions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    /// Agent executes freely within safety bounds.
    #[default]
    Autonomous,
    /// Agent executes, user monitors, can pause/stop. Default for remote sessions.
    Supervised,
    /// Agent suggests each step, user approves before execution.
    Collaborative,
    /// Direct teleop, no agent.
    Manual,
}

impl ControlMode {
    /// Default mode for remote sessions (`host_id` is set).
    pub const fn for_remote() -> Self {
        Self::Supervised
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // SafetyLevel ordering
    // -----------------------------------------------------------------------

    #[test]
    fn safety_level_ordering() {
        assert!(SafetyLevel::Normal < SafetyLevel::Warning);
        assert!(SafetyLevel::Warning < SafetyLevel::ReducedMode);
        assert!(SafetyLevel::ReducedMode < SafetyLevel::ProtectiveStop);
        assert!(SafetyLevel::ProtectiveStop < SafetyLevel::EmergencyStop);
    }

    // -----------------------------------------------------------------------
    // escalate returns max of two levels
    // -----------------------------------------------------------------------

    #[test]
    fn escalate_returns_max_of_two_levels() {
        assert_eq!(SafetyLevel::Normal.escalate(SafetyLevel::Warning), SafetyLevel::Warning);
        assert_eq!(
            SafetyLevel::EmergencyStop.escalate(SafetyLevel::Normal),
            SafetyLevel::EmergencyStop
        );
        assert_eq!(
            SafetyLevel::ReducedMode.escalate(SafetyLevel::ReducedMode),
            SafetyLevel::ReducedMode
        );
        assert_eq!(
            SafetyLevel::Warning.escalate(SafetyLevel::ProtectiveStop),
            SafetyLevel::ProtectiveStop
        );
    }

    // -----------------------------------------------------------------------
    // SafetyVerdict::Block serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn safety_verdict_block_serde_roundtrip() {
        let verdict = SafetyVerdict::Block {
            reason: "joint limit exceeded".to_string(),
        };
        let serialized = serde_json::to_string(&verdict).unwrap();
        let deserialized: SafetyVerdict = serde_json::from_str(&serialized).unwrap();
        assert_eq!(verdict, deserialized);

        // Verify the JSON shape uses snake_case tag
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["type"], "block");
    }

    // -----------------------------------------------------------------------
    // SafetyVerdict::Modify variant holds clamped ToolCall
    // -----------------------------------------------------------------------

    #[test]
    fn safety_verdict_modify_holds_clamped_tool_call() {
        let clamped = crate::tools::ToolCall {
            id: String::new(),
            tool: "move_arm".to_string(),
            params: json!({"x": 1.0, "y": 0.5}),
        };
        let verdict = SafetyVerdict::Modify {
            clamped: clamped.clone(),
            reason: "velocity clamped to safe range".to_string(),
        };

        let serialized = serde_json::to_string(&verdict).unwrap();
        let deserialized: SafetyVerdict = serde_json::from_str(&serialized).unwrap();
        assert_eq!(verdict, deserialized);

        match deserialized {
            SafetyVerdict::Modify { clamped: c, reason } => {
                assert_eq!(c.tool, "move_arm");
                assert_eq!(reason, "velocity clamped to safe range");
            }
            _ => panic!("expected Modify variant"),
        }
    }

    // -----------------------------------------------------------------------
    // OddConstraint::Range check
    // -----------------------------------------------------------------------

    #[test]
    fn odd_constraint_range_in_range_true() {
        let constraint = OddConstraint::Range { min: 0.0, max: 100.0 };
        assert!(constraint.check(0.0));
        assert!(constraint.check(50.0));
        assert!(constraint.check(100.0));
    }

    #[test]
    fn odd_constraint_range_out_of_range_false() {
        let constraint = OddConstraint::Range { min: 0.0, max: 100.0 };
        assert!(!constraint.check(-0.1));
        assert!(!constraint.check(100.1));
    }

    // -----------------------------------------------------------------------
    // OddConstraint::Threshold check
    // -----------------------------------------------------------------------

    #[test]
    fn odd_constraint_threshold_under_true() {
        let constraint = OddConstraint::Threshold { max: 50.0 };
        assert!(constraint.check(0.0));
        assert!(constraint.check(49.9));
        assert!(constraint.check(50.0));
    }

    #[test]
    fn odd_constraint_threshold_over_false() {
        let constraint = OddConstraint::Threshold { max: 50.0 };
        assert!(!constraint.check(50.1));
        assert!(!constraint.check(100.0));
    }

    // -----------------------------------------------------------------------
    // OddConstraint::Boolean check_bool
    // -----------------------------------------------------------------------

    #[test]
    fn odd_constraint_boolean_matching_true() {
        let constraint = OddConstraint::Boolean { required: true };
        assert!(constraint.check_bool(true));

        let constraint_false = OddConstraint::Boolean { required: false };
        assert!(constraint_false.check_bool(false));
    }

    #[test]
    fn odd_constraint_boolean_non_matching_false() {
        let constraint = OddConstraint::Boolean { required: true };
        assert!(!constraint.check_bool(false));

        let constraint_false = OddConstraint::Boolean { required: false };
        assert!(!constraint_false.check_bool(true));
    }

    // -----------------------------------------------------------------------
    // ControlMode
    // -----------------------------------------------------------------------

    #[test]
    fn control_mode_default_is_autonomous() {
        assert_eq!(ControlMode::default(), ControlMode::Autonomous);
    }

    #[test]
    fn control_mode_for_remote_is_supervised() {
        assert_eq!(ControlMode::for_remote(), ControlMode::Supervised);
    }

    #[test]
    fn control_mode_serde_roundtrip() {
        for mode in [
            ControlMode::Autonomous,
            ControlMode::Supervised,
            ControlMode::Collaborative,
            ControlMode::Manual,
        ] {
            let serialized = serde_json::to_string(&mode).unwrap();
            let deserialized: ControlMode = serde_json::from_str(&serialized).unwrap();
            assert_eq!(mode, deserialized);
        }
    }

    #[test]
    fn control_mode_uses_snake_case() {
        let json = serde_json::to_value(ControlMode::Autonomous).unwrap();
        assert_eq!(json, json!("autonomous"));

        let json = serde_json::to_value(ControlMode::Supervised).unwrap();
        assert_eq!(json, json!("supervised"));

        let json = serde_json::to_value(ControlMode::Collaborative).unwrap();
        assert_eq!(json, json!("collaborative"));

        let json = serde_json::to_value(ControlMode::Manual).unwrap();
        assert_eq!(json, json!("manual"));
    }
}
