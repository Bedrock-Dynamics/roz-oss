//! Session memory and skill persistence for agent runtime.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Class of memory entry — 6-class model matching embodied agent time horizons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryClass {
    /// Immediate embodied state — expires aggressively (seconds to minutes).
    /// "object was behind bin 3 two minutes ago"
    WorkingWorld,
    /// Current task context — expires when task completes.
    /// "we're assembling the widget, step 3 of 5"
    Task,
    /// Durable procedures and skills — long-lived.
    /// "drawer handle is usually stiff, apply 2N extra"
    Procedure,
    /// Persistent environment facts — requires provenance.
    /// "camera wrist extrinsic was recalibrated today"
    Environment,
    /// Operator preferences and instructions — cloud-canonical.
    Operator,
    /// Safety-relevant learned facts — highest retention priority.
    /// "joint 3 has 2mm backlash at low speed"
    Safety,
}

/// Confidence level of a memory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low = 0,
    Medium = 1,
    High = 2,
}

/// Where a memory came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceKind {
    Observation,
    OperatorStated,
    VerifierConfirmed,
}

/// A durable curated memory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub memory_id: String,
    pub class: MemoryClass,
    pub scope_key: String,
    pub fact: String,
    pub source_kind: MemorySourceKind,
    pub source_ref: Option<String>,
    pub confidence: Confidence,
    pub verified: bool,
    pub stale_after: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MemoryEntry {
    /// Check if this memory is stale.
    #[must_use]
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        self.stale_after.is_some_and(|stale| now >= stale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_memory() -> MemoryEntry {
        MemoryEntry {
            memory_id: "mem-001".into(),
            class: MemoryClass::Environment,
            scope_key: "env:lab-1".into(),
            fact: "table is 5cm left of URDF position".into(),
            source_kind: MemorySourceKind::Observation,
            source_ref: Some("sess-42:turn-3:observe_scene".into()),
            confidence: Confidence::High,
            verified: false,
            stale_after: Some(Utc::now() + chrono::Duration::hours(8)),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn memory_serde_roundtrip() {
        let mem = sample_memory();
        let json = serde_json::to_string(&mem).unwrap();
        let back: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(mem.memory_id, back.memory_id);
        assert_eq!(mem.class, back.class);
        assert_eq!(mem.fact, back.fact);
    }

    #[test]
    fn memory_not_stale_before_deadline() {
        let mem = sample_memory();
        assert!(!mem.is_stale(Utc::now()));
    }

    #[test]
    fn memory_stale_after_deadline() {
        let mut mem = sample_memory();
        mem.stale_after = Some(Utc::now() - chrono::Duration::hours(1));
        assert!(mem.is_stale(Utc::now()));
    }

    #[test]
    fn memory_never_stale_without_deadline() {
        let mut mem = sample_memory();
        mem.stale_after = None;
        assert!(!mem.is_stale(Utc::now()));
    }

    #[test]
    fn confidence_ordering() {
        assert!(Confidence::Low < Confidence::Medium);
        assert!(Confidence::Medium < Confidence::High);
    }

    #[test]
    fn all_memory_classes_serde() {
        let classes = vec![
            MemoryClass::WorkingWorld,
            MemoryClass::Task,
            MemoryClass::Procedure,
            MemoryClass::Environment,
            MemoryClass::Operator,
            MemoryClass::Safety,
        ];
        assert_eq!(classes.len(), 6, "all 6 memory classes must be tested");
        for c in classes {
            let json = serde_json::to_string(&c).unwrap();
            let back: MemoryClass = serde_json::from_str(&json).unwrap();
            assert_eq!(c, back);
        }
    }
}
