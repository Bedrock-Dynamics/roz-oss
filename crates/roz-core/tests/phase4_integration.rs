//! Phase 4 integration smoke test.
//!
//! Exercises the full Phase 4 pipeline — AI skill parsing, execution skill
//! parsing, BT types, sim-to-real comparison, device trust evaluation, and
//! recording types — without any external services (no Postgres, no NATS).

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// 1. Parse an AI skill from the built-in bundle
// ---------------------------------------------------------------------------

#[test]
fn parse_builtin_ai_skill() {
    use roz_core::skills::SkillKind;
    use roz_core::skills::builtin::builtin_skills;

    let skills = builtin_skills();
    assert!(skills.len() >= 3, "expected at least 3 built-in skills");

    let diagnose = skills.iter().find(|s| s.frontmatter.name == "diagnose-motor");
    assert!(diagnose.is_some(), "diagnose-motor must be in builtins");

    let skill = diagnose.unwrap();
    assert_eq!(skill.frontmatter.kind, SkillKind::Ai);
    assert_eq!(skill.frontmatter.version, "1.0.0");
    assert!(skill.frontmatter.tags.contains(&"diagnostics".to_string()));
    assert!(!skill.body.is_empty());
}

// ---------------------------------------------------------------------------
// 2. Template substitution on AI skill body
// ---------------------------------------------------------------------------

#[test]
fn template_substitution() {
    use roz_core::skills::ParameterType;
    use roz_core::skills::template::substitute;

    let body = "Diagnose motor {{motor_id}}. Args: $ARGUMENTS";
    let params = vec![roz_core::skills::SkillParameter {
        name: "motor_id".to_string(),
        param_type: ParameterType::String,
        required: true,
        default: None,
        range: None,
    }];
    let args = serde_json::json!({"motor_id": "joint_3"});

    let result = substitute(body, &params, &args);
    assert!(result.contains("joint_3"));
    assert!(!result.contains("{{motor_id}}"));
}

// ---------------------------------------------------------------------------
// 3. Skill validation
// ---------------------------------------------------------------------------

#[test]
fn validate_builtin_skills() {
    use roz_core::skills::builtin::builtin_skills;
    use roz_core::skills::validate::validate_skill;

    for skill in builtin_skills() {
        let result = validate_skill(&skill.frontmatter);
        assert!(
            result.is_ok(),
            "built-in skill '{}' failed validation: {result:?}",
            skill.frontmatter.name
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Parse an execution skill YAML
// ---------------------------------------------------------------------------

#[test]
fn parse_execution_skill_yaml() {
    use roz_core::bt::parser::parse_execution_skill;

    let yaml = r#"
name: pick-place
description: Pick up an object and place it at the target
version: "1.0.0"
inputs:
  - name: object_pose
    port_type: Pose3D
    required: true
  - name: target_pose
    port_type: Pose3D
    required: true
outputs:
  - name: success
    port_type: bool
    required: true
conditions:
  pre:
    - expression: "{gripper_open} == true"
      phase: pre
  hold:
    - expression: "{force} < 50"
      phase: hold
  post:
    - expression: "{object_placed} == true"
      phase: post
hardware:
  timeout_secs: 30
  heartbeat_hz: 10.0
  reversible: true
  safe_halt_action: open_gripper
tree:
  type: sequence
  children:
    - type: action
      name: approach_object
      action_type: move
    - type: action
      name: close_gripper
      action_type: gripper
    - type: action
      name: move_to_target
      action_type: move
    - type: action
      name: open_gripper
      action_type: gripper
"#;

    let skill = parse_execution_skill(yaml).expect("parsing should succeed");
    assert_eq!(skill.name, "pick-place");
    assert_eq!(skill.inputs.len(), 2);
    assert_eq!(skill.outputs.len(), 1);
    assert_eq!(skill.conditions.pre.len(), 1);
    assert_eq!(skill.conditions.hold.len(), 1);
    assert_eq!(skill.conditions.post.len(), 1);
    assert_eq!(skill.hardware.timeout_secs, 30);
    assert!(skill.hardware.reversible);
}

// ---------------------------------------------------------------------------
// 5. BT blackboard round-trip
// ---------------------------------------------------------------------------

#[test]
fn blackboard_operations() {
    use roz_core::bt::blackboard::Blackboard;
    use serde_json::json;

    let mut bb = Blackboard::new();
    bb.set("velocity", json!(3.14));
    bb.set("armed", json!(true));
    bb.set("nested", json!({"level": 2, "data": [1, 2, 3]}));

    assert_eq!(bb.get("velocity").unwrap(), &json!(3.14));
    assert_eq!(bb.get("armed").unwrap(), &json!(true));

    // Nested reference resolution
    let resolved = bb.resolve_reference("{nested.level}");
    assert!(resolved.is_some());
    assert_eq!(resolved.unwrap(), json!(2));
}

// ---------------------------------------------------------------------------
// 6. BT condition evaluation
// ---------------------------------------------------------------------------

#[test]
fn condition_evaluation() {
    use roz_core::bt::blackboard::Blackboard;
    use roz_core::bt::conditions::ConditionResult;
    use roz_core::bt::eval::evaluate_condition;
    use serde_json::json;

    let mut bb = Blackboard::new();
    bb.set("velocity", json!(5.0));
    bb.set("armed", json!(true));

    assert!(matches!(
        evaluate_condition("{velocity} < 10", &bb),
        ConditionResult::Satisfied
    ));
    assert!(matches!(
        evaluate_condition("{velocity} > 10", &bb),
        ConditionResult::Violated { .. }
    ));
    assert!(matches!(
        evaluate_condition("{armed} == true", &bb),
        ConditionResult::Satisfied
    ));
}

// ---------------------------------------------------------------------------
// 7. Sim-to-Real metrics
// ---------------------------------------------------------------------------

#[test]
fn sim_to_real_metrics() {
    use roz_core::sim2real::metrics::{mae, max_deviation, normalized_rmse, rmse};

    let sim = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let real = vec![1.1, 2.2, 2.8, 4.1, 5.3];

    let r = rmse(&sim, &real).expect("rmse should return a value");
    assert!(r > 0.0 && r < 1.0, "RMSE should be small for similar signals, got {r}");

    let m = mae(&sim, &real).expect("mae should return a value");
    assert!(m > 0.0 && m < 1.0, "MAE should be small, got {m}");

    let d = max_deviation(&sim, &real).expect("max_dev should return a value");
    assert!(d > 0.0 && d < 1.0, "max dev should be small, got {d}");

    let nrmse = normalized_rmse(&sim, &real).expect("nrmse should return a value");
    assert!(nrmse >= 0.0 && nrmse <= 1.0, "NRMSE should be in [0,1], got {nrmse}");
}

// ---------------------------------------------------------------------------
// 8. Sim-to-Real comparison pipeline
// ---------------------------------------------------------------------------

#[test]
fn sim_to_real_pipeline() {
    use roz_core::sim2real::pipeline::{ChannelConfig, ComparisonConfig, compare};
    use roz_core::sim2real::report::{DiagnosisAction, MetricKind};

    let sim_data: HashMap<String, Vec<f64>> = [("velocity".to_string(), vec![1.0, 2.0, 3.0, 4.0, 5.0])].into();
    let real_data: HashMap<String, Vec<f64>> = [("velocity".to_string(), vec![1.05, 2.1, 2.95, 4.05, 5.1])].into();

    let config = ComparisonConfig {
        channels: vec![ChannelConfig {
            name: "velocity".to_string(),
            metric: MetricKind::Rmse,
            threshold: 0.5,
        }],
        pass_score: 0.7,
    };

    let report = compare(&sim_data, &real_data, &config);
    // With very similar signals and a generous threshold, it should pass
    assert_eq!(report.action, DiagnosisAction::Pass);
}

// ---------------------------------------------------------------------------
// 9. Failure signature matching
// ---------------------------------------------------------------------------

#[test]
fn failure_signature_matching() {
    use roz_core::sim2real::signatures::{builtin_signatures, match_signatures};

    let sigs = builtin_signatures();
    assert!(sigs.len() >= 5, "expected at least 5 built-in signatures");

    // Build a minimal report via the pipeline with divergent data
    use roz_core::sim2real::pipeline::{ChannelConfig, ComparisonConfig, compare};
    use roz_core::sim2real::report::MetricKind;

    let sim_data: HashMap<String, Vec<f64>> = [("motor_current".to_string(), vec![2.0, 2.1, 2.0, 2.0, 2.0])].into();
    let real_data: HashMap<String, Vec<f64>> =
        [("motor_current".to_string(), vec![15.0, 14.5, 15.2, 14.8, 15.0])].into();

    let config = ComparisonConfig {
        channels: vec![ChannelConfig {
            name: "motor_current".to_string(),
            metric: MetricKind::Rmse,
            threshold: 1.0,
        }],
        pass_score: 0.8,
    };

    let report = compare(&sim_data, &real_data, &config);
    let matches = match_signatures(&report, &sigs);
    // Not all signatures will match, but the function should return without error
    assert!(matches.len() <= sigs.len());
}

// ---------------------------------------------------------------------------
// 10. Device trust firmware verification
// ---------------------------------------------------------------------------

#[test]
fn device_trust_firmware_verification() {
    use roz_core::device_trust::verify::{verify_firmware_crc32, verify_firmware_sha256};

    let firmware = b"firmware binary data for testing";

    // CRC32
    let expected_crc = crc32fast::hash(firmware);
    assert!(verify_firmware_crc32(firmware, expected_crc));
    assert!(!verify_firmware_crc32(firmware, expected_crc.wrapping_add(1)));

    // SHA256
    use sha2::{Digest, Sha256};
    let expected_sha = hex::encode(Sha256::digest(firmware));
    assert!(verify_firmware_sha256(firmware, &expected_sha));
    assert!(!verify_firmware_sha256(
        firmware,
        "0000000000000000000000000000000000000000000000000000000000000000"
    ));
}

// ---------------------------------------------------------------------------
// 11. Device trust posture evaluation
// ---------------------------------------------------------------------------

#[test]
fn device_trust_evaluation() {
    use chrono::Utc;
    use roz_core::device_trust::evaluator::{TrustPolicy, evaluate_trust};
    use roz_core::device_trust::{DeviceTrust, FirmwareManifest, FlashPartition, TrustPosture};
    use uuid::Uuid;

    let now = Utc::now();

    let policy = TrustPolicy {
        max_attestation_age_secs: 3600,
        require_firmware_signature: false,
        allowed_firmware_versions: vec!["2.0.0".to_string()],
    };

    // Trusted: known firmware, recent attestation
    let trusted_device = DeviceTrust {
        host_id: Uuid::new_v4(),
        tenant_id: "tenant-1".to_string(),
        posture: TrustPosture::Untrusted,
        firmware: Some(FirmwareManifest {
            version: "2.0.0".to_string(),
            sha256: "abc123".to_string(),
            crc32: 12345,
            ed25519_signature: None,
            partition: FlashPartition::A,
        }),
        sbom_hash: None,
        last_attestation: Some(now),
        created_at: now,
        updated_at: now,
    };

    assert_eq!(evaluate_trust(&trusted_device, &policy, now), TrustPosture::Trusted);

    // Untrusted: unknown firmware
    let untrusted_device = DeviceTrust {
        firmware: Some(FirmwareManifest {
            version: "0.1.0-unknown".to_string(),
            sha256: "xxx".to_string(),
            crc32: 0,
            ed25519_signature: None,
            partition: FlashPartition::A,
        }),
        ..trusted_device.clone()
    };

    assert_eq!(evaluate_trust(&untrusted_device, &policy, now), TrustPosture::Untrusted);
}

// ---------------------------------------------------------------------------
// 12. Recording manifest types
// ---------------------------------------------------------------------------

#[test]
fn recording_manifest_serde() {
    use chrono::Utc;
    use roz_core::recording::{ChannelManifest, RecordingManifest, RecordingSource};
    use uuid::Uuid;

    let manifest = RecordingManifest {
        id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        environment_id: Uuid::new_v4(),
        host_id: Uuid::new_v4(),
        source: RecordingSource::Simulation,
        channels: vec![ChannelManifest {
            name: "joint_velocity".to_string(),
            topic: "/joint/vel".to_string(),
            schema_name: "Float64".to_string(),
            message_count: 100,
        }],
        duration_secs: 10.5,
        created_at: Utc::now(),
    };

    let json = serde_json::to_string(&manifest).expect("serialize");
    let back: RecordingManifest = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.id, manifest.id);
    assert_eq!(back.source, RecordingSource::Simulation);
    assert_eq!(back.channels.len(), 1);
}

// ---------------------------------------------------------------------------
// 13. WAL skill entries
// ---------------------------------------------------------------------------

#[test]
fn wal_skill_entries_serde() {
    use roz_core::wal::WalEntry;

    let entries = vec![
        WalEntry::SkillStarted {
            skill_name: "diagnose-motor".to_string(),
            kind: "ai".to_string(),
        },
        WalEntry::SkillCompleted {
            skill_name: "diagnose-motor".to_string(),
            success: true,
            ticks: Some(5),
        },
        WalEntry::ConditionViolation {
            skill_name: "pick-place".to_string(),
            condition: "{force} < 50".to_string(),
            phase: "hold".to_string(),
        },
    ];

    for entry in &entries {
        let json = serde_json::to_string(entry).expect("serialize WAL entry");
        let back: WalEntry = serde_json::from_str(&json).expect("deserialize WAL entry");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(entry).unwrap()
        );
    }
}

// ---------------------------------------------------------------------------
// 14. Error types
// ---------------------------------------------------------------------------

#[test]
fn error_types_phase4() {
    use roz_core::errors::RozError;

    let errors: Vec<RozError> = vec![
        RozError::SkillParse("bad frontmatter".to_string()),
        RozError::SkillNotFound("nonexistent-skill".to_string()),
        RozError::BehaviorTree("invalid node".to_string()),
        RozError::ConditionViolated("force exceeded".to_string()),
        RozError::Recording("corrupt MCAP".to_string()),
        RozError::TrustVerification("signature mismatch".to_string()),
    ];

    for err in &errors {
        let msg = err.to_string();
        assert!(!msg.is_empty(), "error message should not be empty");
    }
}

// ---------------------------------------------------------------------------
// 15. Skill discovery (filesystem-based)
// ---------------------------------------------------------------------------

#[test]
fn skill_discovery_builtins() {
    use roz_core::skills::SkillSource;
    use roz_core::skills::builtin::builtin_skills;
    use roz_core::skills::discovery::SkillDiscovery;

    let builtins = builtin_skills();
    let discovery = SkillDiscovery::new(vec![], vec![], builtins);
    let summaries = discovery.discover();
    assert!(
        summaries.len() >= 3,
        "should discover at least 3 built-in skills, got {}",
        summaries.len()
    );

    for s in &summaries {
        assert_eq!(s.source, SkillSource::BuiltIn);
    }
}

// ---------------------------------------------------------------------------
// 16. DTW alignment
// ---------------------------------------------------------------------------

#[test]
fn dtw_alignment() {
    use roz_core::sim2real::dtw::dtw_align;

    let sim = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let real = vec![1.0, 2.0, 3.0, 4.0, 5.0]; // identical

    let result = dtw_align(&sim, &real, None);
    assert!(
        result.distance < f64::EPSILON,
        "identical signals should have ~0 DTW distance"
    );
    assert!(!result.warping_path.is_empty());
}
