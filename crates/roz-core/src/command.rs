use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::RozError;

// ---------------------------------------------------------------------------
// MotorCommand
// ---------------------------------------------------------------------------

/// Control mode for motor commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    #[default]
    Velocity,
    Position,
}

/// Motor command output from a controller, consumed by actuators.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MotorCommand {
    /// Per-joint velocity commands in rad/s.
    pub joint_velocities: Vec<f64>,
    /// Per-joint position targets in radians (used in Position mode).
    #[serde(default)]
    pub joint_positions: Option<Vec<f64>>,
    /// Which control mode the actuator should use.
    #[serde(default)]
    pub control_mode: ControlMode,
}

impl MotorCommand {
    /// Zero-velocity command (safe stop).
    pub fn zero(num_joints: usize) -> Self {
        Self {
            joint_velocities: vec![0.0; num_joints],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        }
    }
}

// ---------------------------------------------------------------------------
// CommandState
// ---------------------------------------------------------------------------

/// Every command progresses through a state machine.
/// Terminal states: `Completed`, `Failed`, `Aborted`, `TimedOut`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandState {
    Accepted,
    Started,
    Progress,
    Completed,
    Failed,
    Aborted,
    TimedOut,
}

impl CommandState {
    /// Returns `true` when the command has reached a final state and no
    /// further transitions are valid.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Aborted | Self::TimedOut)
    }

    /// Attempt a state transition driven by `event`.
    ///
    /// Returns the new state on success, or `RozError::InvalidTransition`
    /// when the (state, event) pair is not part of the valid transition table.
    #[allow(clippy::match_same_arms)]
    pub fn transition(&self, event: CommandEvent) -> Result<Self, RozError> {
        use CommandEvent::{Abort, Complete, Fail, ReportProgress, Start, Timeout};
        use CommandState::{Aborted, Accepted, Completed, Failed, Progress, Started, TimedOut};

        let next = match (*self, event) {
            // From Accepted
            (Accepted, Start) => Started,

            // From Started
            (Started, ReportProgress) => Progress,
            (Started, Complete) => Completed,
            (Started, Fail) => Failed,
            (Started, Abort) => Aborted,
            (Started, Timeout) => TimedOut,

            // From Progress (same transitions as Started)
            (Progress, ReportProgress) => Progress,
            (Progress, Complete) => Completed,
            (Progress, Fail) => Failed,
            (Progress, Abort) => Aborted,
            (Progress, Timeout) => TimedOut,

            // Everything else is invalid
            (state, evt) => {
                return Err(RozError::InvalidTransition {
                    from: format!("{state:?}"),
                    to: format!("{evt:?}"),
                });
            }
        };

        Ok(next)
    }
}

impl std::fmt::Display for CommandState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

// ---------------------------------------------------------------------------
// CommandEvent
// ---------------------------------------------------------------------------

/// Events that drive transitions in the command state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandEvent {
    Start,
    ReportProgress,
    Complete,
    Fail,
    Abort,
    Timeout,
}

impl std::fmt::Display for CommandEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

/// A command issued to a host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub id: Uuid,
    pub host_id: String,
    pub command: String,
    pub idempotency_key: String,
    pub state: CommandState,
    pub issued_at: DateTime<Utc>,
    pub acked_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// CommandFrame
// ---------------------------------------------------------------------------

/// Robot-agnostic command frame: flat vector of channel values.
///
/// Channel semantics are defined by the legacy runtime channel manifest for the robot.
/// The index in `values` corresponds 1-to-1 with the index in
/// that manifest's command list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandFrame {
    /// Per-channel command values; length must equal the legacy manifest command count.
    pub values: Vec<f64>,
}

impl CommandFrame {
    /// Create an all-zeros frame with `n` channels (safe stop / no-op command).
    pub fn zero(n: usize) -> Self {
        Self { values: vec![0.0; n] }
    }

    /// Create a frame with each channel set to its manifest default.
    ///
    /// Use this for safe-stop instead of `zero()` on position-controlled robots,
    /// where zero means "go to mechanical origin" rather than "stop."
    pub fn from_defaults(manifest: &crate::channels::LegacyRuntimeManifest) -> Self {
        Self {
            values: manifest.commands.iter().map(|c| c.default).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Valid transitions
    // -----------------------------------------------------------------------

    #[test]
    fn accepted_start_to_started() {
        let next = CommandState::Accepted.transition(CommandEvent::Start).unwrap();
        assert_eq!(next, CommandState::Started);
    }

    #[test]
    fn started_report_progress_to_progress() {
        let next = CommandState::Started.transition(CommandEvent::ReportProgress).unwrap();
        assert_eq!(next, CommandState::Progress);
    }

    #[test]
    fn started_complete_to_completed() {
        let next = CommandState::Started.transition(CommandEvent::Complete).unwrap();
        assert_eq!(next, CommandState::Completed);
    }

    #[test]
    fn started_fail_to_failed() {
        let next = CommandState::Started.transition(CommandEvent::Fail).unwrap();
        assert_eq!(next, CommandState::Failed);
    }

    #[test]
    fn started_abort_to_aborted() {
        let next = CommandState::Started.transition(CommandEvent::Abort).unwrap();
        assert_eq!(next, CommandState::Aborted);
    }

    #[test]
    fn started_timeout_to_timedout() {
        let next = CommandState::Started.transition(CommandEvent::Timeout).unwrap();
        assert_eq!(next, CommandState::TimedOut);
    }

    #[test]
    fn progress_report_progress_to_progress() {
        let next = CommandState::Progress.transition(CommandEvent::ReportProgress).unwrap();
        assert_eq!(next, CommandState::Progress);
    }

    #[test]
    fn progress_complete_to_completed() {
        let next = CommandState::Progress.transition(CommandEvent::Complete).unwrap();
        assert_eq!(next, CommandState::Completed);
    }

    #[test]
    fn progress_fail_to_failed() {
        let next = CommandState::Progress.transition(CommandEvent::Fail).unwrap();
        assert_eq!(next, CommandState::Failed);
    }

    #[test]
    fn progress_abort_to_aborted() {
        let next = CommandState::Progress.transition(CommandEvent::Abort).unwrap();
        assert_eq!(next, CommandState::Aborted);
    }

    #[test]
    fn progress_timeout_to_timedout() {
        let next = CommandState::Progress.transition(CommandEvent::Timeout).unwrap();
        assert_eq!(next, CommandState::TimedOut);
    }

    // -----------------------------------------------------------------------
    // Invalid transitions
    // -----------------------------------------------------------------------

    #[test]
    fn accepted_complete_is_invalid() {
        let result = CommandState::Accepted.transition(CommandEvent::Complete);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Accepted"), "error should mention source state: {msg}");
    }

    #[test]
    fn accepted_fail_is_invalid() {
        let result = CommandState::Accepted.transition(CommandEvent::Fail);
        assert!(result.is_err());
    }

    #[test]
    fn accepted_abort_is_invalid() {
        let result = CommandState::Accepted.transition(CommandEvent::Abort);
        assert!(result.is_err());
    }

    #[test]
    fn accepted_timeout_is_invalid() {
        let result = CommandState::Accepted.transition(CommandEvent::Timeout);
        assert!(result.is_err());
    }

    #[test]
    fn accepted_report_progress_is_invalid() {
        let result = CommandState::Accepted.transition(CommandEvent::ReportProgress);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Terminal states reject ALL events
    // -----------------------------------------------------------------------

    #[test]
    fn terminal_states_reject_all_events() {
        let terminals = [
            CommandState::Completed,
            CommandState::Failed,
            CommandState::Aborted,
            CommandState::TimedOut,
        ];
        let events = [
            CommandEvent::Start,
            CommandEvent::ReportProgress,
            CommandEvent::Complete,
            CommandEvent::Fail,
            CommandEvent::Abort,
            CommandEvent::Timeout,
        ];

        for state in &terminals {
            for event in &events {
                let result = state.transition(*event);
                assert!(
                    result.is_err(),
                    "terminal state {state:?} should reject event {event:?}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // is_terminal()
    // -----------------------------------------------------------------------

    #[test]
    fn is_terminal_returns_true_for_terminal_states() {
        assert!(CommandState::Completed.is_terminal());
        assert!(CommandState::Failed.is_terminal());
        assert!(CommandState::Aborted.is_terminal());
        assert!(CommandState::TimedOut.is_terminal());
    }

    #[test]
    fn is_terminal_returns_false_for_non_terminal_states() {
        assert!(!CommandState::Accepted.is_terminal());
        assert!(!CommandState::Started.is_terminal());
        assert!(!CommandState::Progress.is_terminal());
    }

    // -----------------------------------------------------------------------
    // Serde round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn serde_roundtrip_command_state() {
        let states = [
            CommandState::Accepted,
            CommandState::Started,
            CommandState::Progress,
            CommandState::Completed,
            CommandState::Failed,
            CommandState::Aborted,
            CommandState::TimedOut,
        ];

        for state in &states {
            let json = serde_json::to_string(state).unwrap();
            let deserialized: CommandState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, deserialized, "round-trip failed for {state:?}");
        }
    }

    #[test]
    fn serde_roundtrip_command() {
        let cmd = Command {
            id: Uuid::new_v4(),
            host_id: "host-abc".to_string(),
            command: "restart_service".to_string(),
            idempotency_key: "key-123".to_string(),
            state: CommandState::Accepted,
            issued_at: Utc::now(),
            acked_at: None,
            completed_at: None,
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: Command = serde_json::from_str(&json).unwrap();

        assert_eq!(cmd.id, deserialized.id);
        assert_eq!(cmd.host_id, deserialized.host_id);
        assert_eq!(cmd.command, deserialized.command);
        assert_eq!(cmd.idempotency_key, deserialized.idempotency_key);
        assert_eq!(cmd.state, deserialized.state);
        assert_eq!(cmd.issued_at, deserialized.issued_at);
        assert_eq!(cmd.acked_at, deserialized.acked_at);
        assert_eq!(cmd.completed_at, deserialized.completed_at);
    }

    #[test]
    fn serde_roundtrip_command_with_timestamps() {
        let now = Utc::now();
        let cmd = Command {
            id: Uuid::new_v4(),
            host_id: "host-xyz".to_string(),
            command: "deploy".to_string(),
            idempotency_key: "idem-456".to_string(),
            state: CommandState::Completed,
            issued_at: now,
            acked_at: Some(now),
            completed_at: Some(now),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: Command = serde_json::from_str(&json).unwrap();

        assert_eq!(cmd.state, deserialized.state);
        assert!(deserialized.acked_at.is_some());
        assert!(deserialized.completed_at.is_some());
    }

    // -----------------------------------------------------------------------
    // Idempotent progress reporting
    // -----------------------------------------------------------------------

    #[test]
    fn motor_command_with_control_mode_serde() {
        let cmd = MotorCommand {
            joint_velocities: vec![1.0, -0.5],
            joint_positions: Some(vec![0.5, 1.57]),
            control_mode: ControlMode::Velocity,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: MotorCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.control_mode, ControlMode::Velocity);
        assert_eq!(parsed.joint_positions, Some(vec![0.5, 1.57]));
    }

    #[test]
    fn command_frame_zero() {
        let frame = CommandFrame::zero(6);
        assert_eq!(frame.values.len(), 6);
        assert!(frame.values.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn command_frame_zero_empty() {
        let frame = CommandFrame::zero(0);
        assert!(frame.values.is_empty());
    }

    #[test]
    fn command_frame_serde_roundtrip() {
        let frame = CommandFrame {
            values: vec![1.0, -0.5, 3.14],
        };
        let json = serde_json::to_string(&frame).unwrap();
        let restored: CommandFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, restored);
    }

    #[test]
    fn progress_to_progress_is_idempotent() {
        let state = CommandState::Progress;
        let next = state.transition(CommandEvent::ReportProgress).unwrap();
        assert_eq!(next, CommandState::Progress);

        // Chain multiple progress reports
        let next2 = next.transition(CommandEvent::ReportProgress).unwrap();
        assert_eq!(next2, CommandState::Progress);

        let next3 = next2.transition(CommandEvent::ReportProgress).unwrap();
        assert_eq!(next3, CommandState::Progress);
    }

    #[test]
    fn command_frame_from_defaults_uses_manifest() {
        use crate::channels::{ChannelDescriptor, InterfaceType, LegacyRuntimeManifest};

        let manifest = LegacyRuntimeManifest {
            robot_id: "test".into(),
            robot_class: "test".into(),
            control_rate_hz: 50,
            commands: vec![
                ChannelDescriptor {
                    name: "a".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-1.0, 1.0),
                    default: 0.5, // non-zero default
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
                ChannelDescriptor {
                    name: "b".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "rad/s".into(),
                    limits: (-1.5, 1.5),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
            ],
            states: vec![],
        };
        let frame = CommandFrame::from_defaults(&manifest);
        assert_eq!(frame.values.len(), 2);
        assert!(
            (frame.values[0] - 0.5).abs() < f64::EPSILON,
            "should use channel default 0.5"
        );
        assert!(
            (frame.values[1] - 0.0).abs() < f64::EPSILON,
            "should use channel default 0.0"
        );
    }

    #[test]
    fn command_frame_from_defaults_empty_manifest() {
        let manifest = crate::channels::LegacyRuntimeManifest::default();
        let frame = CommandFrame::from_defaults(&manifest);
        assert!(frame.values.is_empty());
    }
}
