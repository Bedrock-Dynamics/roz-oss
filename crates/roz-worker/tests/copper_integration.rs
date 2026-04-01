//! Integration test: agent command → Copper controller → state feedback.

use std::sync::Arc;
use std::time::Duration;

use roz_agent::spatial_provider::SpatialContextProvider;
use roz_copper::channels::ControllerCommand;

fn test_artifact() -> roz_core::controller::artifact::ControllerArtifact {
    use roz_core::controller::artifact::*;
    ControllerArtifact {
        controller_id: "integration-test".into(),
        sha256: "test".into(),
        source_kind: SourceKind::LlmGenerated,
        controller_class: ControllerClass::LowRiskCommandGenerator,
        generator_model: None,
        generator_provider: None,
        channel_manifest_version: 1,
        host_abi_version: 2,
        evidence_bundle_id: None,
        created_at: chrono::Utc::now(),
        promoted_at: None,
        replaced_controller_id: None,
        verification_key: VerificationKey {
            controller_digest: "test".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            model_digest: "test".into(),
            calibration_digest: "test".into(),
            manifest_digest: "test".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime".into(),
            embodiment_family: None,
        },
        wit_world: "tick-controller".into(),
        verifier_result: None,
    }
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
            (global $tick_count (mut i64) (i64.const 0))
            (func (export "process") (param i64)
                (global.set $tick_count
                    (i64.add (global.get $tick_count) (i64.const 1))
                )
            )
        )
    "#;
    handle
        .send(ControllerCommand::LoadArtifact(
            Box::new(test_artifact()),
            wat.as_bytes().to_vec(),
            roz_core::channels::ChannelManifest::generic_velocity(1, 1.5),
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

    let last_tick = controller.properties.get("last_tick").and_then(|v| v.as_u64()).unwrap();
    assert!(last_tick > 10, "should have ticked many times: {last_tick}");

    // Agent halts the controller.
    handle.send(ControllerCommand::Halt).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    let ctx = provider.snapshot("integration-test").await;
    let controller = ctx.entities.iter().find(|e| e.id == "copper_controller").unwrap();
    assert_eq!(controller.properties.get("running"), Some(&serde_json::json!(false)));

    handle.shutdown().await;
}
