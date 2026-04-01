//! Operator feedback and interaction types.

use serde::{Deserialize, Serialize};

/// Why an action was denied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialCategory {
    SafetyConcern,
    IncorrectPlan,
    WrongTarget,
    NotNow,
    NeedsMoreInfo,
    Other,
}

/// A modification to a proposed action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modification {
    pub field: String,
    pub old_value: String,
    pub new_value: String,
    pub reason: Option<String>,
}

/// The outcome of an approval request. Goes beyond binary approve/deny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalOutcome {
    Approved,
    Denied {
        reason: Option<String>,
        category: Option<DenialCategory>,
    },
    Modified {
        modifications: Vec<Modification>,
    },
    PartialApproval {
        approved_steps: Vec<u32>,
        denied_steps: Vec<u32>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_approved_serde() {
        let outcome = ApprovalOutcome::Approved;
        let json = serde_json::to_string(&outcome).unwrap();
        let back: ApprovalOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
        assert!(json.contains("\"approved\""));
    }

    #[test]
    fn approval_denied_with_reason_serde() {
        let outcome = ApprovalOutcome::Denied {
            reason: Some("too close to table edge".into()),
            category: Some(DenialCategory::SafetyConcern),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: ApprovalOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }

    #[test]
    fn approval_modified_serde() {
        let outcome = ApprovalOutcome::Modified {
            modifications: vec![Modification {
                field: "velocity".into(),
                old_value: "0.5".into(),
                new_value: "0.2".into(),
                reason: Some("too fast near human".into()),
            }],
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: ApprovalOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }

    #[test]
    fn approval_partial_serde() {
        let outcome = ApprovalOutcome::PartialApproval {
            approved_steps: vec![0, 1, 2],
            denied_steps: vec![3],
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: ApprovalOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }

    #[test]
    fn all_denial_categories_serde() {
        let cats = vec![
            DenialCategory::SafetyConcern,
            DenialCategory::IncorrectPlan,
            DenialCategory::WrongTarget,
            DenialCategory::NotNow,
            DenialCategory::NeedsMoreInfo,
            DenialCategory::Other,
        ];
        for c in cats {
            let json = serde_json::to_string(&c).unwrap();
            let back: DenialCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(c, back);
        }
    }
}
