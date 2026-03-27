//! Crash recovery types for edge robots.

use serde::{Deserialize, Serialize};

/// What the robot should do after a crash/restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStrategy {
    ResumeFromCheckpoint,
    RetryFromStart,
    Abort,
    SafeStateWait,
}

/// Physical state of the robot at crash time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashState {
    pub joint_positions: Option<Vec<f64>>,
    pub brakes_engaged: bool,
    pub mid_action: bool,
    pub task_id: Option<String>,
    pub last_wal_seq: Option<i64>,
}

/// Decision made by the recovery coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryDecision {
    pub decided_by: DecisionSource,
    pub strategy: RecoveryStrategy,
    pub reason: String,
}

/// Who decides the recovery strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    Robot,
    Cloud,
    Operator,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_strategy_serde_roundtrip() {
        let strategy = RecoveryStrategy::ResumeFromCheckpoint;
        let json = serde_json::to_string(&strategy).unwrap();
        let parsed: RecoveryStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, RecoveryStrategy::ResumeFromCheckpoint);
    }
}
