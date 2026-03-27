use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

use super::node::BtNode;

/// Trait for custom action execution logic.
pub trait ActionExecutor: Send + Sync {
    /// Called on the first tick or when transitioning from idle to running.
    fn on_start(&mut self, bb: &mut Blackboard) -> BtStatus;

    /// Called on subsequent ticks while the action is running.
    fn on_running(&mut self, bb: &mut Blackboard) -> BtStatus;

    /// Called when the node is halted while running.
    fn on_halted(&mut self, bb: &mut Blackboard);

    /// Name of this action executor.
    fn action_type(&self) -> &'static str;
}

/// A BT leaf node that delegates to an `ActionExecutor`.
pub struct ActionNode {
    executor: Box<dyn ActionExecutor>,
    is_running: bool,
    node_name: String,
}

impl ActionNode {
    pub fn new(name: impl Into<String>, executor: Box<dyn ActionExecutor>) -> Self {
        Self {
            node_name: name.into(),
            executor,
            is_running: false,
        }
    }
}

impl BtNode for ActionNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        let status = if self.is_running {
            self.executor.on_running(bb)
        } else {
            self.executor.on_start(bb)
        };

        self.is_running = status == BtStatus::Running;
        status
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        if self.is_running {
            self.executor.on_halted(bb);
            self.is_running = false;
        }
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct CountingExecutor {
        ticks: u32,
        max_ticks: u32,
        halted: bool,
    }

    impl CountingExecutor {
        fn new(max_ticks: u32) -> Self {
            Self {
                ticks: 0,
                max_ticks,
                halted: false,
            }
        }
    }

    impl ActionExecutor for CountingExecutor {
        fn on_start(&mut self, bb: &mut Blackboard) -> BtStatus {
            self.ticks = 1;
            bb.set("ticks", json!(self.ticks));
            if self.ticks >= self.max_ticks {
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }

        fn on_running(&mut self, bb: &mut Blackboard) -> BtStatus {
            self.ticks += 1;
            bb.set("ticks", json!(self.ticks));
            if self.ticks >= self.max_ticks {
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }

        fn on_halted(&mut self, _bb: &mut Blackboard) {
            self.halted = true;
        }

        fn action_type(&self) -> &'static str {
            "counting"
        }
    }

    #[test]
    fn action_node_immediate_success() {
        let executor = CountingExecutor::new(1);
        let mut node = ActionNode::new("count-1", Box::new(executor));
        let mut bb = Blackboard::new();

        let status = node.tick(&mut bb);
        assert_eq!(status, BtStatus::Success);
        assert_eq!(bb.get("ticks").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn action_node_multi_tick_to_success() {
        let executor = CountingExecutor::new(3);
        let mut node = ActionNode::new("count-3", Box::new(executor));
        let mut bb = Blackboard::new();

        assert_eq!(node.tick(&mut bb), BtStatus::Running);
        assert_eq!(node.tick(&mut bb), BtStatus::Running);
        assert_eq!(node.tick(&mut bb), BtStatus::Success);
        assert_eq!(bb.get("ticks").and_then(|v| v.as_u64()), Some(3));
    }

    #[test]
    fn action_node_halt_resets_running() {
        let executor = CountingExecutor::new(5);
        let mut node = ActionNode::new("count-5", Box::new(executor));
        let mut bb = Blackboard::new();

        assert_eq!(node.tick(&mut bb), BtStatus::Running);
        node.halt(&mut bb);
        // After halt, next tick calls on_start again
        assert_eq!(node.tick(&mut bb), BtStatus::Running);
        assert_eq!(bb.get("ticks").and_then(|v| v.as_u64()), Some(1));
    }

    #[test]
    fn action_node_name() {
        let executor = CountingExecutor::new(1);
        let node = ActionNode::new("my-action", Box::new(executor));
        assert_eq!(node.name(), "my-action");
    }
}
