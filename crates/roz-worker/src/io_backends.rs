//! FW-02 (Codex H2 fix) — Concrete IO backend factory dispatch.
//!
//! Located in roz-worker (NOT roz-copper) to avoid the cargo cycle:
//! the MAVLink driver crate already depends on roz-copper for
//! `ActuatorSink`/`SensorSource`, so a copper -> driver edge would cycle.
//! roz-worker depends on both crates without cycle.
//!
//! Plan 08 replaces `ManipulatorStubFactory` with a real `FakeOpenclawFactory`
//! (gated `test-fixtures` feature). MAVLink wiring lives in Phase 27 SC5/SC6/SC7.

use std::sync::Arc;

use roz_copper::io::{ActuatorSink, SensorSource};
use roz_copper::io_factory::IoFactory;
use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::ControlInterfaceManifest;

/// Static dispatch table. Adding a new backend is one match arm + one impl block.
#[must_use]
pub fn factory_for(family: Option<&str>) -> Option<Box<dyn IoFactory>> {
    match family {
        Some("openclaw" | "manipulator") => Some(Box::new(ManipulatorStubFactory)),
        Some("drone-mavlink") => Some(Box::new(MavlinkFactory)),
        _ => None,
    }
}

/// Stub for the manipulator family. Plan 08 (FW-07) replaces this with a real
/// `FakeOpenclawFactory` that wraps the deterministic fake-OpenClaw backend.
/// The replacement is in this same file (no caller changes — `factory_for`
/// continues to dispatch through `Some("manipulator") | Some("openclaw")`).
pub(crate) struct ManipulatorStubFactory;
impl IoFactory for ManipulatorStubFactory {
    fn build(
        &self,
        _runtime: &EmbodimentRuntime,
        _control_manifest: &ControlInterfaceManifest,
    ) -> anyhow::Result<(Arc<dyn ActuatorSink>, Box<dyn SensorSource>)> {
        anyhow::bail!(
            "manipulator IoFactory stub — Plan 26.10-08 must wire the deterministic \
             fake-OpenClaw backend (or a real Dynamixel/OpenCR backend) before this path \
             can spawn a controller"
        )
    }
    fn name(&self) -> &'static str {
        "manipulator-stub"
    }
}

/// Drone path deferred for 26.10 — Phase 27 SC5/SC6/SC7 owns the live-FCU
/// MAVLink IoFactory (PX4 SITL is the validation target there). The 26.10
/// critical chain is manipulator framework-fidelity; the drone branch is
/// reachable through `factory_for` so the dispatch table is exhaustive,
/// but `build` fails closed so no production binary can spawn a controller
/// on top of an un-wired MAVLink backend.
pub(crate) struct MavlinkFactory;
impl IoFactory for MavlinkFactory {
    fn build(
        &self,
        _runtime: &EmbodimentRuntime,
        _control_manifest: &ControlInterfaceManifest,
    ) -> anyhow::Result<(Arc<dyn ActuatorSink>, Box<dyn SensorSource>)> {
        anyhow::bail!("MAVLink IoFactory deferred — see Phase 27 SC5/SC6/SC7 live-FCU work")
    }
    fn name(&self) -> &'static str {
        "drone-mavlink-deferred"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `EmbodimentRuntime` via the canonical compile path.
    /// Mirrors `crates/roz-nats/src/dispatch.rs:681` `minimal_runtime()`.
    /// `EmbodimentRuntime` does NOT implement `Default` — `compile()` is the
    /// only canonical constructor. For the Err-path tests below the runtime
    /// CONTENTS do not matter; the factory body never reads them.
    fn minimal_runtime() -> EmbodimentRuntime {
        use roz_core::embodiment::EmbodimentModel;
        use roz_core::embodiment::frame_tree::{FrameSource, FrameTree};
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        let mut model = EmbodimentModel {
            model_id: "fw02-test-v1".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![],
            joints: vec![],
            frame_tree: tree,
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![],
            sensor_mounts: vec![],
            workspace_zones: vec![],
            watched_frames: vec!["world".into()],
            channel_bindings: vec![],
        };
        model.stamp_digest();
        EmbodimentRuntime::compile(model, None, None)
    }

    /// Build a minimal `ControlInterfaceManifest`. All fields are public; no
    /// `Default` impl exists, so we construct directly with empty channels +
    /// bindings. Factory body never reads it on the Err paths.
    fn minimal_manifest() -> ControlInterfaceManifest {
        ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![],
            bindings: vec![],
        }
    }

    #[test]
    fn factory_for_returns_none_for_unknown() {
        assert!(factory_for(None).is_none());
        assert!(factory_for(Some("unknown")).is_none());
        assert!(factory_for(Some("")).is_none());
    }

    #[test]
    fn factory_for_returns_some_for_manipulator() {
        let f = factory_for(Some("manipulator")).expect("manipulator factory exists");
        assert_eq!(f.name(), "manipulator-stub");
        let f2 = factory_for(Some("openclaw")).expect("openclaw alias factory exists");
        assert_eq!(f2.name(), "manipulator-stub");
    }

    #[test]
    fn factory_for_returns_some_for_drone_mavlink() {
        let f = factory_for(Some("drone-mavlink")).expect("mavlink factory exists");
        assert_eq!(f.name(), "drone-mavlink-deferred");
    }

    #[test]
    fn mavlink_factory_build_returns_err_deferred() {
        let factory = factory_for(Some("drone-mavlink")).unwrap();
        let runtime = minimal_runtime();
        let manifest = minimal_manifest();
        let result = factory.build(&runtime, &manifest);
        let err = match result {
            Ok(_) => panic!("expected Err"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("MAVLink IoFactory deferred"),
            "expected deferred-message, got: {msg}"
        );
    }

    #[test]
    fn manipulator_factory_build_returns_err_stub() {
        let factory = factory_for(Some("manipulator")).unwrap();
        let runtime = minimal_runtime();
        let manifest = minimal_manifest();
        let result = factory.build(&runtime, &manifest);
        let err = match result {
            Ok(_) => panic!("expected Err"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("manipulator IoFactory stub"),
            "expected stub-message, got: {msg}"
        );
    }
}
