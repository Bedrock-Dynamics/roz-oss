//! Controller verification and validation checks.

use serde::{Deserialize, Serialize};

/// The status of a verification check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifierStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Unavailable,
}

/// A single verification failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierFailure {
    pub check_name: String,
    pub reason: String,
    pub severity: FailureSeverity,
}

/// How severe a verification failure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSeverity {
    /// Blocks promotion.
    Critical,
    /// Warning, does not block.
    Warning,
}

/// The verdict from a verifier run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum VerifierVerdict {
    Pass { evidence_summary: String },
    Fail { failures: Vec<VerifierFailure> },
    Partial { passed: Vec<String>, pending: Vec<String> },
    Unavailable { reason: String },
}

impl VerifierVerdict {
    /// Whether the verdict allows promotion.
    #[must_use]
    pub const fn allows_promotion(&self) -> bool {
        matches!(self, Self::Pass { .. })
    }

    /// Whether the verdict has any critical failures.
    #[must_use]
    pub fn has_critical_failures(&self) -> bool {
        match self {
            Self::Fail { failures } => failures.iter().any(|f| f.severity == FailureSeverity::Critical),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_verdict_serde() {
        let v = VerifierVerdict::Pass {
            evidence_summary: "10k ticks, 0 traps, p99 < 1ms".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"pass\""));
        let back: VerifierVerdict = serde_json::from_str(&json).unwrap();
        assert!(back.allows_promotion());
    }

    #[test]
    fn fail_verdict_serde() {
        let v = VerifierVerdict::Fail {
            failures: vec![
                VerifierFailure {
                    check_name: "safety_envelope".into(),
                    reason: "position limit exceeded 3 times".into(),
                    severity: FailureSeverity::Critical,
                },
                VerifierFailure {
                    check_name: "timing".into(),
                    reason: "p99 latency 1.5ms > 1ms budget".into(),
                    severity: FailureSeverity::Warning,
                },
            ],
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: VerifierVerdict = serde_json::from_str(&json).unwrap();
        assert!(!back.allows_promotion());
        assert!(back.has_critical_failures());
    }

    #[test]
    fn fail_with_only_warnings_still_fails() {
        let v = VerifierVerdict::Fail {
            failures: vec![VerifierFailure {
                check_name: "timing".into(),
                reason: "slightly slow".into(),
                severity: FailureSeverity::Warning,
            }],
        };
        assert!(!v.allows_promotion());
        assert!(!v.has_critical_failures());
    }

    #[test]
    fn partial_verdict_serde() {
        let v = VerifierVerdict::Partial {
            passed: vec!["abi_check".into(), "manifest_check".into()],
            pending: vec!["llm_review".into()],
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: VerifierVerdict = serde_json::from_str(&json).unwrap();
        assert!(!back.allows_promotion());
    }

    #[test]
    fn unavailable_verdict_serde() {
        let v = VerifierVerdict::Unavailable {
            reason: "cloud connectivity lost".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: VerifierVerdict = serde_json::from_str(&json).unwrap();
        assert!(!back.allows_promotion());
    }

    #[test]
    fn all_statuses_serde() {
        for s in [
            VerifierStatus::Pending,
            VerifierStatus::Running,
            VerifierStatus::Complete,
            VerifierStatus::Failed,
            VerifierStatus::Unavailable,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: VerifierStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }
}
