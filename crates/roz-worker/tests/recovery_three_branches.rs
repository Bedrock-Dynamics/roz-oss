//! Phase 24 FS-03 resume-gate decision-matrix integration test.
//!
//! Covers the three recovery-decision branches explicitly (ROADMAP SC#5),
//! with each SafeStateWait sub-case surfaced as its own named test per
//! 24-RESEARCH §Pitfall 7 (fewer than 3 distinct SafeStateWait cases is
//! inadequate coverage):
//!
//! - Resume from checkpoint (happy path)
//! - SafeStateWait — physical state ambiguous
//! - SafeStateWait — checkpoint stale (> 1 h)
//! - SafeStateWait — checkpoint missing entirely
//! - Abort — no task was in progress
//!
//! Plus two edge-case assertions that anchor the boundary behaviour of the
//! D-11 predicate (joint-only physical_ok path + exact-3600 s is stale).

use roz_core::edge::recovery::{CrashState, RecoveryStrategy};
use roz_core::session::event::SessionEvent;
use roz_worker::recovery::{RESUME_AGE_LIMIT_SECS, decide_recovery, emit_recovery_pending};

fn resume_eligible(now_unix: i64) -> CrashState {
    CrashState {
        joint_positions: Some(vec![0.0, 1.0, 0.5]),
        brakes_engaged: true,
        mid_action: true,
        task_id: Some("task-resume".into()),
        last_wal_seq: Some(42),
        last_checkpoint_id: Some("ck-fresh".into()),
        last_checkpoint_ts_unix: Some(now_unix - 60), // 60 s old
    }
}

/// SC#5 Branch 1: all predicates satisfied → ResumeFromCheckpoint.
#[test]
fn branch_resume_from_checkpoint() {
    let now = 1_700_000_000;
    let state = resume_eligible(now);
    let decision = decide_recovery(&state, now);
    assert_eq!(decision.strategy, RecoveryStrategy::ResumeFromCheckpoint);
    assert!(decision.reason.contains("satisfied"), "reason: {}", decision.reason);
}

/// SC#5 Branch 2a: brakes off AND joint positions unknown → SafeStateWait +
/// RecoveryPending.
#[test]
fn branch_safe_state_wait_physical_ambiguous() {
    let now = 1_700_000_000;
    let mut state = resume_eligible(now);
    state.brakes_engaged = false;
    state.joint_positions = None;
    let decision = decide_recovery(&state, now);
    assert_eq!(decision.strategy, RecoveryStrategy::SafeStateWait);
    assert!(decision.reason.contains("physical_ok=false"));
    // Per D-09, caller emits RecoveryPending session event.
    let ev = emit_recovery_pending(&state, &decision);
    assert!(matches!(ev, SessionEvent::RecoveryPending { .. }));
}

/// SC#5 Branch 2b: checkpoint timestamp older than the 1 h limit →
/// SafeStateWait + RecoveryPending carrying the stale-age reason.
#[test]
fn branch_safe_state_wait_stale_checkpoint() {
    let now = 1_700_000_000;
    let mut state = resume_eligible(now);
    // 1 s past the 1 h limit.
    state.last_checkpoint_ts_unix = Some(now - RESUME_AGE_LIMIT_SECS - 1);
    let decision = decide_recovery(&state, now);
    assert_eq!(decision.strategy, RecoveryStrategy::SafeStateWait);
    assert!(decision.reason.contains("age_ok=false"));
    match emit_recovery_pending(&state, &decision) {
        SessionEvent::RecoveryPending { reason, .. } => {
            assert!(reason.contains("age_ok=false"));
        }
        other => panic!("expected RecoveryPending, got {other:?}"),
    }
}

/// SC#5 Branch 2c: no checkpoint at all → SafeStateWait. This sub-case
/// exists because Pitfall 7 flags "fewer than 3 distinct SafeStateWait
/// cases" as inadequate coverage.
#[test]
fn branch_safe_state_wait_missing_checkpoint() {
    let now = 1_700_000_000;
    let mut state = resume_eligible(now);
    state.last_checkpoint_id = None;
    state.last_checkpoint_ts_unix = None;
    let decision = decide_recovery(&state, now);
    assert_eq!(decision.strategy, RecoveryStrategy::SafeStateWait);
    assert!(decision.reason.contains("checkpoint_ok=false"));
}

/// SC#5 Branch 3: mid_action=false → Abort (nothing to recover).
#[test]
fn branch_abort_no_task_in_progress() {
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

/// Edge case: physical-ok via joint_positions only (brakes off but joints
/// known) → resume. D-11 predicate is `brakes_engaged OR joint_positions`.
#[test]
fn branch_resume_via_joint_positions_only() {
    let now = 1_700_000_000;
    let mut state = resume_eligible(now);
    state.brakes_engaged = false;
    // joint_positions stays Some — physical_ok is OR of the two, so this
    // branch still resumes.
    let decision = decide_recovery(&state, now);
    assert_eq!(decision.strategy, RecoveryStrategy::ResumeFromCheckpoint);
}

/// Edge case: checkpoint exactly `RESUME_AGE_LIMIT_SECS` old is considered
/// stale by the strict `<` gate.
#[test]
fn branch_boundary_age_at_limit_is_stale() {
    let now = 1_700_000_000;
    let mut state = resume_eligible(now);
    state.last_checkpoint_ts_unix = Some(now - RESUME_AGE_LIMIT_SECS);
    let decision = decide_recovery(&state, now);
    assert_eq!(decision.strategy, RecoveryStrategy::SafeStateWait);
}
