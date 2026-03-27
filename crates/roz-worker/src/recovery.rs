//! Recovery coordinator for crash restart.
//!
//! On startup after a crash, reads the WAL to determine what was in
//! progress and decides whether to resume, retry, or abort.

use roz_core::edge::recovery::{CrashState, DecisionSource, RecoveryDecision, RecoveryStrategy};

/// Assess the crash state and decide recovery strategy.
///
/// Rules:
/// - Mid-action with unknown physical state -> `SafeStateWait` (needs operator)
/// - Mid-action with known state (brakes engaged) -> `ResumeFromCheckpoint`
/// - No task in progress -> `Abort` (nothing to recover)
#[must_use]
pub fn decide_recovery(state: &CrashState) -> RecoveryDecision {
    if !state.mid_action || state.task_id.is_none() {
        return RecoveryDecision {
            decided_by: DecisionSource::Robot,
            strategy: RecoveryStrategy::Abort,
            reason: "No task was in progress at crash time".to_string(),
        };
    }

    if state.brakes_engaged && state.joint_positions.is_some() {
        return RecoveryDecision {
            decided_by: DecisionSource::Robot,
            strategy: RecoveryStrategy::ResumeFromCheckpoint,
            reason: "Brakes engaged, joint positions known — safe to resume".to_string(),
        };
    }

    RecoveryDecision {
        decided_by: DecisionSource::Robot,
        strategy: RecoveryStrategy::SafeStateWait,
        reason: "Physical state ambiguous — waiting for operator".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_task_aborts() {
        let state = CrashState {
            joint_positions: None,
            brakes_engaged: false,
            mid_action: false,
            task_id: None,
            last_wal_seq: None,
        };
        let decision = decide_recovery(&state);
        assert_eq!(decision.strategy, RecoveryStrategy::Abort);
    }

    #[test]
    fn brakes_engaged_resumes() {
        let state = CrashState {
            joint_positions: Some(vec![0.0, 1.0, 0.5]),
            brakes_engaged: true,
            mid_action: true,
            task_id: Some("task-1".to_string()),
            last_wal_seq: Some(42),
        };
        let decision = decide_recovery(&state);
        assert_eq!(decision.strategy, RecoveryStrategy::ResumeFromCheckpoint);
    }

    #[test]
    fn ambiguous_state_waits() {
        let state = CrashState {
            joint_positions: None,
            brakes_engaged: false,
            mid_action: true,
            task_id: Some("task-2".to_string()),
            last_wal_seq: Some(10),
        };
        let decision = decide_recovery(&state);
        assert_eq!(decision.strategy, RecoveryStrategy::SafeStateWait);
    }
}
