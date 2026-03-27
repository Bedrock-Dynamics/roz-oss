use serde::{Deserialize, Serialize};

use crate::adapter::AdapterState;
use crate::safety::SafetyLevel;
use crate::tools::ToolResult;

/// Write-ahead log entry variants for crash recovery and auditability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WalEntry {
    AdapterTransition {
        from: AdapterState,
        to: AdapterState,
    },
    ToolResult {
        call_id: String,
        result: ToolResult,
    },
    HardwareCommand {
        idempotency_key: String,
        command: String,
        acked: bool,
    },
    OodaCycleComplete {
        cycle: u32,
    },
    TelemetryBatch {
        count: u32,
        last_seq: u64,
    },
    SafetyEvent {
        level: SafetyLevel,
        message: String,
    },
    // Phase 4 — Skill lifecycle events
    SkillStarted {
        skill_name: String,
        kind: String,
    },
    SkillCompleted {
        skill_name: String,
        success: bool,
        ticks: Option<u32>,
    },
    ConditionViolation {
        skill_name: String,
        condition: String,
        phase: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AdapterState;
    use crate::safety::SafetyLevel;
    use crate::tools::ToolResult;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // AdapterTransition serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn adapter_transition_serde_roundtrip() {
        let entry = WalEntry::AdapterTransition {
            from: AdapterState::Inactive,
            to: AdapterState::Active,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: WalEntry = serde_json::from_str(&json).unwrap();

        match deserialized {
            WalEntry::AdapterTransition { from, to } => {
                assert_eq!(from, AdapterState::Inactive);
                assert_eq!(to, AdapterState::Active);
            }
            _ => panic!("expected AdapterTransition variant"),
        }
    }

    // -----------------------------------------------------------------------
    // ToolResult serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn tool_result_entry_serde_roundtrip() {
        let entry = WalEntry::ToolResult {
            call_id: "call-123".to_string(),
            result: ToolResult::success(json!({"status": "ok"})),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: WalEntry = serde_json::from_str(&json).unwrap();

        match deserialized {
            WalEntry::ToolResult { call_id, result } => {
                assert_eq!(call_id, "call-123");
                assert!(result.is_success());
            }
            _ => panic!("expected ToolResult variant"),
        }
    }

    // -----------------------------------------------------------------------
    // HardwareCommand serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn hardware_command_serde_roundtrip() {
        let entry = WalEntry::HardwareCommand {
            idempotency_key: "idem-456".to_string(),
            command: "move_arm".to_string(),
            acked: false,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: WalEntry = serde_json::from_str(&json).unwrap();

        match deserialized {
            WalEntry::HardwareCommand {
                idempotency_key,
                command,
                acked,
            } => {
                assert_eq!(idempotency_key, "idem-456");
                assert_eq!(command, "move_arm");
                assert!(!acked);
            }
            _ => panic!("expected HardwareCommand variant"),
        }
    }

    // -----------------------------------------------------------------------
    // OodaCycleComplete serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn ooda_cycle_complete_serde_roundtrip() {
        let entry = WalEntry::OodaCycleComplete { cycle: 42 };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: WalEntry = serde_json::from_str(&json).unwrap();

        match deserialized {
            WalEntry::OodaCycleComplete { cycle } => {
                assert_eq!(cycle, 42);
            }
            _ => panic!("expected OodaCycleComplete variant"),
        }
    }

    // -----------------------------------------------------------------------
    // All variants serialize with correct type tag
    // -----------------------------------------------------------------------

    #[test]
    fn all_variants_have_correct_type_tag() {
        let entries: Vec<(&str, WalEntry)> = vec![
            (
                "adapter_transition",
                WalEntry::AdapterTransition {
                    from: AdapterState::Unconfigured,
                    to: AdapterState::Inactive,
                },
            ),
            (
                "tool_result",
                WalEntry::ToolResult {
                    call_id: "c1".to_string(),
                    result: ToolResult::success(json!(null)),
                },
            ),
            ("ooda_cycle_complete", WalEntry::OodaCycleComplete { cycle: 1 }),
            (
                "telemetry_batch",
                WalEntry::TelemetryBatch {
                    count: 10,
                    last_seq: 99,
                },
            ),
            (
                "safety_event",
                WalEntry::SafetyEvent {
                    level: SafetyLevel::Warning,
                    message: "overheat".to_string(),
                },
            ),
            (
                "hardware_command",
                WalEntry::HardwareCommand {
                    idempotency_key: "k1".to_string(),
                    command: "stop".to_string(),
                    acked: true,
                },
            ),
        ];

        for (expected_tag, entry) in entries {
            let json = serde_json::to_string(&entry).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let actual_tag = parsed["type"].as_str().unwrap();
            assert_eq!(
                actual_tag, expected_tag,
                "wrong type tag for {:?}: expected '{}', got '{}'",
                entry, expected_tag, actual_tag
            );
        }
    }

    // -----------------------------------------------------------------------
    // TelemetryBatch serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn telemetry_batch_serde_roundtrip() {
        let entry = WalEntry::TelemetryBatch {
            count: 100,
            last_seq: 12345,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: WalEntry = serde_json::from_str(&json).unwrap();

        match deserialized {
            WalEntry::TelemetryBatch { count, last_seq } => {
                assert_eq!(count, 100);
                assert_eq!(last_seq, 12345);
            }
            _ => panic!("expected TelemetryBatch variant"),
        }
    }

    // -----------------------------------------------------------------------
    // SafetyEvent serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn safety_event_serde_roundtrip() {
        let entry = WalEntry::SafetyEvent {
            level: SafetyLevel::EmergencyStop,
            message: "collision imminent".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: WalEntry = serde_json::from_str(&json).unwrap();

        match deserialized {
            WalEntry::SafetyEvent { level, message } => {
                assert_eq!(level, SafetyLevel::EmergencyStop);
                assert_eq!(message, "collision imminent");
            }
            _ => panic!("expected SafetyEvent variant"),
        }
    }
}
