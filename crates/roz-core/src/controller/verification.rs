//! Controller verification and validation checks.

use serde::{Deserialize, Serialize};

/// Outcome of a controller verification run.
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum VerifierVerdict {
    /// All checks passed.
    Pass {
        /// Short human-readable summary of evidence (e.g. `"torque ≤ limit on all joints"`).
        evidence_summary: String,
    },
    /// One or more checks failed.
    Fail {
        /// List of failure descriptions, one per failed check.
        failures: Vec<String>,
    },
}
