use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// BtStatus
// ---------------------------------------------------------------------------

/// The tick-level result of a behavior tree node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtStatus {
    Success,
    Failure,
    Running,
}

// ---------------------------------------------------------------------------
// NodeKind
// ---------------------------------------------------------------------------

/// The structural kind of a behavior tree node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Action,
    Condition,
    Sequence,
    Fallback,
    Parallel,
    Decorator,
    SubTree,
}

// ---------------------------------------------------------------------------
// PortDirection
// ---------------------------------------------------------------------------

/// Direction of a port on a behavior tree node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortDirection {
    Input,
    Output,
    Bidirectional,
}

// ---------------------------------------------------------------------------
// PortSpec
// ---------------------------------------------------------------------------

/// Specification of a single port on a behavior tree node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSpec {
    pub name: String,
    pub direction: PortDirection,
    pub port_type: String,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- BtStatus serde roundtrip --

    #[test]
    fn bt_status_success_serde_roundtrip() {
        let status = BtStatus::Success;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"success\"");
        let deserialized: BtStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, BtStatus::Success);
    }

    #[test]
    fn bt_status_failure_serde_roundtrip() {
        let status = BtStatus::Failure;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"failure\"");
        let deserialized: BtStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, BtStatus::Failure);
    }

    #[test]
    fn bt_status_running_serde_roundtrip() {
        let status = BtStatus::Running;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"running\"");
        let deserialized: BtStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, BtStatus::Running);
    }

    // -- NodeKind serde roundtrip --

    #[test]
    fn node_kind_all_variants_serde_roundtrip() {
        let variants = [
            (NodeKind::Action, "\"action\""),
            (NodeKind::Condition, "\"condition\""),
            (NodeKind::Sequence, "\"sequence\""),
            (NodeKind::Fallback, "\"fallback\""),
            (NodeKind::Parallel, "\"parallel\""),
            (NodeKind::Decorator, "\"decorator\""),
            (NodeKind::SubTree, "\"sub_tree\""),
        ];
        for (variant, expected_json) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json);
            let deserialized: NodeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    // -- PortDirection serde roundtrip --

    #[test]
    fn port_direction_all_variants_serde_roundtrip() {
        let variants = [
            (PortDirection::Input, "\"input\""),
            (PortDirection::Output, "\"output\""),
            (PortDirection::Bidirectional, "\"bidirectional\""),
        ];
        for (variant, expected_json) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json);
            let deserialized: PortDirection = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    // -- PortSpec serde roundtrip --

    #[test]
    fn port_spec_serde_roundtrip() {
        let spec = PortSpec {
            name: "velocity".to_string(),
            direction: PortDirection::Input,
            port_type: "f64".to_string(),
            required: true,
            default: None,
        };
        let serialized = serde_json::to_string(&spec).unwrap();
        let deserialized: PortSpec = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, spec);
    }

    #[test]
    fn port_spec_with_default_value() {
        let spec = PortSpec {
            name: "timeout".to_string(),
            direction: PortDirection::Input,
            port_type: "u32".to_string(),
            required: false,
            default: Some(json!(30)),
        };
        let serialized = serde_json::to_string(&spec).unwrap();
        let deserialized: PortSpec = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, spec);
        assert_eq!(deserialized.default, Some(json!(30)));
    }

    #[test]
    fn port_spec_default_omitted_when_none() {
        let spec = PortSpec {
            name: "target".to_string(),
            direction: PortDirection::Output,
            port_type: "String".to_string(),
            required: true,
            default: None,
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(!json.contains("default"));
    }
}
