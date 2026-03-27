use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

/// Trait implemented by all behavior tree nodes.
pub trait BtNode: Send + Sync {
    /// Execute one tick of this node.
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus;

    /// Signal this node to halt (cleanup running state).
    fn halt(&mut self, bb: &mut Blackboard);

    /// Human-readable name for debugging.
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysSuccess;

    impl BtNode for AlwaysSuccess {
        fn tick(&mut self, _bb: &mut Blackboard) -> BtStatus {
            BtStatus::Success
        }
        fn halt(&mut self, _bb: &mut Blackboard) {}
        fn name(&self) -> &str {
            "always_success"
        }
    }

    struct AlwaysFailure;

    impl BtNode for AlwaysFailure {
        fn tick(&mut self, _bb: &mut Blackboard) -> BtStatus {
            BtStatus::Failure
        }
        fn halt(&mut self, _bb: &mut Blackboard) {}
        fn name(&self) -> &str {
            "always_failure"
        }
    }

    #[test]
    fn always_success_node_returns_success() {
        let mut node = AlwaysSuccess;
        let mut bb = Blackboard::new();
        assert_eq!(node.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn always_failure_node_returns_failure() {
        let mut node = AlwaysFailure;
        let mut bb = Blackboard::new();
        assert_eq!(node.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn node_name_accessible() {
        let node = AlwaysSuccess;
        assert_eq!(node.name(), "always_success");
    }
}
