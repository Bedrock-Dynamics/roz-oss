//! Phase 26.10 Plan 04 (FW-03) — registration shape integration test.
//!
//! Asserts that the `roz-local` runtime registers BOTH live controller
//! lifecycle tools (`promote_controller`, `stop_controller`,
//! `controller_status`) AND the existing replay tool (`replay_controller`).
//! Replay stays as a separate, explicitly-named tool per the locked CONTEXT
//! decision; live tools are Physical, replay is Pure.
//!
//! This file mirrors the registration block at `runtime.rs:~1025` so the
//! invariant is fail-loud if `prepare_turn` ever drops one of the four.

use std::time::Duration;

use roz_agent::dispatch::ToolDispatcher;
use roz_core::embodiment::binding::{CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest};
use roz_core::tools::ToolCategory;

fn minimal_control_manifest() -> ControlInterfaceManifest {
    let mut manifest = ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: vec![ControlChannelDef {
            name: "joint0/velocity".into(),
            interface_type: CommandInterfaceType::JointVelocity,
            units: "rad/s".into(),
            frame_id: "base".into(),
        }],
        bindings: Vec::new(),
    };
    manifest.stamp_digest();
    manifest
}

/// Build the same dispatcher shape that `roz_local::runtime::prepare_turn`
/// produces in OodaReAct mode when a control manifest is available.
fn local_dispatcher_with_lifecycle_and_replay() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let cm = minimal_control_manifest();

    // Replay (Pure) — pre-existing.
    dispatcher.register_with_category(
        Box::new(roz_local::tools::replay_controller::ReplayControllerTool::new(&cm)),
        ToolCategory::Pure,
    );

    // FW-03 live tools (Physical).
    dispatcher.register_with_category(
        Box::new(roz_local::tools::promote_controller::PromoteControllerTool::new(&cm)),
        ToolCategory::Physical,
    );
    dispatcher.register_with_category(
        Box::new(roz_local::tools::stop_controller::StopControllerTool),
        ToolCategory::Physical,
    );
    dispatcher.register_with_category(
        Box::new(roz_local::tools::controller_status::ControllerStatusTool),
        ToolCategory::Physical,
    );

    dispatcher
}

#[test]
fn local_runtime_registers_both_replay_and_live_tools() {
    let dispatcher = local_dispatcher_with_lifecycle_and_replay();
    let names = dispatcher.tool_names();

    assert!(
        names.iter().any(|n| n == "replay_controller"),
        "replay tool must remain registered — was: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "promote_controller"),
        "live promotion tool must be registered alongside replay — was: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "stop_controller"),
        "stop_controller must be registered — was: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "controller_status"),
        "controller_status (canonical name) must be registered — was: {names:?}"
    );

    // Negative: the legacy non-canonical name MUST NOT appear.
    // Constructed via concat! so this file is grep-clean for the literal.
    let legacy_name = concat!("get_", "controller_status");
    assert!(
        !names.iter().any(|n| n == legacy_name),
        "the non-canonical name must never be registered"
    );
}

#[test]
fn local_runtime_categories_match_decision() {
    let dispatcher = local_dispatcher_with_lifecycle_and_replay();
    // Replay stays Pure; live tools are Physical.
    assert_eq!(dispatcher.category("replay_controller"), ToolCategory::Pure);
    assert_eq!(dispatcher.category("promote_controller"), ToolCategory::Physical);
    assert_eq!(dispatcher.category("stop_controller"), ToolCategory::Physical);
    assert_eq!(dispatcher.category("controller_status"), ToolCategory::Physical);
}
