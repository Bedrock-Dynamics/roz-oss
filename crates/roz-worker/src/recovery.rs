//! Recovery coordinator for crash restart (FS-03 D-11).
//!
//! On startup after a crash, reads the WAL to determine what was in
//! progress and decides whether to resume, hold in a safe state, or abort.

use roz_core::edge::recovery::{CrashState, DecisionSource, RecoveryDecision, RecoveryStrategy};
use roz_core::session::event::SessionEvent;

/// Max checkpoint age for resume eligibility (D-11: 3600 s = 1 h).
pub const RESUME_AGE_LIMIT_SECS: i64 = 3600;

/// Assess crash state and decide recovery strategy per D-11.
///
/// Resume iff:
/// ```text
/// (brakes_engaged OR joint_positions.is_some())
///   AND last_wal_seq.is_some()
///   AND last_checkpoint_id.is_some()
///   AND checkpoint_age < 3600
///   AND mid_action == true
///   AND task_id.is_some()
/// ```
///
/// Any failure → `SafeStateWait`. `Abort` is reserved for the specific case
/// where no task was in progress at crash time.
///
/// # Arguments
///
/// - `state`: physical + WAL state gathered on boot
/// - `now_unix_secs`: current wall-clock seconds (callers pass
///   `chrono::Utc::now().timestamp()`).
#[must_use]
pub fn decide_recovery(state: &CrashState, now_unix_secs: i64) -> RecoveryDecision {
    // Branch 3: No task in progress → Abort.
    if !state.mid_action || state.task_id.is_none() {
        return RecoveryDecision {
            decided_by: DecisionSource::Robot,
            strategy: RecoveryStrategy::Abort,
            reason: "No task was in progress at crash time".to_string(),
        };
    }

    let physical_ok = state.brakes_engaged || state.joint_positions.is_some();
    let wal_ok = state.last_wal_seq.is_some();
    let checkpoint_ok = state.last_checkpoint_id.is_some();
    let age_ok = state
        .last_checkpoint_ts_unix
        .is_some_and(|ts| now_unix_secs.saturating_sub(ts) < RESUME_AGE_LIMIT_SECS);

    if physical_ok && wal_ok && checkpoint_ok && age_ok {
        return RecoveryDecision {
            decided_by: DecisionSource::Robot,
            strategy: RecoveryStrategy::ResumeFromCheckpoint,
            reason: "All resume predicates satisfied".to_string(),
        };
    }

    // SafeStateWait with detailed reason for operator visibility (D-09).
    let reason = format!(
        "resume predicates failed: physical_ok={physical_ok} wal_ok={wal_ok} \
         checkpoint_ok={checkpoint_ok} age_ok={age_ok}",
    );
    RecoveryDecision {
        decided_by: DecisionSource::Robot,
        strategy: RecoveryStrategy::SafeStateWait,
        reason,
    }
}

/// Produce the `SessionEvent::RecoveryPending` variant that callers emit
/// whenever [`decide_recovery`] returns [`RecoveryStrategy::SafeStateWait`]
/// (D-09 — session-event only, no NATS subject).
///
/// Callers should only invoke this helper on the SafeStateWait branch; the
/// function is side-effect free and does not publish on its own.
#[must_use]
pub fn emit_recovery_pending(state: &CrashState, decision: &RecoveryDecision) -> SessionEvent {
    SessionEvent::RecoveryPending {
        task_id: state.task_id.clone().unwrap_or_default(),
        checkpoint_id: state.last_checkpoint_id.clone().unwrap_or_default(),
        reason: decision.reason.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crash_state_resume_eligible(now_unix: i64) -> CrashState {
        CrashState {
            joint_positions: Some(vec![0.0, 1.0, 0.5]),
            brakes_engaged: true,
            mid_action: true,
            task_id: Some("task-1".into()),
            last_wal_seq: Some(42),
            last_checkpoint_id: Some("ck-1".into()),
            last_checkpoint_ts_unix: Some(now_unix - 60), // 1 min old — well within 1 h
        }
    }

    #[test]
    fn no_task_aborts() {
        let now = 1_700_000_000;
        let state = CrashState {
            joint_positions: None,
            brakes_engaged: false,
            mid_action: false,
            task_id: None,
            last_wal_seq: None,
            last_checkpoint_id: None,
            last_checkpoint_ts_unix: None,
        };
        let decision = decide_recovery(&state, now);
        assert_eq!(decision.strategy, RecoveryStrategy::Abort);
    }

    #[test]
    fn brakes_engaged_resumes() {
        let now = 1_700_000_000;
        let state = crash_state_resume_eligible(now);
        let decision = decide_recovery(&state, now);
        assert_eq!(decision.strategy, RecoveryStrategy::ResumeFromCheckpoint);
    }

    #[test]
    fn ambiguous_state_waits() {
        let now = 1_700_000_000;
        let state = CrashState {
            joint_positions: None,
            brakes_engaged: false,
            mid_action: true,
            task_id: Some("task-2".to_string()),
            last_wal_seq: Some(10),
            last_checkpoint_id: None,
            last_checkpoint_ts_unix: None,
        };
        let decision = decide_recovery(&state, now);
        assert_eq!(decision.strategy, RecoveryStrategy::SafeStateWait);
    }

    #[test]
    fn resume_happy_path() {
        let now = 1_700_000_000;
        let s = crash_state_resume_eligible(now);
        let d = decide_recovery(&s, now);
        assert_eq!(d.strategy, RecoveryStrategy::ResumeFromCheckpoint);
    }

    #[test]
    fn abort_when_no_task() {
        let now = 1_700_000_000;
        let mut s = crash_state_resume_eligible(now);
        s.mid_action = false;
        s.task_id = None;
        assert_eq!(decide_recovery(&s, now).strategy, RecoveryStrategy::Abort);
    }

    #[test]
    fn safe_state_wait_when_physical_ambiguous() {
        let now = 1_700_000_000;
        let mut s = crash_state_resume_eligible(now);
        s.brakes_engaged = false;
        s.joint_positions = None; // both physical predicates fail
        let d = decide_recovery(&s, now);
        assert_eq!(d.strategy, RecoveryStrategy::SafeStateWait);
        assert!(d.reason.contains("physical_ok=false"));
    }

    #[test]
    fn safe_state_wait_when_checkpoint_stale() {
        let now = 1_700_000_000;
        let mut s = crash_state_resume_eligible(now);
        s.last_checkpoint_ts_unix = Some(now - 3700); // 3700 s = 61.6 min > 1 h
        let d = decide_recovery(&s, now);
        assert_eq!(d.strategy, RecoveryStrategy::SafeStateWait);
        assert!(d.reason.contains("age_ok=false"));
    }

    #[test]
    fn safe_state_wait_when_checkpoint_missing() {
        let now = 1_700_000_000;
        let mut s = crash_state_resume_eligible(now);
        s.last_checkpoint_id = None;
        let d = decide_recovery(&s, now);
        assert_eq!(d.strategy, RecoveryStrategy::SafeStateWait);
        assert!(d.reason.contains("checkpoint_ok=false"));
    }

    #[test]
    fn emit_recovery_pending_carries_task_checkpoint_reason() {
        let now = 1_700_000_000;
        let mut s = crash_state_resume_eligible(now);
        s.last_checkpoint_id = None;
        s.last_checkpoint_ts_unix = None;
        let d = decide_recovery(&s, now);
        let ev = emit_recovery_pending(&s, &d);
        match ev {
            SessionEvent::RecoveryPending {
                task_id,
                checkpoint_id,
                reason,
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(checkpoint_id, ""); // was None → default
                assert!(reason.contains("checkpoint_ok=false"));
            }
            other => panic!("expected RecoveryPending, got {other:?}"),
        }
    }
}
