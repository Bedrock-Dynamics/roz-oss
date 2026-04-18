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

    #[test]
    fn crash_state_defaults_new_fields_to_none() {
        // JSON missing the new fields must still deserialize cleanly so
        // worker WAL payloads persisted by pre-24-04 binaries survive the
        // upgrade.
        let legacy = serde_json::json!({
            "joint_positions": null,
            "brakes_engaged": false,
            "mid_action": false,
            "task_id": null,
            "last_wal_seq": null
        });
        let state: CrashState = serde_json::from_value(legacy).unwrap();
        assert!(state.last_checkpoint_id.is_none());
        assert!(state.last_checkpoint_ts_unix.is_none());
    }

    #[test]
    fn crash_state_serde_roundtrip_with_new_fields() {
        let s = CrashState {
            joint_positions: Some(vec![0.0, 1.0]),
            brakes_engaged: true,
            mid_action: true,
            task_id: Some("task-1".into()),
            last_wal_seq: Some(42),
            last_checkpoint_id: Some("ck-abc".into()),
            last_checkpoint_ts_unix: Some(1_700_000_000),
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: CrashState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.last_checkpoint_id.as_deref(), Some("ck-abc"));
        assert_eq!(parsed.last_checkpoint_ts_unix, Some(1_700_000_000));
    }
}
