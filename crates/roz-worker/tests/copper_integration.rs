//! Integration test: agent command → Copper controller → state feedback.

use std::sync::Arc;
use std::time::Duration;

use roz_agent::spatial_provider::SpatialContextProvider;
use roz_copper::channels::ControllerCommand;
use roz_core::controller::artifact::{ControllerArtifact, ControllerClass, ExecutionMode, SourceKind, VerificationKey};
use roz_core::controller::verification::VerifierVerdict;
use roz_core::embodiment::binding::{BindingType, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest};
use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime, FrameSource, Joint, Link, Transform3D};
use sha2::{Digest, Sha256};

const LIVE_WIT_WORLD: &str = "live-controller";
const LIVE_WIT_WORLD_VERSION: &str = "bedrock:controller@1.0.0";
const LIVE_COMPILER_VERSION: &str = "wasmtime";
const LIVE_CHANNEL_MANIFEST_VERSION: u32 = 1;
const LIVE_HOST_ABI_VERSION: u32 = 2;

fn test_control_manifest() -> ControlInterfaceManifest {
    let mut control_manifest = ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: vec![ControlChannelDef {
            name: "joint0/velocity".into(),
            interface_type: CommandInterfaceType::JointVelocity,
            units: "rad/s".into(),
            frame_id: "joint0_link".into(),
        }],
        bindings: vec![roz_core::embodiment::binding::ChannelBinding {
            physical_name: "joint0".into(),
            channel_index: 0,
            binding_type: BindingType::JointVelocity,
            frame_id: "joint0_link".into(),
            units: "rad/s".into(),
            semantic_role: None,
        }],
    };
    control_manifest.stamp_digest();
    control_manifest
}

fn compile_test_embodiment_runtime(control_manifest: &ControlInterfaceManifest) -> EmbodimentRuntime {
    let mut frame_tree = roz_core::embodiment::FrameTree::new();
    frame_tree.set_root("world", FrameSource::Static);
    frame_tree
        .add_frame("joint0_link", "world", Transform3D::identity(), FrameSource::Dynamic)
        .expect("add joint frame");

    let model = EmbodimentModel {
        model_id: "worker-copper-integration".into(),
        model_digest: String::new(),
        embodiment_family: None,
        links: vec![
            Link {
                name: "world".into(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            },
            Link {
                name: "joint0_link".into(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            },
        ],
        joints: Vec::<Joint>::new(),
        frame_tree,
        collision_bodies: Vec::new(),
        allowed_collision_pairs: Vec::new(),
        tcps: Vec::new(),
        sensor_mounts: Vec::new(),
        workspace_zones: Vec::new(),
        watched_frames: vec!["joint0_link".into()],
        channel_bindings: control_manifest.bindings.clone(),
    };

    EmbodimentRuntime::compile(model, None, None)
}

fn build_live_artifact(
    controller_id: &str,
    source_bytes: &[u8],
    control_manifest: &ControlInterfaceManifest,
    embodiment_runtime: &EmbodimentRuntime,
) -> (ControllerArtifact, Vec<u8>) {
    let component_bytes = roz_copper::wasm::CuWasmTask::canonical_live_component_bytes(source_bytes, control_manifest)
        .expect("componentize integration-test controller");
    let controller_digest = hex::encode(Sha256::digest(&component_bytes));
    let artifact = ControllerArtifact {
        controller_id: controller_id.into(),
        sha256: controller_digest.clone(),
        source_kind: SourceKind::LlmGenerated,
        controller_class: ControllerClass::LowRiskCommandGenerator,
        generator_model: None,
        generator_provider: None,
        channel_manifest_version: LIVE_CHANNEL_MANIFEST_VERSION,
        host_abi_version: LIVE_HOST_ABI_VERSION,
        evidence_bundle_id: None,
        created_at: chrono::Utc::now(),
        promoted_at: None,
        replaced_controller_id: None,
        verification_key: VerificationKey {
            controller_digest,
            wit_world_version: LIVE_WIT_WORLD_VERSION.into(),
            model_digest: embodiment_runtime.model_digest.clone(),
            calibration_digest: embodiment_runtime.calibration_digest.clone(),
            manifest_digest: control_manifest.manifest_digest.clone(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: LIVE_COMPILER_VERSION.into(),
            embodiment_family: embodiment_runtime
                .model
                .embodiment_family
                .as_ref()
                .map(|family| format!("{family:?}")),
        },
        wit_world: LIVE_WIT_WORLD.into(),
        verifier_result: Some(VerifierVerdict::Pass {
            evidence_summary: "worker copper integration test artifact".into(),
        }),
    };

    (artifact, component_bytes)
}

#[tokio::test]
async fn agent_deploys_wasm_to_copper_and_reads_state() {
    // Spawn Copper controller.
    let handle = roz_worker::copper_handle::CopperHandle::spawn(1.5);

    // Verify starts idle.
    let state = handle.state().load();
    assert!(!state.running, "should start idle");

    // Agent deploys a WASM controller via artifact.
    let wat = r#"
        (module
            (type (func (result i32)))
            (type (func (param i32) (result i32)))
            (type (func (param i32)))
            (type (func (param i32 i32 i32 i32) (result i32)))
            (type (func))
            (import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode" (func $current_execution_mode (type 0)))
            (memory (export "cm32p2_memory") 1)
            (global $heap (mut i32) (i32.const 1024))
            (data (i32.const 0) "\40\00\00\00\01\00\00\00")
            (data (i32.const 64) "\00\00\00\00\00\00\00\00")
            (func (export "cm32p2|bedrock:controller/control@1|process") (type 1) (param $input i32) (result i32)
                (i32.const 0)
            )
            (func (export "cm32p2|bedrock:controller/control@1|process_post") (type 2) (param $result i32)
                (global.set $heap (i32.const 1024))
            )
            (func (export "cm32p2_realloc") (type 3) (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
                (local $ptr i32)
                global.get $heap
                local.get $align
                i32.const 1
                i32.sub
                i32.add
                local.get $align
                i32.const 1
                i32.sub
                i32.const -1
                i32.xor
                i32.and
                local.tee $ptr
                local.get $new_size
                i32.add
                global.set $heap
                local.get $ptr
            )
            (func (export "cm32p2_initialize") (type 4))
        )
    "#;
    let control_manifest = test_control_manifest();
    let embodiment_runtime = compile_test_embodiment_runtime(&control_manifest);
    let (artifact, component_bytes) = build_live_artifact(
        "integration-test",
        wat.as_bytes(),
        &control_manifest,
        &embodiment_runtime,
    );
    handle
        .send(ControllerCommand::load_artifact_with_embodiment_runtime(
            artifact,
            component_bytes,
            &control_manifest,
            &embodiment_runtime,
        ))
        .await
        .unwrap();

    // Wait for some ticks.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Read state via CopperSpatialProvider (same path the agent uses).
    let provider = roz_worker::spatial_bridge::CopperSpatialProvider::new(Arc::clone(handle.state()));
    let ctx = provider.snapshot("integration-test").await;

    let controller = ctx
        .entities
        .iter()
        .find(|e| e.id == "copper_controller")
        .expect("should have copper_controller entity");

    assert_eq!(controller.properties.get("running"), Some(&serde_json::json!(true)));

    let last_tick = controller
        .properties
        .get("last_tick")
        .and_then(serde_json::Value::as_u64)
        .unwrap();
    assert!(last_tick > 10, "should have ticked many times: {last_tick}");

    // Agent halts the controller.
    handle.send(ControllerCommand::Halt).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    let ctx = provider.snapshot("integration-test").await;
    let controller = ctx.entities.iter().find(|e| e.id == "copper_controller").unwrap();
    assert_eq!(controller.properties.get("running"), Some(&serde_json::json!(false)));

    handle.shutdown().await;
}
