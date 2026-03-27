use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// DecoratorType
// ---------------------------------------------------------------------------

/// The kind of decorator applied to a child node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoratorType {
    Retry { max_attempts: u32 },
    Timeout { secs: u32 },
    Invert,
    Repeat { count: u32 },
    While,
    RepeatUntilFailure,
}

// ---------------------------------------------------------------------------
// TreeNode
// ---------------------------------------------------------------------------

/// A single node in a behavior tree AST, parsed from YAML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TreeNode {
    Action {
        name: String,
        action_type: String,
        #[serde(default)]
        ports: HashMap<String, serde_json::Value>,
    },
    Condition {
        expression: String,
    },
    Sequence {
        children: Vec<Self>,
    },
    Fallback {
        children: Vec<Self>,
    },
    Parallel {
        children: Vec<Self>,
        success_threshold: u32,
    },
    Decorator {
        decorator_type: DecoratorType,
        child: Box<Self>,
    },
    SubTree {
        skill_name: String,
        #[serde(default)]
        port_mappings: HashMap<String, String>,
    },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Action serde roundtrip --

    #[test]
    fn action_node_serde_roundtrip() {
        let node = TreeNode::Action {
            name: "move_arm".to_string(),
            action_type: "motion".to_string(),
            ports: HashMap::from([("target".to_string(), json!({"x": 1.0, "y": 2.0}))]),
        };
        let serialized = serde_json::to_string(&node).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, node);
    }

    // -- Condition serde roundtrip --

    #[test]
    fn condition_node_serde_roundtrip() {
        let node = TreeNode::Condition {
            expression: "{velocity} < 5.0".to_string(),
        };
        let serialized = serde_json::to_string(&node).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, node);
    }

    // -- Sequence serde roundtrip --

    #[test]
    fn sequence_node_serde_roundtrip() {
        let node = TreeNode::Sequence {
            children: vec![
                TreeNode::Action {
                    name: "step1".to_string(),
                    action_type: "io".to_string(),
                    ports: HashMap::new(),
                },
                TreeNode::Action {
                    name: "step2".to_string(),
                    action_type: "io".to_string(),
                    ports: HashMap::new(),
                },
            ],
        };
        let serialized = serde_json::to_string(&node).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, node);
    }

    // -- Fallback serde roundtrip --

    #[test]
    fn fallback_node_serde_roundtrip() {
        let node = TreeNode::Fallback {
            children: vec![
                TreeNode::Condition {
                    expression: "{ready} == true".to_string(),
                },
                TreeNode::Action {
                    name: "fallback_action".to_string(),
                    action_type: "recovery".to_string(),
                    ports: HashMap::new(),
                },
            ],
        };
        let serialized = serde_json::to_string(&node).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, node);
    }

    // -- Parallel serde roundtrip --

    #[test]
    fn parallel_node_with_threshold_serde_roundtrip() {
        let node = TreeNode::Parallel {
            children: vec![
                TreeNode::Action {
                    name: "task_a".to_string(),
                    action_type: "compute".to_string(),
                    ports: HashMap::new(),
                },
                TreeNode::Action {
                    name: "task_b".to_string(),
                    action_type: "compute".to_string(),
                    ports: HashMap::new(),
                },
            ],
            success_threshold: 1,
        };
        let serialized = serde_json::to_string(&node).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, node);
    }

    // -- Decorator serde roundtrip --

    #[test]
    fn decorator_with_child_serde_roundtrip() {
        let node = TreeNode::Decorator {
            decorator_type: DecoratorType::Retry { max_attempts: 3 },
            child: Box::new(TreeNode::Action {
                name: "flaky_action".to_string(),
                action_type: "network".to_string(),
                ports: HashMap::new(),
            }),
        };
        let serialized = serde_json::to_string(&node).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, node);
    }

    // -- SubTree serde roundtrip --

    #[test]
    fn subtree_node_serde_roundtrip() {
        let node = TreeNode::SubTree {
            skill_name: "navigate-to-waypoint".to_string(),
            port_mappings: HashMap::from([
                ("target".to_string(), "waypoint_pos".to_string()),
                ("speed".to_string(), "nav_speed".to_string()),
            ]),
        };
        let serialized = serde_json::to_string(&node).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, node);
    }

    // -- Nested tree structure --

    #[test]
    fn nested_tree_structure_serde_roundtrip() {
        let tree = TreeNode::Sequence {
            children: vec![
                TreeNode::Condition {
                    expression: "{battery} > 20".to_string(),
                },
                TreeNode::Fallback {
                    children: vec![
                        TreeNode::Decorator {
                            decorator_type: DecoratorType::Timeout { secs: 30 },
                            child: Box::new(TreeNode::Action {
                                name: "primary".to_string(),
                                action_type: "motion".to_string(),
                                ports: HashMap::new(),
                            }),
                        },
                        TreeNode::Action {
                            name: "recovery".to_string(),
                            action_type: "safety".to_string(),
                            ports: HashMap::new(),
                        },
                    ],
                },
            ],
        };
        let serialized = serde_json::to_string(&tree).unwrap();
        let deserialized: TreeNode = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, tree);
    }

    // -- DecoratorType variants --

    #[test]
    fn decorator_type_all_variants_serde_roundtrip() {
        let variants = vec![
            DecoratorType::Retry { max_attempts: 5 },
            DecoratorType::Timeout { secs: 60 },
            DecoratorType::Invert,
            DecoratorType::Repeat { count: 10 },
            DecoratorType::While,
            DecoratorType::RepeatUntilFailure,
        ];
        for variant in variants {
            let serialized = serde_json::to_string(&variant).unwrap();
            let deserialized: DecoratorType = serde_json::from_str(&serialized).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    // -- Tag-based deserialization --

    #[test]
    fn tree_node_json_uses_type_tag() {
        let node = TreeNode::Action {
            name: "test".to_string(),
            action_type: "io".to_string(),
            ports: HashMap::new(),
        };
        let value: serde_json::Value = serde_json::to_value(&node).unwrap();
        assert_eq!(value["type"], "action");
    }
}
