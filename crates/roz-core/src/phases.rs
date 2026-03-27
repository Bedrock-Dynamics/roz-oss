//! Declarative task phase specification for the agent loop.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// PhaseMode
// ---------------------------------------------------------------------------

/// Execution mode for a task phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseMode {
    React,
    OodaReAct,
}

// ---------------------------------------------------------------------------
// PhaseSpec
// ---------------------------------------------------------------------------

/// Specifies one phase of an agent task: which mode to run in,
/// which tools are available, and when to enter this phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseSpec {
    pub mode: PhaseMode,
    pub tools: ToolSetFilter,
    pub trigger: PhaseTrigger,
}

// ---------------------------------------------------------------------------
// ToolSetFilter
// ---------------------------------------------------------------------------

/// Controls which tools the model can use in a phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSetFilter {
    All,
    Named(Vec<String>),
    None,
}

// ---------------------------------------------------------------------------
// PhaseTrigger
// ---------------------------------------------------------------------------

/// Determines when a phase transition fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseTrigger {
    Immediate,
    AfterCycles(u32),
    OnToolSignal,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // PhaseTrigger serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn phase_trigger_serde_roundtrip() {
        let variants = [
            PhaseTrigger::Immediate,
            PhaseTrigger::AfterCycles(5),
            PhaseTrigger::OnToolSignal,
        ];
        for trigger in &variants {
            let json = serde_json::to_string(trigger).unwrap();
            let roundtripped: PhaseTrigger = serde_json::from_str(&json).unwrap();
            assert_eq!(trigger, &roundtripped);
        }
        // Verify the snake_case tag for the newtype variant
        let json = serde_json::to_string(&PhaseTrigger::AfterCycles(3)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["after_cycles"], 3);
    }

    // -----------------------------------------------------------------------
    // ToolSetFilter serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn tool_set_filter_serde_roundtrip() {
        let variants = [
            ToolSetFilter::All,
            ToolSetFilter::Named(vec!["move_arm".to_string(), "gripper_open".to_string()]),
            ToolSetFilter::None,
        ];
        for filter in &variants {
            let json = serde_json::to_string(filter).unwrap();
            let roundtripped: ToolSetFilter = serde_json::from_str(&json).unwrap();
            assert_eq!(filter, &roundtripped);
        }
    }

    // -----------------------------------------------------------------------
    // PhaseSpec serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn phase_spec_serde_roundtrip() {
        let spec = PhaseSpec {
            mode: PhaseMode::OodaReAct,
            tools: ToolSetFilter::Named(vec!["sensor_read".to_string()]),
            trigger: PhaseTrigger::AfterCycles(2),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let roundtripped: PhaseSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, roundtripped);
    }
}
