//! Session-scoped recording triggers driven by blueprint config.

use roz_core::blueprint::RecordingConfig;
use roz_core::session::event::SessionEvent;

/// Decides whether to start/stop recording based on session events.
pub struct RecordingTrigger {
    config: RecordingConfig,
    recording: bool,
}

impl RecordingTrigger {
    pub const fn new(config: RecordingConfig) -> Self {
        Self {
            config,
            recording: false,
        }
    }

    /// Process a session event and return whether recording state changed.
    pub const fn process_event(&mut self, event: &SessionEvent) -> RecordingAction {
        match event {
            SessionEvent::SessionStarted { .. } if self.config.auto_record => {
                self.recording = true;
                RecordingAction::StartRecording
            }
            SessionEvent::SafetyIntervention { .. } if self.config.record_on_safety && !self.recording => {
                self.recording = true;
                RecordingAction::StartRecording
            }
            SessionEvent::SessionCompleted { .. } | SessionEvent::SessionFailed { .. } if self.recording => {
                self.recording = false;
                RecordingAction::StopRecording
            }
            _ => RecordingAction::NoChange,
        }
    }

    pub const fn is_recording(&self) -> bool {
        self.recording
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingAction {
    StartRecording,
    StopRecording,
    NoChange,
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::controller::intervention::InterventionKind;
    use roz_core::session::activity::RuntimeFailureKind;
    use roz_core::session::control::SessionMode;

    fn safety_intervention_event() -> SessionEvent {
        SessionEvent::SafetyIntervention {
            channel: "joint_vel".into(),
            raw_value: 2.0,
            clamped_value: 1.0,
            kind: InterventionKind::VelocityClamp,
            reason: "exceeded limit".into(),
        }
    }

    fn session_started_event() -> SessionEvent {
        SessionEvent::SessionStarted {
            session_id: "sess-1".into(),
            mode: SessionMode::Local,
            blueprint_version: "1.0".into(),
            model_name: None,
            permissions: vec![],
        }
    }

    fn session_completed_event() -> SessionEvent {
        SessionEvent::SessionCompleted {
            summary: "done".into(),
            total_usage: roz_core::session::event::SessionUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
        }
    }

    fn session_failed_event() -> SessionEvent {
        SessionEvent::SessionFailed {
            failure: RuntimeFailureKind::ControllerTrap,
        }
    }

    #[test]
    fn auto_record_starts_on_session_started() {
        let config = RecordingConfig {
            auto_record: true,
            ..RecordingConfig::default()
        };
        let mut trigger = RecordingTrigger::new(config);
        let action = trigger.process_event(&session_started_event());
        assert_eq!(action, RecordingAction::StartRecording);
        assert!(trigger.is_recording());
    }

    #[test]
    fn no_auto_record() {
        let config = RecordingConfig {
            auto_record: false,
            ..RecordingConfig::default()
        };
        let mut trigger = RecordingTrigger::new(config);
        let action = trigger.process_event(&session_started_event());
        assert_eq!(action, RecordingAction::NoChange);
        assert!(!trigger.is_recording());
    }

    #[test]
    fn record_on_safety_intervention() {
        let config = RecordingConfig {
            record_on_safety: true,
            ..RecordingConfig::default()
        };
        let mut trigger = RecordingTrigger::new(config);
        let action = trigger.process_event(&safety_intervention_event());
        assert_eq!(action, RecordingAction::StartRecording);
        assert!(trigger.is_recording());
    }

    #[test]
    fn stops_on_session_completed() {
        let config = RecordingConfig {
            auto_record: true,
            ..RecordingConfig::default()
        };
        let mut trigger = RecordingTrigger::new(config);
        trigger.process_event(&session_started_event());
        let action = trigger.process_event(&session_completed_event());
        assert_eq!(action, RecordingAction::StopRecording);
        assert!(!trigger.is_recording());
    }

    #[test]
    fn stops_on_session_failed() {
        let config = RecordingConfig {
            auto_record: true,
            ..RecordingConfig::default()
        };
        let mut trigger = RecordingTrigger::new(config);
        trigger.process_event(&session_started_event());
        let action = trigger.process_event(&session_failed_event());
        assert_eq!(action, RecordingAction::StopRecording);
        assert!(!trigger.is_recording());
    }

    #[test]
    fn no_double_start_on_safety_when_already_recording() {
        let config = RecordingConfig {
            auto_record: true,
            record_on_safety: true,
            ..RecordingConfig::default()
        };
        let mut trigger = RecordingTrigger::new(config);
        // Start recording via SessionStarted
        trigger.process_event(&session_started_event());
        assert!(trigger.is_recording());
        // Safety event while already recording should be NoChange
        let action = trigger.process_event(&safety_intervention_event());
        assert_eq!(action, RecordingAction::NoChange);
        assert!(trigger.is_recording());
    }
}
