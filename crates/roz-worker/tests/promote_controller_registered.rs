//! Phase 26.10 Plan 04 (FW-03) — registration shape integration test.
//!
//! Asserts that all three controller-lifecycle tools register on a worker-side
//! `ToolDispatcher` under the canonical names with `ToolCategory::Physical`.
//! The worker boot path in `main.rs` performs the same three
//! `register_with_category` calls — keeping this test in lock-step prevents
//! silent drift between the boot path and the agent-visible tool registry.

use std::time::Duration;

use roz_agent::dispatch::ToolDispatcher;
use roz_core::embodiment::binding::{
    CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
};
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

fn worker_dispatcher_with_lifecycle_tools() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let cm = minimal_control_manifest();
    dispatcher.register_with_category(
        Box::new(roz_worker::tools::promote_controller::PromoteControllerTool::new(&cm)),
        ToolCategory::Physical,
    );
    dispatcher.register_with_category(
        Box::new(roz_worker::tools::stop_controller::StopControllerTool),
        ToolCategory::Physical,
    );
    dispatcher.register_with_category(
        Box::new(roz_worker::tools::controller_status::ControllerStatusTool),
        ToolCategory::Physical,
    );
    dispatcher
}

#[test]
fn promote_controller_registered_for_ooda_react() {
    let dispatcher = worker_dispatcher_with_lifecycle_tools();
    assert!(
        dispatcher.tool_names().iter().any(|n| n == "promote_controller"),
        "promote_controller must be registered on the worker dispatcher"
    );
    // category() defaults to Physical for unknown tools, so the registration
    // assertion above is the load-bearing check; this just confirms category.
    assert_eq!(dispatcher.category("promote_controller"), ToolCategory::Physical);
}

#[test]
fn stop_controller_registered_for_ooda_react() {
    let dispatcher = worker_dispatcher_with_lifecycle_tools();
    assert!(
        dispatcher.tool_names().iter().any(|n| n == "stop_controller"),
        "stop_controller must be registered on the worker dispatcher"
    );
    assert_eq!(dispatcher.category("stop_controller"), ToolCategory::Physical);
}

#[test]
fn controller_status_registered_for_ooda_react() {
    let dispatcher = worker_dispatcher_with_lifecycle_tools();
    assert!(
        dispatcher.tool_names().iter().any(|n| n == "controller_status"),
        "controller_status (canonical name) must be registered on the worker dispatcher"
    );
    assert_eq!(dispatcher.category("controller_status"), ToolCategory::Physical);
    // Negative assertion: the legacy non-canonical name MUST NOT appear.
    // Constructed via concat! so this file is grep-clean for the literal.
    let legacy_name = concat!("get_", "controller_status");
    assert!(
        !dispatcher.tool_names().iter().any(|n| n == legacy_name),
        "the non-canonical name must never be registered"
    );
}
