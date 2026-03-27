use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

use super::node::BtNode;

/// Ticks children left-to-right. First Success stops the fallback.
/// Remembers the index of the Running child.
pub struct FallbackNode {
    node_name: String,
    children: Vec<Box<dyn BtNode>>,
    running_index: Option<usize>,
}

impl FallbackNode {
    pub fn new(name: impl Into<String>, children: Vec<Box<dyn BtNode>>) -> Self {
        Self {
            node_name: name.into(),
            children,
            running_index: None,
        }
    }
}

impl BtNode for FallbackNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        let start = self.running_index.unwrap_or(0);

        for i in start..self.children.len() {
            let status = self.children[i].tick(bb);
            match status {
                BtStatus::Failure => {}
                BtStatus::Running => {
                    self.running_index = Some(i);
                    return BtStatus::Running;
                }
                BtStatus::Success => {
                    self.running_index = None;
                    return BtStatus::Success;
                }
            }
        }

        self.running_index = None;
        BtStatus::Failure
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

    #[test]
    fn first_success_stops_fallback() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Failure)),
            Box::new(FixedNode::new("b", BtStatus::Success)),
            Box::new(FixedNode::new("c", BtStatus::Failure)),
        ];
        let mut fb = FallbackNode::new("fb", children);
        let mut bb = Blackboard::new();

        assert_eq!(fb.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn all_failure_returns_failure() {
        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Failure)),
            Box::new(FixedNode::new("b", BtStatus::Failure)),
        ];
        let mut fb = FallbackNode::new("fb", children);
        let mut bb = Blackboard::new();

        assert_eq!(fb.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn empty_fallback_returns_failure() {
        let mut fb = FallbackNode::new("empty", vec![]);
        let mut bb = Blackboard::new();
        assert_eq!(fb.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn running_child_remembered_in_fallback() {
        struct TickCounter {
            name: String,
            ticks: u32,
            target: u32,
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

        let children: Vec<Box<dyn BtNode>> = vec![
            Box::new(FixedNode::new("a", BtStatus::Failure)),
            Box::new(TickCounter {
                name: "b".to_string(),
                ticks: 0,
                target: 2,
            }),
        ];
        let mut fb = FallbackNode::new("fb", children);
        let mut bb = Blackboard::new();

        assert_eq!(fb.tick(&mut bb), BtStatus::Running);
        assert_eq!(fb.tick(&mut bb), BtStatus::Success);
    }
}
