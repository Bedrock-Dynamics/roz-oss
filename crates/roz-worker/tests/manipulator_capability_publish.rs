//! Phase 26.10 Plan 09 (FW-06 / Codex H5) — capability publication is lossless
//! from `EmbodimentRuntime`.
//!
//! Pinned at the integration-test layer so the worker's external contract
//! (capability publish payload shape) is enforced regardless of how the helper
//! is wired into worker startup. If a future refactor moves the
//! `project_capabilities` call site or changes its arguments, this test still
//! catches drift between manifest content and capability publish.

#![cfg(feature = "test-fixtures")]

use roz_core::embodiment::projection::project_capabilities;
use roz_core::embodiment::test_fixtures::manipulator_runtime;

#[test]
fn manipulator_capability_publish_lossless_from_runtime() {
    // 6-joint UR5-style fixture; the test_fixtures helper covers joints +
    // limits exhaustively. TCPs/sensors/workspace coverage lives in the
    // roz-core projection unit tests (which extend the fixture inline) so
    // this integration assertion focuses on the joint-descriptor contract
    // any consumer of `caps.{joints,joint_descriptors}` depends on.
    let rt = manipulator_runtime(6, 3.14, 3.14);
    let caps = project_capabilities(Some(&rt), 1.5);
    assert_eq!(
        caps.joint_descriptors.len(),
        6,
        "6-joint manipulator must surface 6 typed joint descriptors"
    );
    for jd in &caps.joint_descriptors {
        assert!(
            jd.max_velocity.is_finite(),
            "joint max_velocity must be finite (manifest never synthesizes NaN/inf)"
        );
        assert!(
            jd.position_min < jd.position_max,
            "joint position bounds must be non-degenerate"
        );
        assert!(
            !jd.joint_type.is_empty(),
            "joint_type string must be populated (e.g. \"revolute\")"
        );
    }
    // Legacy field still populated (wire-compat for clients that read names
    // out of `caps.joints` rather than the typed descriptor vector).
    assert_eq!(caps.joints.len(), 6);
    assert!((caps.max_velocity - 1.5).abs() < f64::EPSILON);
}

#[test]
fn capability_publish_falls_back_when_runtime_absent() {
    // Worker startup path when no robot.toml is configured — capability
    // publish must still emit a valid `RobotCapabilities` with the configured
    // velocity cap and empty descriptor vectors. Wire-format stays identical
    // to the pre-FW-06 baseline (descriptor keys omitted via
    // `skip_serializing_if`).
    let caps = project_capabilities(None, 1.5);
    assert!(caps.joint_descriptors.is_empty());
    assert!(caps.tcp_descriptors.is_empty());
    assert!(caps.sensor_mount_descriptors.is_empty());
    assert!(caps.workspace_zone_descriptors.is_empty());
    assert!((caps.max_velocity - 1.5).abs() < f64::EPSILON);

    // Wire-compat assertion: legacy clients see no descriptor keys in JSON.
    let json = serde_json::to_value(&caps).unwrap();
    assert!(json.get("joint_descriptors").is_none());
    assert!(json.get("tcp_descriptors").is_none());
    assert!(json.get("sensor_mount_descriptors").is_none());
    assert!(json.get("workspace_zone_descriptors").is_none());
}

#[test]
fn reference_manipulator_robot_toml_projects_full_descriptor_set() {
    // Phase 26.10 Plan 09 (Task 3) — `examples/reference_manipulator/robot.toml`
    // is the reference manipulator fixture used by the live-matrix manipulator
    // row. This test asserts the manifest parses through `EmbodimentManifest::load`
    // and then through `project_capabilities` produces the lossless typed
    // descriptor set substrate-ide depends on (6 joints, ≥1 TCP, ≥1 sensor
    // mount, ≥1 workspace zone — matches the plan's acceptance criteria for
    // the fixture).
    let workspace_root = std::env::var("CARGO_MANIFEST_DIR")
        .map(|d| std::path::PathBuf::from(d).join("../..").canonicalize().unwrap())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let path = workspace_root.join("examples/reference_manipulator/robot.toml");
    let manifest = roz_core::manifest::EmbodimentManifest::load(&path)
        .expect("examples/reference_manipulator/robot.toml must parse via EmbodimentManifest::load");
    // Authoritative path (FW-06 Plan 02 M1) — uses `[[joints]]` block, real
    // limits, plus `[[tcps]]` / `[[sensor_mounts]]` / `[[workspace_zones]]`.
    // The legacy `embodiment_runtime()` synthesizing path drops these and is
    // not the right surface for the H5 lossless contract.
    let runtime = manifest
        .try_authoritative_embodiment_runtime()
        .expect("reference manipulator fixture has explicit [[joints]] schema");
    assert_eq!(
        runtime.model.joints.len(),
        6,
        "fixture is a 6-DOF reference manipulator"
    );
    assert!(
        !runtime.model.tcps.is_empty(),
        "fixture must declare at least one TCP (gripper)"
    );
    assert!(
        !runtime.model.sensor_mounts.is_empty(),
        "fixture must declare at least one sensor mount"
    );
    assert!(
        !runtime.model.workspace_zones.is_empty(),
        "fixture must declare at least one workspace zone"
    );

    let caps = project_capabilities(Some(&runtime), 1.5);
    assert_eq!(caps.joint_descriptors.len(), 6);
    assert!(!caps.tcp_descriptors.is_empty());
    assert!(!caps.sensor_mount_descriptors.is_empty());
    assert!(!caps.workspace_zone_descriptors.is_empty());
}

#[test]
fn capability_publish_serde_roundtrip_includes_descriptors_when_populated() {
    // Defensive — when descriptors ARE populated, the JSON wire format must
    // round-trip lossless. This is the wire contract substrate-ide depends on
    // to render manipulator joints with real limits.
    let rt = manipulator_runtime(2, 1.0, 1.0);
    let caps = project_capabilities(Some(&rt), 1.5);
    let json = serde_json::to_string(&caps).expect("RobotCapabilities must serialize");
    let back: roz_core::capabilities::RobotCapabilities =
        serde_json::from_str(&json).expect("JSON round-trip must succeed");
    assert_eq!(back.joint_descriptors.len(), 2);
    assert_eq!(back.joint_descriptors[0].name, caps.joint_descriptors[0].name);
    assert!((back.joint_descriptors[0].max_velocity - 1.0).abs() < f64::EPSILON);
}
