//! Feedback handler — processes operator `ApprovalOutcome` into concrete actions.

use roz_core::memory::MemoryClass;
use roz_core::session::feedback::{ApprovalOutcome, DenialCategory, Modification};

/// Action to take in response to feedback.
pub enum FeedbackAction {
    InjectDenialContext { category: DenialCategory, reason: String },
    WriteMemory { class: MemoryClass, fact: String },
    UpdateParameters { modifications: Vec<Modification> },
    ProceedWithApproved { steps: Vec<u32> },
}

/// Processes operator feedback into concrete actions.
pub struct FeedbackHandler;

impl FeedbackHandler {
    /// Convert an `ApprovalOutcome` into zero or more `FeedbackAction`s.
    pub fn process(outcome: &ApprovalOutcome) -> Vec<FeedbackAction> {
        match outcome {
            ApprovalOutcome::Approved => vec![],
            ApprovalOutcome::Denied { reason, category } => {
                let mut actions = vec![];
                if let Some(cat) = category {
                    actions.push(FeedbackAction::InjectDenialContext {
                        category: cat.clone(),
                        reason: reason.as_deref().unwrap_or("no reason given").into(),
                    });
                    // Safety-related denials become memory entries
                    if matches!(cat, DenialCategory::SafetyConcern) {
                        actions.push(FeedbackAction::WriteMemory {
                            class: MemoryClass::Safety,
                            fact: format!(
                                "Operator denied action due to safety concern: {}",
                                reason.as_deref().unwrap_or("unspecified")
                            ),
                        });
                    }
                }
                actions
            }
            ApprovalOutcome::Modified { modifications } => {
                vec![FeedbackAction::UpdateParameters {
                    modifications: modifications.clone(),
                }]
            }
            ApprovalOutcome::PartialApproval { approved_steps, .. } => {
                vec![FeedbackAction::ProceedWithApproved {
                    steps: approved_steps.clone(),
                }]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use roz_core::session::feedback::Modification;

    use super::*;

    #[test]
    fn approved_produces_no_actions() {
        let actions = FeedbackHandler::process(&ApprovalOutcome::Approved);
        assert!(actions.is_empty());
    }

    #[test]
    fn denied_safety_produces_memory_write() {
        let outcome = ApprovalOutcome::Denied {
            reason: Some("arm too close to human".into()),
            category: Some(DenialCategory::SafetyConcern),
        };
        let actions = FeedbackHandler::process(&outcome);
        assert_eq!(actions.len(), 2, "expected InjectDenialContext + WriteMemory");

        assert!(
            matches!(actions[0], FeedbackAction::InjectDenialContext { .. }),
            "first action must be InjectDenialContext"
        );
        match &actions[0] {
            FeedbackAction::InjectDenialContext { category, reason } => {
                assert!(matches!(category, DenialCategory::SafetyConcern));
                assert_eq!(reason, "arm too close to human");
            }
            _ => unreachable!(),
        }

        assert!(
            matches!(actions[1], FeedbackAction::WriteMemory { .. }),
            "second action must be WriteMemory"
        );
        match &actions[1] {
            FeedbackAction::WriteMemory { class, fact } => {
                assert!(matches!(class, MemoryClass::Safety));
                assert!(fact.contains("arm too close to human"), "fact should include reason");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn denied_other_produces_context_only() {
        let outcome = ApprovalOutcome::Denied {
            reason: Some("wrong joint selected".into()),
            category: Some(DenialCategory::IncorrectPlan),
        };
        let actions = FeedbackHandler::process(&outcome);
        assert_eq!(
            actions.len(),
            1,
            "IncorrectPlan should only produce InjectDenialContext"
        );
        assert!(matches!(actions[0], FeedbackAction::InjectDenialContext { .. }));
    }

    #[test]
    fn modified_produces_update() {
        let outcome = ApprovalOutcome::Modified {
            modifications: vec![Modification {
                field: "velocity".into(),
                old_value: "0.5".into(),
                new_value: "0.2".into(),
                reason: Some("too fast".into()),
            }],
        };
        let actions = FeedbackHandler::process(&outcome);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            FeedbackAction::UpdateParameters { modifications } => {
                assert_eq!(modifications.len(), 1);
                assert_eq!(modifications[0].field, "velocity");
                assert_eq!(modifications[0].new_value, "0.2");
            }
            _ => panic!("expected UpdateParameters"),
        }
    }

    #[test]
    fn partial_produces_proceed() {
        let outcome = ApprovalOutcome::PartialApproval {
            approved_steps: vec![0, 1],
            denied_steps: vec![2, 3],
        };
        let actions = FeedbackHandler::process(&outcome);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            FeedbackAction::ProceedWithApproved { steps } => {
                assert_eq!(steps, &vec![0, 1]);
            }
            _ => panic!("expected ProceedWithApproved"),
        }
    }

    #[test]
    fn denied_no_category_produces_no_actions() {
        // Without a category, no context to inject
        let outcome = ApprovalOutcome::Denied {
            reason: Some("not now".into()),
            category: None,
        };
        let actions = FeedbackHandler::process(&outcome);
        assert!(
            actions.is_empty(),
            "denied without category produces no actions (no category to inject)"
        );
    }
}
