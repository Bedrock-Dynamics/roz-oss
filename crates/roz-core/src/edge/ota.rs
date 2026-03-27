//! OTA (Over-The-Air) module distribution types.
//!
//! WASM modules are distributed via NATS Object Store. Each module has a
//! manifest describing its version, target architecture, and checksum.
//! The `.cwasm.lkg` (last-known-good) file enables instant rollback.

use serde::{Deserialize, Serialize};

/// Manifest for a deployable WASM module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleManifest {
    /// Module name (e.g., "arm-controller").
    pub name: String,
    /// Semantic version.
    pub version: String,
    /// Target architecture (e.g., "aarch64", "x86\_64").
    pub target_arch: String,
    /// SHA-256 hash of the `.cwasm` file.
    pub sha256: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Unix timestamp of creation.
    pub created_at: u64,
    /// Sim-to-real validation score (0.0 -- 1.0). Modules below threshold are rejected.
    #[serde(default)]
    pub sim2real_score: Option<f64>,
    /// ID of the sim-to-real comparison report.
    #[serde(default)]
    pub sim2real_report_id: Option<String>,
}

/// Rollback tracking for a deployed module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackInfo {
    /// Path to the currently active `.cwasm` file.
    pub current: String,
    /// Path to the last-known-good `.cwasm` file (for instant rollback).
    pub last_known_good: Option<String>,
}

/// Canary rollout stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutStage {
    /// Deploy to a single robot for validation.
    Canary,
    /// Deploy to 10% of fleet.
    Partial10,
    /// Deploy to 25% of fleet.
    Partial25,
    /// Deploy to 75% of fleet.
    Partial75,
    /// Deploy to all robots.
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_manifest_serde_roundtrip() {
        let manifest = ModuleManifest {
            name: "arm-controller".to_string(),
            version: "1.2.3".to_string(),
            target_arch: "aarch64".to_string(),
            sha256: "abc123".to_string(),
            size_bytes: 1024,
            created_at: 1_234_567_890,
            sim2real_score: None,
            sim2real_report_id: None,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: ModuleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "arm-controller");
        assert_eq!(parsed.version, "1.2.3");
        assert!(parsed.sim2real_score.is_none());
    }

    #[test]
    fn manifest_with_sim2real_score_roundtrip() {
        let manifest = ModuleManifest {
            name: "leg-controller".to_string(),
            version: "2.0.0".to_string(),
            target_arch: "aarch64".to_string(),
            sha256: "def456".to_string(),
            size_bytes: 2048,
            created_at: 1_700_000_000,
            sim2real_score: Some(0.92),
            sim2real_report_id: Some("report-abc-123".to_string()),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: ModuleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.sim2real_score, Some(0.92));
        assert_eq!(parsed.sim2real_report_id.as_deref(), Some("report-abc-123"));

        // Verify backwards compatibility: JSON without sim2real fields deserializes fine.
        let legacy_json = r#"{
            "name": "old-module",
            "version": "0.1.0",
            "target_arch": "x86_64",
            "sha256": "aaa",
            "size_bytes": 512,
            "created_at": 1000
        }"#;
        let legacy: ModuleManifest = serde_json::from_str(legacy_json).unwrap();
        assert!(legacy.sim2real_score.is_none());
        assert!(legacy.sim2real_report_id.is_none());
    }

    #[test]
    fn rollback_info_tracks_lkg() {
        let info = RollbackInfo {
            current: "arm-controller-1.2.3.cwasm".to_string(),
            last_known_good: Some("arm-controller-1.2.2.cwasm".to_string()),
        };
        assert!(info.last_known_good.is_some());
    }
}
