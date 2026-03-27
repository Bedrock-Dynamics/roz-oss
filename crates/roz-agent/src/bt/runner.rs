use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;
use serde::{Deserialize, Serialize};

use super::conditions::{CheckResult, ConditionChecker};
use super::node::BtNode;

/// Result of a single tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillTickResult {
    Running,
    Completed { success: bool },
    PreViolation { condition: String, reason: String },
    HoldViolation { condition: String, reason: String },
    PostViolation { condition: String, reason: String },
}

/// Final result of running a skill to completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResult {
    pub success: bool,
    pub ticks: u32,
    pub message: Option<String>,
}

/// Runs an execution skill's behavior tree with condition checking.
pub struct SkillRunner {
    root: Box<dyn BtNode>,
    checker: ConditionChecker,
    blackboard: Blackboard,
    started: bool,
    ticks: u32,
}

impl SkillRunner {
    pub fn new(root: Box<dyn BtNode>, checker: ConditionChecker) -> Self {
        Self {
            root,
            checker,
            blackboard: Blackboard::new(),
            started: false,
            ticks: 0,
        }
    }

    /// Access the blackboard for setting up initial values.
    pub const fn blackboard_mut(&mut self) -> &mut Blackboard {
        &mut self.blackboard
    }

    /// Execute a single tick with condition checking.
    pub fn tick(&mut self) -> SkillTickResult {
        // Check pre-conditions before first tick
        if !self.started {
            if let CheckResult::Violated { condition, reason } = self.checker.check_pre(&self.blackboard) {
                return SkillTickResult::PreViolation { condition, reason };
            }
            self.started = true;
        }

        self.ticks += 1;
        let status = self.root.tick(&mut self.blackboard);

        // Check hold-conditions AFTER tick (validates updated blackboard)
        if status == BtStatus::Running
            && let CheckResult::Violated { condition, reason } = self.checker.check_hold(&self.blackboard)
        {
            self.root.halt(&mut self.blackboard);
            return SkillTickResult::HoldViolation { condition, reason };
        }

        match status {
            BtStatus::Running => SkillTickResult::Running,
            BtStatus::Success => {
                // Check post-conditions after completion
                if let CheckResult::Violated { condition, reason } = self.checker.check_post(&self.blackboard) {
                    return SkillTickResult::PostViolation { condition, reason };
                }
                SkillTickResult::Completed { success: true }
            }
            BtStatus::Failure => SkillTickResult::Completed { success: false },
        }
    }

    /// Run the skill to completion or until `max_ticks` is reached.
    pub fn run_to_completion(&mut self, max_ticks: u32) -> SkillResult {
        for _ in 0..max_ticks {
            match self.tick() {
                SkillTickResult::Running => {}
                SkillTickResult::Completed { success } => {
                    return SkillResult {
                        success,
                        ticks: self.ticks,
                        message: None,
                    };
                }
                SkillTickResult::PreViolation { condition, reason } => {
                    return SkillResult {
                        success: false,
                        ticks: self.ticks,
                        message: Some(format!("Pre-condition violation: {condition} — {reason}")),
                    };
                }
                SkillTickResult::HoldViolation { condition, reason } => {
                    return SkillResult {
                        success: false,
                        ticks: self.ticks,
                        message: Some(format!("Hold violation: {condition} — {reason}")),
                    };
                }
                SkillTickResult::PostViolation { condition, reason } => {
                    return SkillResult {
                        success: false,
                        ticks: self.ticks,
                        message: Some(format!("Post-condition violation: {condition} — {reason}")),
                    };
                }
            }
        }

        self.root.halt(&mut self.blackboard);
        SkillResult {
            success: false,
            ticks: self.ticks,
            message: Some(format!("Max ticks ({max_ticks}) exceeded")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::bt::conditions::{ConditionPhase, ConditionSpec};
    use serde_json::json;

    struct FixedNode {
        name: String,
        status: BtStatus,
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

    struct CountdownNode {
        name: String,
        remaining: u32,
    }

    impl BtNode for CountdownNode {
        fn tick(&mut self, _bb: &mut Blackboard) -> BtStatus {
            if self.remaining == 0 {
                return BtStatus::Success;
            }
            self.remaining -= 1;
            if self.remaining == 0 {
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }
        fn halt(&mut self, _bb: &mut Blackboard) {}
        fn name(&self) -> &str {
            &self.name
        }
    }

    fn no_conditions() -> ConditionChecker {
        ConditionChecker::new(vec![], vec![], vec![])
    }

    #[test]
    fn immediate_success() {
        let root = Box::new(FixedNode {
            name: "ok".to_string(),
            status: BtStatus::Success,
        });
        let mut runner = SkillRunner::new(root, no_conditions());
        let result = runner.run_to_completion(10);
        assert!(result.success);
        assert_eq!(result.ticks, 1);
    }

    #[test]
    fn multi_tick_to_completion() {
        let root = Box::new(CountdownNode {
            name: "countdown".to_string(),
            remaining: 3,
        });
        let mut runner = SkillRunner::new(root, no_conditions());
        let result = runner.run_to_completion(10);
        assert!(result.success);
        assert_eq!(result.ticks, 3);
    }

    #[test]
    fn max_ticks_exceeded() {
        let root = Box::new(FixedNode {
            name: "stuck".to_string(),
            status: BtStatus::Running,
        });
        let mut runner = SkillRunner::new(root, no_conditions());
        let result = runner.run_to_completion(5);
        assert!(!result.success);
        assert_eq!(result.ticks, 5);
        assert!(result.message.unwrap().contains("Max ticks"));
    }

    #[test]
    fn pre_condition_violation_prevents_start() {
        let root = Box::new(FixedNode {
            name: "ok".to_string(),
            status: BtStatus::Success,
        });
        let checker = ConditionChecker::new(
            vec![ConditionSpec {
                expression: "{armed} == true".to_string(),
                phase: ConditionPhase::Pre,
            }],
            vec![],
            vec![],
        );
        let mut runner = SkillRunner::new(root, checker);
        runner.blackboard_mut().set("armed", json!(false));

        let result = runner.tick();
        assert!(matches!(result, SkillTickResult::PreViolation { .. }));
    }

    #[test]
    fn hold_condition_violation_halts_skill() {
        struct VelocityIncreaser {
            tick_count: u32,
        }

        impl BtNode for VelocityIncreaser {
            fn tick(&mut self, bb: &mut Blackboard) -> BtStatus {
                self.tick_count += 1;
                bb.set("velocity", json!(self.tick_count * 5));
                BtStatus::Running
            }
            fn halt(&mut self, _bb: &mut Blackboard) {}
            fn name(&self) -> &str {
                "vel_inc"
            }
        }

        let root = Box::new(VelocityIncreaser { tick_count: 0 });
        let checker = ConditionChecker::new(
            vec![],
            vec![ConditionSpec {
                expression: "{velocity} < 10".to_string(),
                phase: ConditionPhase::Hold,
            }],
            vec![],
        );
        let mut runner = SkillRunner::new(root, checker);
        runner.blackboard_mut().set("velocity", json!(0));

        // Hold check happens AFTER tick, so it sees updated blackboard:
        assert_eq!(runner.tick(), SkillTickResult::Running); // velocity=5 after tick, hold passes (5 < 10)
        assert!(matches!(runner.tick(), SkillTickResult::HoldViolation { .. })); // velocity=10 after tick, hold violated (10 not < 10)
    }

    #[test]
    fn failure_result() {
        let root = Box::new(FixedNode {
            name: "fail".to_string(),
            status: BtStatus::Failure,
        });
        let mut runner = SkillRunner::new(root, no_conditions());
        let result = runner.run_to_completion(10);
        assert!(!result.success);
        assert_eq!(result.ticks, 1);
        assert!(result.message.is_none());
    }

    #[test]
    fn post_condition_violation_on_success() {
        let root = Box::new(FixedNode {
            name: "ok".to_string(),
            status: BtStatus::Success,
        });
        let checker = ConditionChecker::new(
            vec![],
            vec![],
            vec![ConditionSpec {
                expression: "{result_valid} == true".to_string(),
                phase: ConditionPhase::Post,
            }],
        );
        let mut runner = SkillRunner::new(root, checker);
        runner.blackboard_mut().set("result_valid", json!(false));

        let result = runner.tick();
        assert!(matches!(result, SkillTickResult::PostViolation { .. }));
    }

    #[test]
    fn skill_tick_result_serde() {
        let result = SkillTickResult::Completed { success: true };
        let json = serde_json::to_string(&result).unwrap();
        let back: SkillTickResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
    }
}
