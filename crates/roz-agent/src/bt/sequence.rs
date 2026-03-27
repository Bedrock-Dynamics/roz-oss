use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

use super::node::BtNode;

/// Ticks children left-to-right. First Failure stops the sequence.
/// Remembers the index of the Running child for reactive sequences.
pub struct SequenceNode {
    node_name: String,
    children: Vec<Box<dyn BtNode>>,
    running_index: Option<usize>,
}

impl SequenceNode {
    pub fn new(name: impl Into<String>, children: Vec<Box<dyn BtNode>>) -> Self {
        Self {
            node_name: name.into(),
            children,
            running_index: None,
        }
    }
}

impl BtNode for SequenceNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        let start = self.running_index.unwrap_or(0);

        for i in start..self.children.len() {
            let status = self.children[i].tick(bb);
            match status {
                BtStatus::Success => {}
                BtStatus::Running => {
                    self.running_index = Some(i);
                    return BtStatus::Running;
                }
                BtStatus::Failure => {
                    self.running_index = None;
                    return BtStatus::Failure;
                }
            }
        }

        self.running_index = None;
        BtStatus::Success
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        if let Some(idx) = self.running_index {
            self.children[idx].halt(bb);
        }
        self.running_index = None;
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

    struct TickCounter {
        name: String,
        ticks: u32,
        target: u32,
    }

    impl TickCounter {
        fn new(name: &str, target: u32) -> Self {
            Self {
                name: name.to_string(),
                ticks: 0,
                target,
            }
        }
    }

    impl BtNode for TickCounter {
        fn tick(&mut self, _bb: &mut Blackboard) -> BtStatus {
            self.ticks += 1;
            if self.ticks >= self.target {
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }
        fn halt(&mut self, _bb: &mut Blackboard) {
            self.ticks = 0;
        }
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn all_success_returns_success() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Success)),
            Box::new(FixedNode::new("b", BtStatus::Success)),
            Box::new(FixedNode::new("c", BtStatus::Success)),
        ];
        let mut seq = SequenceNode::new("seq", children);
        let mut bb = Blackboard::new();

        assert_eq!(seq.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn first_failure_stops_sequence() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Success)),
            Box::new(FixedNode::new("b", BtStatus::Failure)),
            Box::new(FixedNode::new("c", BtStatus::Success)),
        ];
        let mut seq = SequenceNode::new("seq", children);
        let mut bb = Blackboard::new();

        assert_eq!(seq.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn running_child_remembered() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Success)),
            Box::new(TickCounter::new("b", 3)),
        ];
        let mut seq = SequenceNode::new("seq", children);
        let mut bb = Blackboard::new();

        assert_eq!(seq.tick(&mut bb), BtStatus::Running);
        assert_eq!(seq.tick(&mut bb), BtStatus::Running);
        assert_eq!(seq.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn empty_sequence_returns_success() {
        let mut seq = SequenceNode::new("empty", vec![]);
        let mut bb = Blackboard::new();
        assert_eq!(seq.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn halt_resets_running_index() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Success)),
            Box::new(TickCounter::new("b", 5)),
        ];
        let mut seq = SequenceNode::new("seq", children);
        let mut bb = Blackboard::new();

        assert_eq!(seq.tick(&mut bb), BtStatus::Running);
        seq.halt(&mut bb);
        // After halt, starts from beginning again
        assert_eq!(seq.tick(&mut bb), BtStatus::Running);
    }
}
