use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// RunProvenance
// ---------------------------------------------------------------------------

/// Immutable manifest capturing the full context of a task run.
///
/// Records every input that could affect reproducibility: the model and its
/// version, a hash of the prompt, tool versions, firmware/calibration state,
/// and the simulation image. This is the "birth certificate" of a run result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunProvenance {
    pub id: Uuid,
    pub task_run_id: Uuid,
    pub tenant_id: String,
    pub model_id: Option<String>,
    pub model_version: Option<String>,
    pub prompt_hash: Option<String>,
    pub tool_versions: serde_json::Value,
    pub firmware_sha: Option<String>,
    pub calibration_hash: Option<String>,
    pub sim_image: Option<String>,
    pub environment_hash: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sample_provenance_full() -> RunProvenance {
        RunProvenance {
            id: Uuid::new_v4(),
            task_run_id: Uuid::new_v4(),
            tenant_id: "tenant-acme-001".into(),
            model_id: Some("claude-opus-4-6".into()),
            model_version: Some("2025-05-14".into()),
            prompt_hash: Some("sha256:abc123def456".into()),
            tool_versions: json!({
                "gripper_v2": "1.4.0",
                "camera_feed": "2.1.0",
                "path_planner": "0.9.3"
            }),
            firmware_sha: Some("sha256:fw_deadbeef".into()),
            calibration_hash: Some("sha256:cal_cafebabe".into()),
            sim_image: Some("ghcr.io/bedrock/sim-env:v3.2".into()),
            environment_hash: Some("sha256:env_12345678".into()),
            created_at: Utc::now(),
        }
    }

    fn sample_provenance_minimal() -> RunProvenance {
        RunProvenance {
            id: Uuid::new_v4(),
            task_run_id: Uuid::new_v4(),
            tenant_id: "tenant-solo-007".into(),
            model_id: None,
            model_version: None,
            prompt_hash: None,
            tool_versions: json!({}),
            firmware_sha: None,
            calibration_hash: None,
            sim_image: None,
            environment_hash: None,
            created_at: Utc::now(),
        }
    }

    // -----------------------------------------------------------------------
    // Serde round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn provenance_serde_roundtrip_all_fields_populated() {
        let original = sample_provenance_full();
        let json = serde_json::to_string(&original).unwrap();
        let restored: RunProvenance = serde_json::from_str(&json).unwrap();

        assert_eq!(original.id, restored.id);
        assert_eq!(original.task_run_id, restored.task_run_id);
        assert_eq!(original.tenant_id, restored.tenant_id);
        assert_eq!(original.model_id, restored.model_id);
        assert_eq!(original.model_version, restored.model_version);
        assert_eq!(original.prompt_hash, restored.prompt_hash);
        assert_eq!(original.tool_versions, restored.tool_versions);
        assert_eq!(original.firmware_sha, restored.firmware_sha);
        assert_eq!(original.calibration_hash, restored.calibration_hash);
        assert_eq!(original.sim_image, restored.sim_image);
        assert_eq!(original.environment_hash, restored.environment_hash);
    }

    #[test]
    fn provenance_serde_roundtrip_optional_fields_none() {
        let original = sample_provenance_minimal();
        let json = serde_json::to_string(&original).unwrap();
        let restored: RunProvenance = serde_json::from_str(&json).unwrap();

        assert_eq!(original.id, restored.id);
        assert_eq!(original.task_run_id, restored.task_run_id);
        assert_eq!(original.tenant_id, restored.tenant_id);
        assert_eq!(restored.model_id, None);
        assert_eq!(restored.model_version, None);
        assert_eq!(restored.prompt_hash, None);
        assert_eq!(restored.tool_versions, json!({}));
        assert_eq!(restored.firmware_sha, None);
        assert_eq!(restored.calibration_hash, None);
        assert_eq!(restored.sim_image, None);
        assert_eq!(restored.environment_hash, None);
    }

    #[test]
    fn provenance_json_has_expected_field_names() {
        let prov = sample_provenance_full();
        let json = serde_json::to_string(&prov).unwrap();

        assert!(json.contains("\"id\""));
        assert!(json.contains("\"task_run_id\""));
        assert!(json.contains("\"tenant_id\""));
        assert!(json.contains("\"model_id\""));
        assert!(json.contains("\"model_version\""));
        assert!(json.contains("\"prompt_hash\""));
        assert!(json.contains("\"tool_versions\""));
        assert!(json.contains("\"firmware_sha\""));
        assert!(json.contains("\"calibration_hash\""));
        assert!(json.contains("\"sim_image\""));
        assert!(json.contains("\"environment_hash\""));
        assert!(json.contains("\"created_at\""));
    }
}
