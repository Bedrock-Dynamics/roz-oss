use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// An emergency-stop event emitted by the safety daemon.
///
/// Published to `safety.estop.{worker_id}` when a worker becomes
/// unresponsive, violates a safety guard, or an operator triggers
/// a manual stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EStopEvent {
    pub worker_id: String,
    pub reason: EStopReason,
    pub timestamp: DateTime<Utc>,
}

/// Why the e-stop was triggered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EStopReason {
    /// Worker missed its heartbeat deadline.
    HeartbeatTimeout,
    /// A safety guard was violated during operation.
    SafetyViolation { guard: String, details: String },
    /// An operator (or external system) manually requested the stop.
    ManualTrigger,
    /// The safety daemon's own watchdog timed out.
    WatchdogTimeout,
}

impl EStopEvent {
    /// Create an e-stop event for a worker that missed its heartbeat.
    pub fn heartbeat_timeout(worker_id: &str) -> Self {
        Self {
            worker_id: worker_id.to_owned(),
            reason: EStopReason::HeartbeatTimeout,
            timestamp: Utc::now(),
        }
    }

    /// Create an e-stop event for a safety guard violation.
    pub fn safety_violation(worker_id: &str, guard: &str, details: &str) -> Self {
        Self {
            worker_id: worker_id.to_owned(),
            reason: EStopReason::SafetyViolation {
                guard: guard.to_owned(),
                details: details.to_owned(),
            },
            timestamp: Utc::now(),
        }
    }

    /// Create an e-stop event for a manual operator trigger.
    pub fn manual_trigger(worker_id: &str) -> Self {
        Self {
            worker_id: worker_id.to_owned(),
            reason: EStopReason::ManualTrigger,
            timestamp: Utc::now(),
        }
    }

    /// Serialize this event to JSON bytes suitable for NATS publishing.
    pub fn to_json_bytes(&self) -> Result<bytes::Bytes, serde_json::Error> {
        serde_json::to_vec(self).map(bytes::Bytes::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estop_event_serde_roundtrip() {
        let event = EStopEvent::heartbeat_timeout("worker-42");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: EStopEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.worker_id, parsed.worker_id);
        assert_eq!(event.reason, parsed.reason);
        assert_eq!(event.timestamp, parsed.timestamp);
    }

    #[test]
    fn estop_reason_heartbeat_timeout_serde() {
        let reason = EStopReason::HeartbeatTimeout;
        let json = serde_json::to_string(&reason).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "heartbeat_timeout");

        let parsed: EStopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(reason, parsed);
    }

    #[test]
    fn estop_reason_safety_violation_serde() {
        let reason = EStopReason::SafetyViolation {
            guard: "joint_limit".to_string(),
            details: "elbow exceeded 170 degrees".to_string(),
        };
        let json = serde_json::to_string(&reason).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "safety_violation");
        assert_eq!(value["guard"], "joint_limit");
        assert_eq!(value["details"], "elbow exceeded 170 degrees");

        let parsed: EStopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(reason, parsed);
    }

    #[test]
    fn estop_reason_manual_trigger_serde() {
        let reason = EStopReason::ManualTrigger;
        let json = serde_json::to_string(&reason).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "manual_trigger");

        let parsed: EStopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(reason, parsed);
    }

    #[test]
    fn estop_reason_watchdog_timeout_serde() {
        let reason = EStopReason::WatchdogTimeout;
        let json = serde_json::to_string(&reason).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "watchdog_timeout");

        let parsed: EStopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(reason, parsed);
    }

    #[test]
    fn heartbeat_timeout_constructor_fields() {
        let event = EStopEvent::heartbeat_timeout("worker-99");
        assert_eq!(event.worker_id, "worker-99");
        assert_eq!(event.reason, EStopReason::HeartbeatTimeout);
        // Timestamp should be very recent (within last second).
        let elapsed = Utc::now() - event.timestamp;
        assert!(elapsed.num_seconds() < 2, "timestamp should be recent");
    }

    #[test]
    fn safety_violation_constructor_fields() {
        let event = EStopEvent::safety_violation("arm-1", "velocity_limit", "exceeded 2.0 m/s");
        assert_eq!(event.worker_id, "arm-1");
        match &event.reason {
            EStopReason::SafetyViolation { guard, details } => {
                assert_eq!(guard, "velocity_limit");
                assert_eq!(details, "exceeded 2.0 m/s");
            }
            other => panic!("expected SafetyViolation, got {other:?}"),
        }
    }

    #[test]
    fn manual_trigger_constructor_fields() {
        let event = EStopEvent::manual_trigger("robot-7");
        assert_eq!(event.worker_id, "robot-7");
        assert_eq!(event.reason, EStopReason::ManualTrigger);
    }

    #[test]
    fn to_json_bytes_produces_valid_json() {
        let event = EStopEvent::heartbeat_timeout("worker-1");
        let bytes = event.to_json_bytes().unwrap();
        let parsed: EStopEvent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.worker_id, "worker-1");
    }
}
