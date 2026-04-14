#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::approx_constant,
    clippy::doc_markdown,
    clippy::ignore_without_reason,
    clippy::large_enum_variant,
    clippy::missing_const_for_fn,
    clippy::or_fun_call,
    clippy::struct_excessive_bools,
    clippy::type_complexity,
    clippy::derive_partial_eq_without_eq,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::format_collect,
    reason = "test-only style/complexity lints; tech-debt follow-up"
)]
use roz_core::controller::artifact::{ControllerArtifact, ControllerClass, ExecutionMode, SourceKind, VerificationKey};
use roz_core::controller::verification::VerifierVerdict;
use roz_core::embodiment::binding::ControlInterfaceManifest;
use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime, FrameSource, Joint, Link, Transform3D};
use sha2::{Digest, Sha256};

const LIVE_WIT_WORLD: &str = "live-controller";
const LIVE_WIT_WORLD_VERSION: &str = "bedrock:controller@1.0.0";
const LIVE_COMPILER_VERSION: &str = "wasmtime";
const LIVE_CHANNEL_MANIFEST_VERSION: u32 = 1;
const LIVE_HOST_ABI_VERSION: u32 = 2;

pub fn compile_test_embodiment_runtime(control_manifest: &ControlInterfaceManifest) -> EmbodimentRuntime {
    let mut frame_tree = roz_core::embodiment::FrameTree::new();
    frame_tree.set_root("world", FrameSource::Static);

    let mut links = vec![Link {
        name: "world".into(),
        parent_joint: None,
        inertial: None,
        visual_geometry: None,
        collision_geometry: None,
    }];
    let mut watched_frames = Vec::new();
    let mut seen_frames = std::collections::BTreeSet::new();

    for frame_id in control_manifest
        .channels
        .iter()
        .map(|channel| channel.frame_id.as_str())
        .chain(
            control_manifest
                .bindings
                .iter()
                .map(|binding| binding.frame_id.as_str()),
        )
    {
        if frame_id.is_empty() || !seen_frames.insert(frame_id.to_string()) {
            continue;
        }
        let _ = frame_tree.add_frame(frame_id, "world", Transform3D::identity(), FrameSource::Dynamic);
        links.push(Link {
            name: frame_id.to_string(),
            parent_joint: None,
            inertial: None,
            visual_geometry: None,
            collision_geometry: None,
        });
        watched_frames.push(frame_id.to_string());
    }

    // Some test manifests, especially drone/body-velocity fixtures, do not
    // declare per-channel frame ids. The live path still requires an explicit
    // watched-frame declaration, so fall back to the already-declared root.
    if watched_frames.is_empty() {
        watched_frames.push("world".into());
    }

    let model = EmbodimentModel {
        model_id: "roz-copper-live-test".into(),
        model_digest: String::new(),
        embodiment_family: None,
        links,
        joints: Vec::<Joint>::new(),
        frame_tree,
        collision_bodies: Vec::new(),
        allowed_collision_pairs: Vec::new(),
        tcps: Vec::new(),
        sensor_mounts: Vec::new(),
        workspace_zones: Vec::new(),
        watched_frames,
        channel_bindings: control_manifest.bindings.clone(),
    };

    EmbodimentRuntime::compile(model, None, None)
}

pub fn constant_output_controller_wat(command_values: &[f64]) -> String {
    let mut result_record = Vec::new();
    result_record.extend_from_slice(&(64u32).to_le_bytes());
    result_record.extend_from_slice(&(command_values.len() as u32).to_le_bytes());
    let result_record = escape_bytes(&result_record);
    let command_bytes: Vec<u8> = command_values.iter().flat_map(|value| value.to_le_bytes()).collect();
    let command_bytes = escape_bytes(&command_bytes);

    format!(
        r#"(module
            (type (func (result i32)))
            (type (func (param i32) (result i32)))
            (type (func (param i32)))
            (type (func (param i32 i32 i32 i32) (result i32)))
            (type (func))
            (import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode" (func $current_execution_mode (type 0)))
            (memory (export "cm32p2_memory") 1)
            (global $heap (mut i32) (i32.const 1024))
            (data (i32.const 0) "{result_record}")
            (data (i32.const 64) "{command_bytes}")
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
        )"#
    )
}

pub fn build_live_artifact(
    controller_id: &str,
    source_bytes: &[u8],
    control_manifest: &ControlInterfaceManifest,
    embodiment_runtime: &EmbodimentRuntime,
) -> (ControllerArtifact, Vec<u8>) {
    let component_bytes = roz_copper::wasm::CuWasmTask::canonical_live_component_bytes(source_bytes, control_manifest)
        .expect("componentize live-controller test source");
    let code_sha256 = hex::encode(Sha256::digest(&component_bytes));
    let artifact = ControllerArtifact {
        controller_id: controller_id.into(),
        sha256: code_sha256.clone(),
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
            controller_digest: code_sha256,
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
            evidence_summary: "live test artifact".into(),
        }),
    };

    (artifact, component_bytes)
}

fn escape_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}
