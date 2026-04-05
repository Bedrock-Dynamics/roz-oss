//! Controller verification and validation checks.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The status of a verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifierStatus {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "pass")]
    Complete,
    #[serde(rename = "fail")]
    Failed,
    #[serde(rename = "unavailable")]
    Unavailable,
}

impl VerifierStatus {
    /// Stable wire label used for compatibility surfaces that still expose a string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Complete => "pass",
            Self::Failed => "fail",
            Self::Unavailable => "unavailable",
        }
    }

    /// Parse canonical verifier labels.
    #[must_use]
    pub fn from_wire_label(label: &str) -> Option<Self> {
        match label {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "pass" => Some(Self::Complete),
            "fail" => Some(Self::Failed),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// Whether this status is a passing verifier outcome.
    #[must_use]
    pub const fn is_pass(self) -> bool {
        matches!(self, Self::Complete)
    }
}

impl std::fmt::Display for VerifierStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VerifierStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_wire_label(s).ok_or(())
    }
}

impl From<&str> for VerifierStatus {
    fn from(value: &str) -> Self {
        Self::from_wire_label(value).unwrap_or(Self::Unavailable)
    }
}

impl From<String> for VerifierStatus {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
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

    #[test]
    fn verifier_status_rejects_legacy_labels() {
        assert!(serde_json::from_str::<VerifierStatus>("\"complete\"").is_err());
        assert!(serde_json::from_str::<VerifierStatus>("\"failed\"").is_err());
        assert_eq!(VerifierStatus::Complete.to_string(), "pass");
        assert_eq!(VerifierStatus::Failed.to_string(), "fail");
    }
}
