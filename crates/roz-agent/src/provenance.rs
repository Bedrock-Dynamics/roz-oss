use std::collections::HashMap;

use roz_core::provenance::RunProvenance;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Compute a hex-encoded SHA-256 hash of the input string.
fn sha256_hex(input: &str) -> String {
    let hash = Sha256::digest(input.as_bytes());
    hex::encode(hash)
}

/// Parameters describing a task's execution context, used to generate a
/// [`RunProvenance`] manifest.
pub struct ProvenanceInput<'a> {
    pub task_run_id: Uuid,
    pub tenant_id: &'a str,
    pub prompt: &'a str,
    pub model_id: Option<&'a str>,
    pub model_version: Option<&'a str>,
    pub tool_versions: &'a HashMap<String, String>,
    pub firmware_sha: Option<&'a str>,
    pub calibration_hash: Option<&'a str>,
    pub sim_image: Option<&'a str>,
    pub environment_config_json: &'a str,
}

/// Generate an immutable [`RunProvenance`] manifest from the given inputs.
///
/// Hashes are computed deterministically so that identical inputs always
/// produce identical provenance records.
pub fn generate(input: &ProvenanceInput<'_>) -> RunProvenance {
    RunProvenance {
        id: Uuid::new_v4(),
        task_run_id: input.task_run_id,
        tenant_id: input.tenant_id.to_string(),
        model_id: input.model_id.map(String::from),
        model_version: input.model_version.map(String::from),
        prompt_hash: Some(sha256_hex(input.prompt)),
        tool_versions: serde_json::to_value(input.tool_versions).unwrap_or_default(),
        firmware_sha: input.firmware_sha.map(String::from),
        calibration_hash: input.calibration_hash.map(String::from),
        sim_image: input.sim_image.map(String::from),
        environment_hash: Some(sha256_hex(input.environment_config_json)),
        created_at: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> HashMap<String, String> {
        [
            ("gripper_v2".into(), "1.4.0".into()),
            ("camera_feed".into(), "2.1.0".into()),
        ]
        .into()
    }

    fn make_input(tools: &HashMap<String, String>) -> ProvenanceInput<'_> {
        ProvenanceInput {
            task_run_id: Uuid::nil(),
            tenant_id: "tenant-acme-001",
            prompt: "Pick up the red cube and place it on the shelf",
            model_id: Some("claude-opus-4-6"),
            model_version: Some("2025-05-14"),
            tool_versions: tools,
            firmware_sha: Some("sha256:fw_deadbeef"),
            calibration_hash: Some("sha256:cal_cafebabe"),
            sim_image: Some("ghcr.io/bedrock/sim-env:v3.2"),
            environment_config_json: r#"{"image":"px4-sitl","world":"warehouse"}"#,
        }
    }

    #[test]
    fn prompt_hash_is_deterministic() {
        let tools = tools();
        let input = make_input(&tools);
        let prov_a = generate(&input);
        let prov_b = generate(&input);

        assert_eq!(prov_a.prompt_hash, prov_b.prompt_hash);
        assert!(prov_a.prompt_hash.as_ref().unwrap().len() == 64); // SHA-256 = 64 hex chars
    }

    #[test]
    fn environment_hash_is_deterministic() {
        let tools = tools();
        let input = make_input(&tools);
        let prov_a = generate(&input);
        let prov_b = generate(&input);

        assert_eq!(prov_a.environment_hash, prov_b.environment_hash);
        assert!(prov_a.environment_hash.as_ref().unwrap().len() == 64);
    }

    #[test]
    fn different_prompts_produce_different_hashes() {
        let tools = tools();
        let mut input = make_input(&tools);
        let prov_a = generate(&input);

        input.prompt = "Navigate to waypoint alpha";
        let prov_b = generate(&input);

        assert_ne!(prov_a.prompt_hash, prov_b.prompt_hash);
    }

    #[test]
    fn different_environments_produce_different_hashes() {
        let tools = tools();
        let mut input = make_input(&tools);
        let prov_a = generate(&input);

        input.environment_config_json = r#"{"image":"ardupilot-sitl","world":"outdoor"}"#;
        let prov_b = generate(&input);

        assert_ne!(prov_a.environment_hash, prov_b.environment_hash);
    }

    #[test]
    fn fields_are_correctly_mapped() {
        let tools = tools();
        let input = make_input(&tools);
        let prov = generate(&input);

        assert_eq!(prov.task_run_id, Uuid::nil());
        assert_eq!(prov.tenant_id, "tenant-acme-001");
        assert_eq!(prov.model_id.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(prov.model_version.as_deref(), Some("2025-05-14"));
        assert_eq!(prov.firmware_sha.as_deref(), Some("sha256:fw_deadbeef"));
        assert_eq!(prov.calibration_hash.as_deref(), Some("sha256:cal_cafebabe"));
        assert_eq!(prov.sim_image.as_deref(), Some("ghcr.io/bedrock/sim-env:v3.2"));
    }

    #[test]
    fn tool_versions_serialized_correctly() {
        let tools = tools();
        let input = make_input(&tools);
        let prov = generate(&input);

        let tv = &prov.tool_versions;
        assert_eq!(tv.get("gripper_v2").and_then(|v| v.as_str()), Some("1.4.0"));
        assert_eq!(tv.get("camera_feed").and_then(|v| v.as_str()), Some("2.1.0"));
    }

    #[test]
    fn minimal_input_produces_valid_provenance() {
        let tools = HashMap::new();
        let input = ProvenanceInput {
            task_run_id: Uuid::new_v4(),
            tenant_id: "tenant-solo",
            prompt: "",
            model_id: None,
            model_version: None,
            tool_versions: &tools,
            firmware_sha: None,
            calibration_hash: None,
            sim_image: None,
            environment_config_json: "{}",
        };
        let prov = generate(&input);

        assert!(prov.model_id.is_none());
        assert!(prov.firmware_sha.is_none());
        assert!(prov.prompt_hash.is_some()); // even empty string gets hashed
        assert!(prov.environment_hash.is_some());
    }

    #[test]
    fn sha256_hex_known_value() {
        // SHA-256 of empty string is well-known
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn each_invocation_gets_unique_id() {
        let tools = tools();
        let input = make_input(&tools);
        let prov_a = generate(&input);
        let prov_b = generate(&input);

        assert_ne!(prov_a.id, prov_b.id);
    }
}
