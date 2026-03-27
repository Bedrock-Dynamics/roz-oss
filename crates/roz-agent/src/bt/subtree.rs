use std::collections::HashMap;

use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

use super::node::BtNode;

/// A subtree node that runs a child with an isolated blackboard.
/// Input/output ports are remapped at the boundary.
pub struct SubTreeNode {
    node_name: String,
    child: Box<dyn BtNode>,
    input_mappings: HashMap<String, String>,
    output_mappings: HashMap<String, String>,
    child_bb: Blackboard,
}

impl SubTreeNode {
    /// Create a subtree node.
    /// - `input_mappings`: parent key → child key (copied into child BB before tick)
    /// - `output_mappings`: child key → parent key (copied back after tick)
    pub fn new(
        name: impl Into<String>,
        child: Box<dyn BtNode>,
        input_mappings: HashMap<String, String>,
        output_mappings: HashMap<String, String>,
    ) -> Self {
        Self {
            node_name: name.into(),
            child,
            input_mappings,
            output_mappings,
            child_bb: Blackboard::new(),
        }
    }
}

impl BtNode for SubTreeNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        // Copy inputs from parent → child
        for (parent_key, child_key) in &self.input_mappings {
            if let Some(val) = bb.get(parent_key) {
                self.child_bb.set(child_key, val.clone());
            }
        }

        let status = self.child.tick(&mut self.child_bb);

        // Copy outputs from child → parent
        for (child_key, parent_key) in &self.output_mappings {
            if let Some(val) = self.child_bb.get(child_key) {
                bb.set(parent_key, val.clone());
            }
        }

        status
    }

    fn halt(&mut self, _bb: &mut Blackboard) {
        self.child.halt(&mut self.child_bb);
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct SetOutputNode {
        name: String,
        key: String,
        value: serde_json::Value,
    }

    impl BtNode for SetOutputNode {
        fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
            bb.set(&self.key, self.value.clone());
            BtStatus::Success
        }
        fn halt(&mut self, _bb: &mut Blackboard) {}
        fn name(&self) -> &str {
            &self.name
        }
    }

    struct ReadInputNode {
        name: String,
        key: String,
        found: Option<serde_json::Value>,
    }

    impl BtNode for ReadInputNode {
        fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
            self.found = bb.get(&self.key).cloned();
            if self.found.is_some() {
                BtStatus::Success
            } else {
                BtStatus::Failure
            }
        }
        fn halt(&mut self, _bb: &mut Blackboard) {}
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn input_mapping_copies_to_child() {
        let child = Box::new(ReadInputNode {
            name: "reader".to_string(),
            key: "child_input".to_string(),
            found: None,
        });

        let mut inputs = HashMap::new();
        inputs.insert("parent_val".to_string(), "child_input".to_string());

        let mut subtree = SubTreeNode::new("sub", child, inputs, HashMap::new());
        let mut bb = Blackboard::new();
        bb.set("parent_val", json!(42));

        let status = subtree.tick(&mut bb);
        assert_eq!(status, BtStatus::Success);
    }

    #[test]
    fn output_mapping_copies_back_to_parent() {
        let child = Box::new(SetOutputNode {
            name: "writer".to_string(),
            key: "child_result".to_string(),
            value: json!("done"),
        });

        let mut outputs = HashMap::new();
        outputs.insert("child_result".to_string(), "parent_result".to_string());

        let mut subtree = SubTreeNode::new("sub", child, HashMap::new(), outputs);
        let mut bb = Blackboard::new();

        subtree.tick(&mut bb);
        assert_eq!(bb.get("parent_result"), Some(&json!("done")));
    }

    #[test]
    fn child_bb_isolated_from_parent() {
        let child = Box::new(ReadInputNode {
            name: "reader".to_string(),
            key: "secret".to_string(),
            found: None,
        });

        // No input mapping for "secret"
        let mut subtree = SubTreeNode::new("sub", child, HashMap::new(), HashMap::new());
        let mut bb = Blackboard::new();
        bb.set("secret", json!("hidden"));

        // Child should NOT see parent's "secret" because there's no mapping
        let status = subtree.tick(&mut bb);
        assert_eq!(status, BtStatus::Failure);
    }

    /// Node whose halt() writes a cleanup key into whatever blackboard it receives.
    /// This lets us detect whether halt got the child BB or the parent BB.
    struct HaltWriterNode {
        name: String,
    }

    impl BtNode for HaltWriterNode {
        fn tick(&mut self, _bb: &mut Blackboard) -> BtStatus {
            BtStatus::Running
        }
        fn halt(&mut self, bb: &mut Blackboard) {
            bb.set("__halt_cleanup", json!(true));
        }
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn halt_uses_child_blackboard_not_parent() {
        let mut subtree = SubTreeNode::new(
            "sub",
            Box::new(HaltWriterNode {
                name: "writer".to_string(),
            }),
            HashMap::new(),
            HashMap::new(),
        );
        let mut parent_bb = Blackboard::new();

        // Tick so the subtree is in Running state
        subtree.tick(&mut parent_bb);

        // Halt: child.halt() writes __halt_cleanup into the BB it receives.
        // BUG: if halt passes parent_bb, the write pollutes the parent.
        // FIX: halt should pass child_bb, so parent stays clean.
        subtree.halt(&mut parent_bb);

        // With the bug, __halt_cleanup leaks into parent_bb.
        // With the fix, it goes into child_bb and parent_bb stays clean.
        assert!(
            parent_bb.get("__halt_cleanup").is_none(),
            "halt() must pass child_bb to child.halt(), not parent_bb — \
             cleanup key leaked into parent blackboard"
        );
    }

    #[test]
    fn bidirectional_mapping() {
        let child = Box::new(SetOutputNode {
            name: "process".to_string(),
            key: "result".to_string(),
            value: json!(100),
        });

        let mut inputs = HashMap::new();
        inputs.insert("input_val".to_string(), "input_val".to_string());

        let mut outputs = HashMap::new();
        outputs.insert("result".to_string(), "output_val".to_string());

        let mut subtree = SubTreeNode::new("sub", child, inputs, outputs);
        let mut bb = Blackboard::new();
        bb.set("input_val", json!(50));

        subtree.tick(&mut bb);
        assert_eq!(bb.get("output_val"), Some(&json!(100)));
    }
}
