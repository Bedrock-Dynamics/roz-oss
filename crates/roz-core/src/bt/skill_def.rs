use super::conditions::ConditionSpec;
use super::tree::TreeNode;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// PortDef
// ---------------------------------------------------------------------------

/// Definition of a single input or output port on an execution skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortDef {
    pub name: String,
    pub port_type: String,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// ConditionSet
// ---------------------------------------------------------------------------

/// Groups of conditions evaluated at different phases of skill execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionSet {
    #[serde(default)]
    pub pre: Vec<ConditionSpec>,
    #[serde(default)]
    pub hold: Vec<ConditionSpec>,
    #[serde(default)]
    pub post: Vec<ConditionSpec>,
}

// ---------------------------------------------------------------------------
// HardwareSpec
// ---------------------------------------------------------------------------

/// Hardware-related constraints for executing a skill safely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardwareSpec {
    pub timeout_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_hz: Option<f64>,
    pub reversible: bool,
    pub safe_halt_action: String,
}

// ---------------------------------------------------------------------------
// ExecutionSkillDef
// ---------------------------------------------------------------------------

/// Complete definition of a deterministic execution skill, including its
/// behavior tree, port declarations, conditions, and hardware constraints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionSkillDef {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub inputs: Vec<PortDef>,
    #[serde(default)]
    pub outputs: Vec<PortDef>,
    #[serde(default)]
    pub conditions: ConditionSet,
    pub hardware: HardwareSpec,
    pub tree: TreeNode,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::conditions::ConditionPhase;
    use serde_json::json;
    use std::collections::HashMap;

    // -- PortDef serde roundtrip --

    #[test]
    fn port_def_serde_roundtrip() {
        let port = PortDef {
            name: "target_position".to_string(),
            port_type: "Pose".to_string(),
            required: true,
            default: None,
        };
        let serialized = serde_json::to_string(&port).unwrap();
        let deserialized: PortDef = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, port);
    }

    #[test]
    fn port_def_with_default_value() {
        let port = PortDef {
            name: "speed".to_string(),
            port_type: "f64".to_string(),
            required: false,
            default: Some(json!(1.0)),
        };
        let serialized = serde_json::to_string(&port).unwrap();
        let deserialized: PortDef = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, port);
    }

    // -- ConditionSet default --

    #[test]
    fn condition_set_default_is_empty() {
        let cs = ConditionSet::default();
        assert!(cs.pre.is_empty());
        assert!(cs.hold.is_empty());
        assert!(cs.post.is_empty());
    }

    #[test]
    fn condition_set_serde_roundtrip() {
        let cs = ConditionSet {
            pre: vec![ConditionSpec {
                expression: "{battery} > 20".to_string(),
                phase: ConditionPhase::Pre,
            }],
            hold: vec![ConditionSpec {
                expression: "{velocity} < 5.0".to_string(),
                phase: ConditionPhase::Hold,
            }],
            post: vec![],
        };
        let serialized = serde_json::to_string(&cs).unwrap();
        let deserialized: ConditionSet = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, cs);
    }

    // -- HardwareSpec serde roundtrip --

    #[test]
    fn hardware_spec_serde_roundtrip() {
        let hw = HardwareSpec {
            timeout_secs: 30,
            heartbeat_hz: Some(10.0),
            reversible: true,
            safe_halt_action: "emergency_stop".to_string(),
        };
        let serialized = serde_json::to_string(&hw).unwrap();
        let deserialized: HardwareSpec = serde_json::from_str(&serialized).unwrap();
        // Compare fields individually since HardwareSpec contains f64
        assert_eq!(deserialized.timeout_secs, hw.timeout_secs);
        assert_eq!(deserialized.heartbeat_hz, hw.heartbeat_hz);
        assert_eq!(deserialized.reversible, hw.reversible);
        assert_eq!(deserialized.safe_halt_action, hw.safe_halt_action);
    }

    #[test]
    fn hardware_spec_without_heartbeat() {
        let hw = HardwareSpec {
            timeout_secs: 60,
            heartbeat_hz: None,
            reversible: false,
            safe_halt_action: "safe_stop".to_string(),
        };
        let json = serde_json::to_string(&hw).unwrap();
        assert!(!json.contains("heartbeat_hz"));
    }

    // -- ExecutionSkillDef serde roundtrip --

    #[test]
    fn execution_skill_def_serde_roundtrip() {
        let skill = ExecutionSkillDef {
            name: "pick-and-place".to_string(),
            description: "Pick an object and place it at target".to_string(),
            version: "1.0.0".to_string(),
            inputs: vec![PortDef {
                name: "target".to_string(),
                port_type: "Pose".to_string(),
                required: true,
                default: None,
            }],
            outputs: vec![PortDef {
                name: "result".to_string(),
                port_type: "String".to_string(),
                required: true,
                default: None,
            }],
            conditions: ConditionSet::default(),
            hardware: HardwareSpec {
                timeout_secs: 120,
                heartbeat_hz: Some(5.0),
                reversible: true,
                safe_halt_action: "retract_arm".to_string(),
            },
            tree: TreeNode::Sequence {
                children: vec![TreeNode::Action {
                    name: "grasp".to_string(),
                    action_type: "manipulation".to_string(),
                    ports: HashMap::new(),
                }],
            },
        };
        let serialized = serde_json::to_string(&skill).unwrap();
        let deserialized: ExecutionSkillDef = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, skill.name);
        assert_eq!(deserialized.description, skill.description);
        assert_eq!(deserialized.version, skill.version);
        assert_eq!(deserialized.inputs.len(), 1);
        assert_eq!(deserialized.outputs.len(), 1);
    }

    #[test]
    fn execution_skill_def_with_empty_defaults() {
        let skill = ExecutionSkillDef {
            name: "minimal".to_string(),
            description: "Minimal skill".to_string(),
            version: "0.1.0".to_string(),
            inputs: vec![],
            outputs: vec![],
            conditions: ConditionSet::default(),
            hardware: HardwareSpec {
                timeout_secs: 10,
                heartbeat_hz: None,
                reversible: false,
                safe_halt_action: "stop".to_string(),
            },
            tree: TreeNode::Action {
                name: "noop".to_string(),
                action_type: "test".to_string(),
                ports: HashMap::new(),
            },
        };
        let serialized = serde_json::to_string(&skill).unwrap();
        let deserialized: ExecutionSkillDef = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, "minimal");
        assert!(deserialized.inputs.is_empty());
        assert!(deserialized.outputs.is_empty());
    }
}
