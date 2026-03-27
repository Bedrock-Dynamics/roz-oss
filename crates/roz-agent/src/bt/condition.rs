use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::conditions::ConditionResult;
use roz_core::bt::eval::evaluate_condition;
use roz_core::bt::status::BtStatus;

use super::node::BtNode;

/// A BT leaf node that evaluates a condition expression.
/// Always returns Success or Failure — never Running.
pub struct ConditionNode {
    node_name: String,
    expression: String,
}

impl ConditionNode {
    pub fn new(name: impl Into<String>, expression: impl Into<String>) -> Self {
        Self {
            node_name: name.into(),
            expression: expression.into(),
        }
    }
}

impl BtNode for ConditionNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        match evaluate_condition(&self.expression, bb) {
            ConditionResult::Satisfied => BtStatus::Success,
            ConditionResult::Violated { .. } => BtStatus::Failure,
        }
    }

    fn halt(&mut self, _bb: &mut Blackboard) {
        // Conditions are stateless; nothing to halt.
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn condition_true_returns_success() {
        let mut node = ConditionNode::new("check-vel", "{velocity} < 10");
        let mut bb = Blackboard::new();
        bb.set("velocity", json!(5.0));

        assert_eq!(node.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn condition_false_returns_failure() {
        let mut node = ConditionNode::new("check-vel", "{velocity} < 10");
        let mut bb = Blackboard::new();
        bb.set("velocity", json!(15.0));

        assert_eq!(node.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn missing_key_returns_failure() {
        let mut node = ConditionNode::new("check-missing", "{nonexistent} == 1");
        let mut bb = Blackboard::new();

        assert_eq!(node.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn condition_node_never_returns_running() {
        let mut node = ConditionNode::new("check", "{x} == 1");
        let mut bb = Blackboard::new();
        bb.set("x", json!(1));

        let status = node.tick(&mut bb);
        assert_ne!(status, BtStatus::Running);
    }
}
