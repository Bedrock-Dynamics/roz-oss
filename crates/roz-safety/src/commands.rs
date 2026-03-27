use serde::{Deserialize, Serialize};

/// Safety commands that can be received from NATS or other sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum SafetyCommand {
    EmergencyStop,
    SafeStop { reason: String },
    Resume,
    SetLevel { level: roz_core::safety::SafetyLevel },
    Heartbeat,
}

impl SafetyCommand {
    /// Parse a safety command from JSON bytes.
    pub fn parse(json: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::safety::SafetyLevel;

    #[test]
    fn parse_emergency_stop() {
        let json = br#"{"command": "emergency_stop"}"#;
        let cmd = SafetyCommand::parse(json).unwrap();
        assert_eq!(cmd, SafetyCommand::EmergencyStop);
    }

    #[test]
    fn parse_safe_stop() {
        let json = br#"{"command": "safe_stop", "reason": "battery low"}"#;
        let cmd = SafetyCommand::parse(json).unwrap();
        assert_eq!(
            cmd,
            SafetyCommand::SafeStop {
                reason: "battery low".to_string()
            }
        );
    }

    #[test]
    fn parse_resume() {
        let json = br#"{"command": "resume"}"#;
        let cmd = SafetyCommand::parse(json).unwrap();
        assert_eq!(cmd, SafetyCommand::Resume);
    }

    #[test]
    fn parse_set_level() {
        let json = br#"{"command": "set_level", "level": "warning"}"#;
        let cmd = SafetyCommand::parse(json).unwrap();
        assert_eq!(
            cmd,
            SafetyCommand::SetLevel {
                level: SafetyLevel::Warning
            }
        );
    }

    #[test]
    fn parse_set_level_emergency_stop() {
        let json = br#"{"command": "set_level", "level": "emergency_stop"}"#;
        let cmd = SafetyCommand::parse(json).unwrap();
        assert_eq!(
            cmd,
            SafetyCommand::SetLevel {
                level: SafetyLevel::EmergencyStop
            }
        );
    }

    #[test]
    fn parse_heartbeat() {
        let json = br#"{"command": "heartbeat"}"#;
        let cmd = SafetyCommand::parse(json).unwrap();
        assert_eq!(cmd, SafetyCommand::Heartbeat);
    }

    #[test]
    fn invalid_json_returns_error() {
        let json = b"not valid json";
        let result = SafetyCommand::parse(json);
        assert!(result.is_err(), "invalid JSON should return error");
    }

    #[test]
    fn unknown_command_returns_error() {
        let json = br#"{"command": "self_destruct"}"#;
        let result = SafetyCommand::parse(json);
        assert!(result.is_err(), "unknown command should return error");
    }

    #[test]
    fn roundtrip_emergency_stop() {
        let cmd = SafetyCommand::EmergencyStop;
        let json = serde_json::to_vec(&cmd).unwrap();
        let parsed = SafetyCommand::parse(&json).unwrap();
        assert_eq!(cmd, parsed);
    }

    #[test]
    fn roundtrip_safe_stop() {
        let cmd = SafetyCommand::SafeStop {
            reason: "operator request".to_string(),
        };
        let json = serde_json::to_vec(&cmd).unwrap();
        let parsed = SafetyCommand::parse(&json).unwrap();
        assert_eq!(cmd, parsed);
    }

    #[test]
    fn roundtrip_resume() {
        let cmd = SafetyCommand::Resume;
        let json = serde_json::to_vec(&cmd).unwrap();
        let parsed = SafetyCommand::parse(&json).unwrap();
        assert_eq!(cmd, parsed);
    }

    #[test]
    fn roundtrip_set_level() {
        let cmd = SafetyCommand::SetLevel {
            level: SafetyLevel::ReducedMode,
        };
        let json = serde_json::to_vec(&cmd).unwrap();
        let parsed = SafetyCommand::parse(&json).unwrap();
        assert_eq!(cmd, parsed);
    }

    #[test]
    fn roundtrip_heartbeat() {
        let cmd = SafetyCommand::Heartbeat;
        let json = serde_json::to_vec(&cmd).unwrap();
        let parsed = SafetyCommand::parse(&json).unwrap();
        assert_eq!(cmd, parsed);
    }

    #[test]
    fn serialized_json_uses_snake_case_tag() {
        let cmd = SafetyCommand::EmergencyStop;
        let json_str = serde_json::to_string(&cmd).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["command"], "emergency_stop");
    }

    #[test]
    fn safe_stop_missing_reason_returns_error() {
        let json = br#"{"command": "safe_stop"}"#;
        let result = SafetyCommand::parse(json);
        assert!(result.is_err(), "safe_stop without reason should return error");
    }
}
