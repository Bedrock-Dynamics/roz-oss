use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

use super::node::BtNode;

// ---------------------------------------------------------------------------
// RetryNode
// ---------------------------------------------------------------------------

/// Retries a child up to `max_attempts` times on failure.
pub struct RetryNode {
    node_name: String,
    child: Box<dyn BtNode>,
    max_attempts: u32,
    attempts: u32,
}

impl RetryNode {
    pub fn new(name: impl Into<String>, child: Box<dyn BtNode>, max_attempts: u32) -> Self {
        Self {
            node_name: name.into(),
            child,
            max_attempts,
            attempts: 0,
        }
    }
}

impl BtNode for RetryNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        let status = self.child.tick(bb);
        match status {
            BtStatus::Success => {
                self.attempts = 0;
                BtStatus::Success
            }
            BtStatus::Running => BtStatus::Running,
            BtStatus::Failure => {
                self.attempts += 1;
                if self.attempts >= self.max_attempts {
                    self.attempts = 0;
                    BtStatus::Failure
                } else {
                    self.child.halt(bb);
                    BtStatus::Running
                }
            }
        }
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        self.attempts = 0;
        self.child.halt(bb);
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

// ---------------------------------------------------------------------------
// TimeoutNode
// ---------------------------------------------------------------------------

/// Fails if child doesn't complete within `max_ticks` ticks.
pub struct TimeoutNode {
    node_name: String,
    child: Box<dyn BtNode>,
    max_ticks: u32,
    ticks: u32,
}

impl TimeoutNode {
    pub fn new(name: impl Into<String>, child: Box<dyn BtNode>, max_ticks: u32) -> Self {
        Self {
            node_name: name.into(),
            child,
            max_ticks,
            ticks: 0,
        }
    }
}

impl BtNode for TimeoutNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        self.ticks += 1;
        if self.ticks > self.max_ticks {
            self.child.halt(bb);
            self.ticks = 0;
            return BtStatus::Failure;
        }

        let status = self.child.tick(bb);
        if status != BtStatus::Running {
            self.ticks = 0;
        }
        status
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        self.ticks = 0;
        self.child.halt(bb);
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

// ---------------------------------------------------------------------------
// InvertNode
// ---------------------------------------------------------------------------

/// Inverts Success ↔ Failure. Running passes through.
pub struct InvertNode {
    node_name: String,
    child: Box<dyn BtNode>,
}

impl InvertNode {
    pub fn new(name: impl Into<String>, child: Box<dyn BtNode>) -> Self {
        Self {
            node_name: name.into(),
            child,
        }
    }
}

impl BtNode for InvertNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        match self.child.tick(bb) {
            BtStatus::Success => BtStatus::Failure,
            BtStatus::Failure => BtStatus::Success,
            BtStatus::Running => BtStatus::Running,
        }
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        self.child.halt(bb);
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

// ---------------------------------------------------------------------------
// RepeatNode
// ---------------------------------------------------------------------------

/// Repeats a child `count` times, stopping on first failure.
pub struct RepeatNode {
    node_name: String,
    child: Box<dyn BtNode>,
    count: u32,
    current: u32,
}

impl RepeatNode {
    pub fn new(name: impl Into<String>, child: Box<dyn BtNode>, count: u32) -> Self {
        Self {
            node_name: name.into(),
            child,
            count,
            current: 0,
        }
    }
}

impl BtNode for RepeatNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        loop {
            if self.current >= self.count {
                self.current = 0;
                return BtStatus::Success;
            }

            let status = self.child.tick(bb);
            match status {
                BtStatus::Success => {
                    self.current += 1;
                    self.child.halt(bb);
                }
                BtStatus::Running => return BtStatus::Running,
                BtStatus::Failure => {
                    self.current = 0;
                    return BtStatus::Failure;
                }
            }
        }
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        self.current = 0;
        self.child.halt(bb);
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

// ---------------------------------------------------------------------------
// WhileNode
// ---------------------------------------------------------------------------

/// Keeps ticking child while it returns Success. Exits on Failure or
/// Running, and yields `Running` when the iteration limit is reached to
/// prevent starvation in a safety-critical tick loop.
pub struct WhileNode {
    node_name: String,
    child: Box<dyn BtNode>,
    max_iterations: u32,
}

impl WhileNode {
    pub fn new(name: impl Into<String>, child: Box<dyn BtNode>) -> Self {
        Self {
            node_name: name.into(),
            child,
            max_iterations: 1000,
        }
    }

    #[must_use]
    pub const fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max;
        self
    }
}

impl BtNode for WhileNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        for _ in 0..self.max_iterations {
            match self.child.tick(bb) {
                BtStatus::Success => {
                    self.child.halt(bb);
                    // Re-tick: "while" keeps going on success
                }
                other => return other,
            }
        }
        // Iteration limit reached — yield to prevent starvation.
        BtStatus::Running
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        self.child.halt(bb);
    }

    fn name(&self) -> &str {
        &self.node_name
    }
}

// ---------------------------------------------------------------------------
// RepeatUntilFailureNode
// ---------------------------------------------------------------------------

/// Repeats a child until it returns Failure, then returns **Success**.
///
/// Standard BT semantics: "keep going until it fails" — the failure condition
/// being met is the goal, so the node reports Success when the child finally fails.
/// Returns Running if the child returns Running or if the iteration limit is hit.
pub struct RepeatUntilFailureNode {
    node_name: String,
    child: Box<dyn BtNode>,
    max_iterations: u32,
}

impl RepeatUntilFailureNode {
    pub fn new(name: impl Into<String>, child: Box<dyn BtNode>) -> Self {
        Self {
            node_name: name.into(),
            child,
            max_iterations: 1000,
        }
    }

    #[must_use]
    pub const fn with_max_iterations(mut self, max: u32) -> Self {
        self.max_iterations = max;
        self
    }
}

impl BtNode for RepeatUntilFailureNode {
    fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
        for _ in 0..self.max_iterations {
            match self.child.tick(bb) {
                BtStatus::Success => {
                    self.child.halt(bb);
                }
                BtStatus::Failure => return BtStatus::Success, // Goal met
                BtStatus::Running => return BtStatus::Running,
            }
        }
        // Iteration limit reached — yield to prevent starvation.
        BtStatus::Running
    }

    fn halt(&mut self, bb: &mut Blackboard) {
        self.child.halt(bb);
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

    struct SequencedNode {
        name: String,
        sequence: Vec<BtStatus>,
        index: usize,
    }

    impl SequencedNode {
        fn new(name: &str, sequence: Vec<BtStatus>) -> Self {
            Self {
                name: name.to_string(),
                sequence,
                index: 0,
            }
        }
    }

    impl BtNode for SequencedNode {
        fn tick(&mut self, _bb: &mut Blackboard) -> BtStatus {
            let status = self.sequence[self.index.min(self.sequence.len() - 1)];
            if self.index < self.sequence.len() {
                self.index += 1;
            }
            status
        }
        fn halt(&mut self, _bb: &mut Blackboard) {}
        fn name(&self) -> &str {
            &self.name
        }
    }

    // Retry tests
    #[test]
    fn retry_succeeds_on_first_attempt() {
        let child = Box::new(FixedNode::new("ok", BtStatus::Success));
        let mut retry = RetryNode::new("retry", child, 3);
        let mut bb = Blackboard::new();
        assert_eq!(retry.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn retry_retries_on_failure() {
        let child = Box::new(SequencedNode::new(
            "flaky",
            vec![BtStatus::Failure, BtStatus::Failure, BtStatus::Success],
        ));
        let mut retry = RetryNode::new("retry", child, 3);
        let mut bb = Blackboard::new();
        assert_eq!(retry.tick(&mut bb), BtStatus::Running); // fail 1, retry
        assert_eq!(retry.tick(&mut bb), BtStatus::Running); // fail 2, retry
        assert_eq!(retry.tick(&mut bb), BtStatus::Success); // success
    }

    #[test]
    fn retry_fails_after_max_attempts() {
        let child = Box::new(FixedNode::new("fail", BtStatus::Failure));
        let mut retry = RetryNode::new("retry", child, 2);
        let mut bb = Blackboard::new();
        assert_eq!(retry.tick(&mut bb), BtStatus::Running); // fail 1
        assert_eq!(retry.tick(&mut bb), BtStatus::Failure); // fail 2 = max
    }

    // Timeout tests
    #[test]
    fn timeout_succeeds_within_limit() {
        let child = Box::new(SequencedNode::new("slow", vec![BtStatus::Running, BtStatus::Success]));
        let mut timeout = TimeoutNode::new("timeout", child, 5);
        let mut bb = Blackboard::new();
        assert_eq!(timeout.tick(&mut bb), BtStatus::Running);
        assert_eq!(timeout.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn timeout_fails_when_exceeded() {
        let child = Box::new(FixedNode::new("stuck", BtStatus::Running));
        let mut timeout = TimeoutNode::new("timeout", child, 2);
        let mut bb = Blackboard::new();
        assert_eq!(timeout.tick(&mut bb), BtStatus::Running); // tick 1
        assert_eq!(timeout.tick(&mut bb), BtStatus::Running); // tick 2
        assert_eq!(timeout.tick(&mut bb), BtStatus::Failure); // tick 3 > max
    }

    // Invert tests
    #[test]
    fn invert_success_to_failure() {
        let child = Box::new(FixedNode::new("ok", BtStatus::Success));
        let mut inv = InvertNode::new("invert", child);
        let mut bb = Blackboard::new();
        assert_eq!(inv.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn invert_failure_to_success() {
        let child = Box::new(FixedNode::new("fail", BtStatus::Failure));
        let mut inv = InvertNode::new("invert", child);
        let mut bb = Blackboard::new();
        assert_eq!(inv.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn invert_running_unchanged() {
        let child = Box::new(FixedNode::new("run", BtStatus::Running));
        let mut inv = InvertNode::new("invert", child);
        let mut bb = Blackboard::new();
        assert_eq!(inv.tick(&mut bb), BtStatus::Running);
    }

    // Repeat tests
    #[test]
    fn repeat_completes_all_iterations() {
        let child = Box::new(FixedNode::new("ok", BtStatus::Success));
        let mut rep = RepeatNode::new("repeat", child, 3);
        let mut bb = Blackboard::new();
        assert_eq!(rep.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn repeat_stops_on_failure() {
        let child = Box::new(SequencedNode::new("flaky", vec![BtStatus::Success, BtStatus::Failure]));
        let mut rep = RepeatNode::new("repeat", child, 5);
        let mut bb = Blackboard::new();
        assert_eq!(rep.tick(&mut bb), BtStatus::Failure);
    }

    // While tests
    #[test]
    fn while_node_terminates_on_child_failure() {
        let child = Box::new(SequencedNode::new(
            "counted",
            vec![BtStatus::Success, BtStatus::Success, BtStatus::Failure],
        ));
        let mut while_node = WhileNode::new("while", child);
        let mut bb = Blackboard::new();
        assert_eq!(while_node.tick(&mut bb), BtStatus::Failure);
    }

    #[test]
    fn while_node_returns_running_on_child_running() {
        let child = Box::new(SequencedNode::new(
            "pausing",
            vec![BtStatus::Success, BtStatus::Running],
        ));
        let mut while_node = WhileNode::new("while", child);
        let mut bb = Blackboard::new();
        assert_eq!(while_node.tick(&mut bb), BtStatus::Running);
    }

    #[test]
    fn while_node_yields_running_at_iteration_limit() {
        let child = Box::new(FixedNode::new("always_ok", BtStatus::Success));
        let mut while_node = WhileNode::new("while", child).with_max_iterations(5);
        let mut bb = Blackboard::new();
        assert_eq!(while_node.tick(&mut bb), BtStatus::Running);
    }

    // RepeatUntilFailure tests
    #[test]
    fn repeat_until_failure_returns_success_on_child_failure() {
        // Child succeeds 3 times then fails -> RepeatUntilFailureNode returns Success
        let child = Box::new(SequencedNode::new(
            "eventually_fails",
            vec![
                BtStatus::Success,
                BtStatus::Success,
                BtStatus::Success,
                BtStatus::Failure,
            ],
        ));
        let mut node = RepeatUntilFailureNode::new("repeat_until_failure", child);
        let mut bb = Blackboard::new();
        assert_eq!(node.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn repeat_until_failure_returns_running_on_child_running() {
        let child = Box::new(SequencedNode::new(
            "pausing",
            vec![BtStatus::Success, BtStatus::Running],
        ));
        let mut node = RepeatUntilFailureNode::new("repeat_until_failure", child);
        let mut bb = Blackboard::new();
        assert_eq!(node.tick(&mut bb), BtStatus::Running);
    }

    #[test]
    fn repeat_until_failure_immediate_failure_returns_success() {
        let child = Box::new(FixedNode::new("fail", BtStatus::Failure));
        let mut node = RepeatUntilFailureNode::new("repeat_until_failure", child);
        let mut bb = Blackboard::new();
        assert_eq!(node.tick(&mut bb), BtStatus::Success);
    }

    #[test]
    fn repeat_until_failure_yields_running_at_iteration_limit() {
        let child = Box::new(FixedNode::new("always_ok", BtStatus::Success));
        let mut node = RepeatUntilFailureNode::new("repeat_until_failure", child).with_max_iterations(5);
        let mut bb = Blackboard::new();
        assert_eq!(node.tick(&mut bb), BtStatus::Running);
    }
}
