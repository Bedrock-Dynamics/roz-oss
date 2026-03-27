use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::conditions::{ConditionResult, ConditionSpec};
use roz_core::bt::eval::evaluate_condition;

/// Integrates condition evaluation into the BT tick cycle.
pub struct ConditionChecker {
    pre: Vec<ConditionSpec>,
    hold: Vec<ConditionSpec>,
    post: Vec<ConditionSpec>,
}

/// Result of a condition check phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    AllSatisfied,
    Violated { condition: String, reason: String },
}

impl ConditionChecker {
    pub const fn new(pre: Vec<ConditionSpec>, hold: Vec<ConditionSpec>, post: Vec<ConditionSpec>) -> Self {
        Self { pre, hold, post }
    }

    /// Check pre-conditions before the first tick.
    pub fn check_pre(&self, bb: &Blackboard) -> CheckResult {
        Self::check_conditions(&self.pre, bb)
    }

    /// Check hold-conditions every tick.
    pub fn check_hold(&self, bb: &Blackboard) -> CheckResult {
        Self::check_conditions(&self.hold, bb)
    }

    /// Check post-conditions after completion.
    pub fn check_post(&self, bb: &Blackboard) -> CheckResult {
        Self::check_conditions(&self.post, bb)
    }

    fn check_conditions(conditions: &[ConditionSpec], bb: &Blackboard) -> CheckResult {
        for cond in conditions {
            if let ConditionResult::Violated { reason } = evaluate_condition(&cond.expression, bb) {
                return CheckResult::Violated {
                    condition: cond.expression.clone(),
                    reason,
                };
            }
        }
        CheckResult::AllSatisfied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::bt::conditions::ConditionPhase;
    use serde_json::json;

    fn make_spec(expr: &str, phase: ConditionPhase) -> ConditionSpec {
        ConditionSpec {
            expression: expr.to_string(),
            phase,
        }
    }

    #[test]
    fn pre_conditions_all_satisfied() {
        let checker = ConditionChecker::new(vec![make_spec("{ready} == true", ConditionPhase::Pre)], vec![], vec![]);
        let mut bb = Blackboard::new();
        bb.set("ready", json!(true));

        assert_eq!(checker.check_pre(&bb), CheckResult::AllSatisfied);
    }

    #[test]
    fn pre_conditions_violated() {
        let checker = ConditionChecker::new(vec![make_spec("{ready} == true", ConditionPhase::Pre)], vec![], vec![]);
        let mut bb = Blackboard::new();
        bb.set("ready", json!(false));

        assert!(matches!(checker.check_pre(&bb), CheckResult::Violated { .. }));
    }

    #[test]
    fn hold_conditions_checked() {
        let checker = ConditionChecker::new(vec![], vec![make_spec("{velocity} < 10", ConditionPhase::Hold)], vec![]);
        let mut bb = Blackboard::new();
        bb.set("velocity", json!(5.0));
        assert_eq!(checker.check_hold(&bb), CheckResult::AllSatisfied);

        bb.set("velocity", json!(15.0));
        assert!(matches!(checker.check_hold(&bb), CheckResult::Violated { .. }));
    }

    #[test]
    fn post_conditions_checked() {
        let checker = ConditionChecker::new(
            vec![],
            vec![],
            vec![make_spec("{completed} == true", ConditionPhase::Post)],
        );
        let mut bb = Blackboard::new();
        bb.set("completed", json!(true));

        assert_eq!(checker.check_post(&bb), CheckResult::AllSatisfied);
    }

    #[test]
    fn empty_conditions_always_satisfied() {
        let checker = ConditionChecker::new(vec![], vec![], vec![]);
        let bb = Blackboard::new();

        assert_eq!(checker.check_pre(&bb), CheckResult::AllSatisfied);
        assert_eq!(checker.check_hold(&bb), CheckResult::AllSatisfied);
        assert_eq!(checker.check_post(&bb), CheckResult::AllSatisfied);
    }

    #[test]
    fn multiple_conditions_first_violation_wins() {
        let checker = ConditionChecker::new(
            vec![
                make_spec("{a} == true", ConditionPhase::Pre),
                make_spec("{b} == true", ConditionPhase::Pre),
            ],
            vec![],
            vec![],
        );
        let mut bb = Blackboard::new();
        bb.set("a", json!(false));
        bb.set("b", json!(true));

        if let CheckResult::Violated { condition, .. } = checker.check_pre(&bb) {
            assert_eq!(condition, "{a} == true");
        } else {
            panic!("expected Violated");
        }
    }

    #[test]
    fn missing_key_violates_condition() {
        let checker = ConditionChecker::new(
            vec![make_spec("{missing} == true", ConditionPhase::Pre)],
            vec![],
            vec![],
        );
        let bb = Blackboard::new();

        assert!(matches!(checker.check_pre(&bb), CheckResult::Violated { .. }));
    }

    #[test]
    fn hold_condition_with_numeric_comparison() {
        let checker = ConditionChecker::new(
            vec![],
            vec![
                make_spec("{temp} < 100", ConditionPhase::Hold),
                make_spec("{pressure} >= 0", ConditionPhase::Hold),
            ],
            vec![],
        );
        let mut bb = Blackboard::new();
        bb.set("temp", json!(50));
        bb.set("pressure", json!(1.0));

        assert_eq!(checker.check_hold(&bb), CheckResult::AllSatisfied);
    }
}
