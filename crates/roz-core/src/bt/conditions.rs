use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ConditionPhase
// ---------------------------------------------------------------------------

/// When a condition should be evaluated relative to skill execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionPhase {
    /// Evaluated before the skill starts.
    Pre,
    /// Monitored continuously while the skill runs.
    Hold,
    /// Evaluated after the skill completes.
    Post,
}

// ---------------------------------------------------------------------------
// ConditionSpec
// ---------------------------------------------------------------------------

/// A condition expression bound to a specific evaluation phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionSpec {
    pub expression: String,
    pub phase: ConditionPhase,
}

// ---------------------------------------------------------------------------
// ConditionResult
// ---------------------------------------------------------------------------

/// The outcome of evaluating a condition expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionResult {
    Satisfied,
    Violated { reason: String },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ConditionPhase serde roundtrip --

    #[test]
    fn condition_phase_all_variants_serde_roundtrip() {
        let variants = [
            (ConditionPhase::Pre, "\"pre\""),
            (ConditionPhase::Hold, "\"hold\""),
            (ConditionPhase::Post, "\"post\""),
        ];
        for (variant, expected_json) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json);
            let deserialized: ConditionPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    // -- ConditionSpec serde roundtrip --

    #[test]
    fn condition_spec_serde_roundtrip() {
        let spec = ConditionSpec {
            expression: "{velocity} < 5.0".to_string(),
            phase: ConditionPhase::Hold,
        };
        let serialized = serde_json::to_string(&spec).unwrap();
        let deserialized: ConditionSpec = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, spec);
    }

    // -- ConditionResult serde roundtrips --

    #[test]
    fn condition_result_satisfied_serde_roundtrip() {
        let result = ConditionResult::Satisfied;
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ConditionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, ConditionResult::Satisfied);
    }

    #[test]
    fn condition_result_violated_serde_roundtrip() {
        let result = ConditionResult::Violated {
            reason: "velocity exceeded threshold".to_string(),
        };
        let serialized = serde_json::to_string(&result).unwrap();
        let deserialized: ConditionResult = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, result);
    }

    // -- ConditionResult construction --

    #[test]
    fn condition_result_violated_carries_reason() {
        let result = ConditionResult::Violated {
            reason: "sensor offline".to_string(),
        };
        match result {
            ConditionResult::Violated { reason } => {
                assert_eq!(reason, "sensor offline");
            }
            ConditionResult::Satisfied => panic!("expected Violated variant"),
        }
    }

    // -- ConditionSpec with different phases --

    #[test]
    fn condition_spec_pre_phase() {
        let spec = ConditionSpec {
            expression: "{battery} > 20".to_string(),
            phase: ConditionPhase::Pre,
        };
        let serialized = serde_json::to_string(&spec).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["phase"], "pre");
    }
}
