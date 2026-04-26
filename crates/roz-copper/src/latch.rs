//! FW-05 — Latched e-stop state machine (IEC 60204-1 + EN ISO 13849-1).
//!
//! State machine (see plan diagram for full graph):
//!   Run         -> Latched      : force_estop asserted (any source)
//!   Latched     -> AwaitingAck  : signed safety.estop_ack.{worker_id} arrives
//!   AwaitingAck -> ZeroVerified : N=10 consecutive zero-motion ticks AND sensor frame present
//!   ZeroVerified -> Run         : signed safety.resume.{worker_id} arrives
//!   Latched     -X-> Run         : FORBIDDEN per IEC 60204-1 (no auto-rearm)
//!
//! WAL-authoritative boot: on restart, load_latch_state() drives initial state.
//! Sensor-absent: stay in current latched state until motion can be verified.
//!
//! Default = Run so that `ControllerState::default()` keeps working unchanged.

use serde::{Deserialize, Serialize};

/// Number of consecutive zero-motion ticks required to advance from
/// AwaitingAck to ZeroVerified. At 100 Hz this is ~100 ms.
pub const ZERO_VERIFY_TICK_COUNT: u32 = 10;

/// Latched e-stop state. Auto-rearm is forbidden per IEC 60204-1.
/// Default = Run so `ControllerState::default()` keeps working unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatchState {
    /// Normal operation — controller runs and emits commands.
    #[default]
    Run,
    /// Force-estop asserted — emit per-channel-kind zero each tick.
    /// Sticky: requires explicit AckEstop to advance.
    Latched,
    /// Operator acknowledged the e-stop; counting consecutive zero-motion
    /// ticks observed by the sensor. Still emits per-channel-kind zero.
    AwaitingAck,
    /// Zero motion verified for ZERO_VERIFY_TICK_COUNT consecutive ticks
    /// with sensor frame present each tick. Ready for human-driven
    /// ResumeAfterZeroVerified to return to Run.
    ZeroVerified,
}

impl LatchState {
    /// Whether this state should emit explicit zero CommandFrames each tick.
    /// Run does not require zero emission; every other state does.
    #[must_use]
    pub const fn requires_zero_emission(self) -> bool {
        !matches!(self, Self::Run)
    }

    /// Apply an e-stop assertion. Always transitions to Latched (sticky).
    #[must_use]
    pub const fn assert_estop(self) -> Self {
        Self::Latched
    }

    /// Apply a signed AckEstop (distinct from existing Resume).
    /// Only valid from Latched.
    #[must_use]
    pub const fn apply_ack_estop(self) -> Self {
        match self {
            Self::Latched => Self::AwaitingAck,
            other => other,
        }
    }

    /// Advance after N consecutive zero ticks AND sensor frames present.
    /// Only valid from AwaitingAck.
    #[must_use]
    pub const fn apply_zero_verified(self) -> Self {
        match self {
            Self::AwaitingAck => Self::ZeroVerified,
            other => other,
        }
    }

    /// Apply a signed ResumeAfterZeroVerified.
    /// Only valid from ZeroVerified. Distinct from the existing
    /// `ControllerCommand::Resume` — that variant retains its old semantics.
    /// Latched -> Run via this fn is a no-op (returns Latched).
    #[must_use]
    pub const fn apply_resume_after_zero_verified(self) -> Self {
        match self {
            Self::ZeroVerified => Self::Run,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latch_state_default_is_run() {
        assert_eq!(LatchState::default(), LatchState::Run);
    }

    #[test]
    fn latch_state_serde_roundtrip() {
        for state in [
            LatchState::Run,
            LatchState::Latched,
            LatchState::AwaitingAck,
            LatchState::ZeroVerified,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: LatchState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back, "roundtrip failed for {state:?}");
        }
    }

    #[test]
    fn latch_state_serde_uses_snake_case() {
        // Defends against accidental rename_all removal — the WAL JSON depends
        // on these literals for forward compatibility.
        assert_eq!(serde_json::to_string(&LatchState::Run).unwrap(), "\"run\"");
        assert_eq!(serde_json::to_string(&LatchState::Latched).unwrap(), "\"latched\"");
        assert_eq!(
            serde_json::to_string(&LatchState::AwaitingAck).unwrap(),
            "\"awaiting_ack\""
        );
        assert_eq!(
            serde_json::to_string(&LatchState::ZeroVerified).unwrap(),
            "\"zero_verified\""
        );
    }

    #[test]
    fn latch_state_requires_zero_emission() {
        assert!(!LatchState::Run.requires_zero_emission());
        assert!(LatchState::Latched.requires_zero_emission());
        assert!(LatchState::AwaitingAck.requires_zero_emission());
        assert!(LatchState::ZeroVerified.requires_zero_emission());
    }

    #[test]
    fn latch_state_no_auto_rearm() {
        // IEC 60204-1: Latched cannot transition directly to Run via the
        // ResumeAfterZeroVerified command.
        assert_eq!(
            LatchState::Latched.apply_resume_after_zero_verified(),
            LatchState::Latched
        );
        assert_eq!(
            LatchState::AwaitingAck.apply_resume_after_zero_verified(),
            LatchState::AwaitingAck
        );
        // Only ZeroVerified -> Run succeeds.
        assert_eq!(
            LatchState::ZeroVerified.apply_resume_after_zero_verified(),
            LatchState::Run
        );
    }

    #[test]
    fn ack_estop_from_run_is_noop() {
        assert_eq!(LatchState::Run.apply_ack_estop(), LatchState::Run);
    }

    #[test]
    fn ack_estop_only_from_latched() {
        assert_eq!(LatchState::Latched.apply_ack_estop(), LatchState::AwaitingAck);
        assert_eq!(LatchState::AwaitingAck.apply_ack_estop(), LatchState::AwaitingAck);
        assert_eq!(LatchState::ZeroVerified.apply_ack_estop(), LatchState::ZeroVerified);
    }

    #[test]
    fn zero_verified_only_from_awaiting_ack() {
        assert_eq!(LatchState::Latched.apply_zero_verified(), LatchState::Latched);
        assert_eq!(LatchState::Run.apply_zero_verified(), LatchState::Run);
        assert_eq!(LatchState::AwaitingAck.apply_zero_verified(), LatchState::ZeroVerified);
        assert_eq!(LatchState::ZeroVerified.apply_zero_verified(), LatchState::ZeroVerified);
    }

    #[test]
    fn assert_estop_is_sticky() {
        // From any state, assert_estop drops to Latched.
        assert_eq!(LatchState::Run.assert_estop(), LatchState::Latched);
        assert_eq!(LatchState::Latched.assert_estop(), LatchState::Latched);
        assert_eq!(LatchState::AwaitingAck.assert_estop(), LatchState::Latched);
        assert_eq!(LatchState::ZeroVerified.assert_estop(), LatchState::Latched);
    }
}
