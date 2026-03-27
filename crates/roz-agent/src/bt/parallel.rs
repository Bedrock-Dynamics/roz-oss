use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

use super::node::BtNode;

/// Ticks all children every tick. Configurable success threshold.
pub struct ParallelNode {
    node_name: String,
    children: Vec<Box<dyn BtNode>>,
    success_threshold: u32,
}

impl ParallelNode {
    pub fn new(name: impl Into<String>, children: Vec<Box<dyn BtNode>>, success_threshold: u32) -> Self {
        Self {
            node_name: name.into(),
            children,
            success_threshold,
        }
    }
}

impl BtNode for ParallelNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        let mut success_count = 0u32;
        let mut failure_count = 0u32;

        for child in &mut self.children {
            match child.tick(bb) {
                BtStatus::Success => success_count += 1,
                BtStatus::Failure => failure_count += 1,
                BtStatus::Running => {}
            }
        }

        #[allow(clippy::cast_possible_truncation)] // BT children count never exceeds u32::MAX
        let total = self.children.len() as u32;

        if success_count >= self.success_threshold {
            BtStatus::Success
        } else if total.saturating_sub(failure_count) < self.success_threshold {
            // Even if all remaining children succeed, can't reach threshold
            BtStatus::Failure
        } else {
            BtStatus::Running
        }
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        for child in &mut self.children {
            child.halt(bb);
        }
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedNode {
        name: String,
        status: BtStatus,
    }

    impl FixedNode {
        fn new(name: &str, status: BtStatus) -> Self {
            Self {
                name: name.to_string(),
                status,
            }
        }
    }

    impl BtNode for FixedNode {
        fn tick(&mut self, _bb: &mut Blackboard) -> BtStatus {
            self.status
        }
        fn halt(&mut self, _bb: &mut Blackboard) {}
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn all_success_meets_threshold() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Success)),
            Box::new(FixedNode::new("b", BtStatus::Success)),
        ];
        let mut par = ParallelNode::new("par", children, 2);
        let mut bb = Blackboard::new();

        assert_eq!(par.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn partial_success_meets_threshold() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Success)),
            Box::new(FixedNode::new("b", BtStatus::Running)),
            Box::new(FixedNode::new("c", BtStatus::Success)),
        ];
        let mut par = ParallelNode::new("par", children, 2);
        let mut bb = Blackboard::new();

        assert_eq!(par.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn too_many_failures_returns_failure() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Failure)),
            Box::new(FixedNode::new("b", BtStatus::Failure)),
            Box::new(FixedNode::new("c", BtStatus::Running)),
        ];
        // Need 2 successes out of 3, but 2 already failed
        let mut par = ParallelNode::new("par", children, 2);
        let mut bb = Blackboard::new();

        assert_eq!(par.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn still_running_when_outcome_undecided() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Success)),
            Box::new(FixedNode::new("b", BtStatus::Running)),
            Box::new(FixedNode::new("c", BtStatus::Running)),
        ];
        // Need 2 successes, have 1 so far, 2 still running
        let mut par = ParallelNode::new("par", children, 2);
        let mut bb = Blackboard::new();

        assert_eq!(par.tick(&mut bb), BtStatus::Running);
    }

    #[test]
    fn empty_parallel_with_zero_threshold_succeeds() {
        let mut par = ParallelNode::new("par", vec![], 0);
        let mut bb = Blackboard::new();
        assert_eq!(par.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn threshold_of_one() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Failure)),
            Box::new(FixedNode::new("b", BtStatus::Success)),
        ];
        let mut par = ParallelNode::new("par", children, 1);
        let mut bb = Blackboard::new();

        assert_eq!(par.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn threshold_exceeding_children_count_does_not_panic() {
        let children: Vec<Box<dyn BtNode>> = vec![Box::new(FixedNode::new("a", BtStatus::Success))];
        // threshold=5 but only 1 child — should not panic from underflow
        let mut par = ParallelNode::new("par", children, 5);
        let mut bb = Blackboard::new();

        // 1 success < 5 threshold, and failure_count (0) > 0 (saturating_sub), so Failure
        assert_eq!(par.tick(&mut bb), BtStatus::Failure);
    }
}
