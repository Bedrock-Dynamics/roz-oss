//! FW-02 (Codex H2 fix) — Concrete IO backend factory dispatch.
//!
//! Located in roz-worker (NOT roz-copper) to avoid the cargo cycle:
//! the MAVLink driver crate already depends on roz-copper for
//! `ActuatorSink`/`SensorSource`, so a copper -> driver edge would cycle.
//! roz-worker depends on both crates without cycle.
//!
//! Phase 26.10 Plan 08 (FW-07) replaces Plan 03's deferred manipulator-stub
//! factory with `FakeOpenclawFactory`, which under the `test-fixtures` feature wires
//! `roz_copper::fake_openclaw::fake_openclaw_pair`. Without the feature,
//! `build()` returns `Err` so production binaries fail closed (T-26.10-08-04).
//! MAVLink wiring lives in Phase 27 SC5/SC6/SC7.

use std::sync::Arc;

use roz_copper::io::{ActuatorSink, SensorSource};
use roz_copper::io_factory::IoFactory;
use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::ControlInterfaceManifest;

/// Static dispatch table. Adding a new backend is one match arm + one impl block.
#[must_use]
pub fn factory_for(family: Option<&str>) -> Option<Box<dyn IoFactory>> {
    match family {
        Some("openclaw" | "manipulator") => Some(Box::new(FakeOpenclawFactory)),
        Some("drone-mavlink") => Some(Box::new(MavlinkFactory)),
        _ => None,
    }
}

/// Manipulator-family backend. Under the `test-fixtures` feature this wires
/// the deterministic fake-OpenClaw backend from `roz_copper::fake_openclaw`;
/// without the feature, `build()` returns `Err` so production binaries cannot
/// silently spawn a controller against a fake (T-26.10-08-04 mitigation).
///
/// Production manipulator backends (Dynamixel, OpenCR, real OpenClaw firmware)
/// are scope of a future phase; they will replace this `cfg(not(...))` branch
/// with their own real implementation.
pub(crate) struct FakeOpenclawFactory;

#[cfg(feature = "test-fixtures")]
impl IoFactory for FakeOpenclawFactory {
    fn build(
        &self,
        runtime: &EmbodimentRuntime,
        _control_manifest: &ControlInterfaceManifest,
    ) -> anyhow::Result<(Arc<dyn ActuatorSink>, Box<dyn SensorSource>)> {
        let (actuator, sensor) = roz_copper::fake_openclaw::fake_openclaw_pair(runtime);
        Ok((Arc::new(actuator), Box::new(sensor)))
    }
    fn name(&self) -> &'static str {
        "fake-openclaw"
    }
}

#[cfg(not(feature = "test-fixtures"))]
impl IoFactory for FakeOpenclawFactory {
    fn build(
        &self,
        _runtime: &EmbodimentRuntime,
        _control_manifest: &ControlInterfaceManifest,
    ) -> anyhow::Result<(Arc<dyn ActuatorSink>, Box<dyn SensorSource>)> {
        anyhow::bail!(
            "manipulator IoFactory: test-fixtures feature not enabled — production \
             manipulator backends (Dynamixel, OpenCR) are scope of a future phase. \
             Enable `roz-worker/test-fixtures` for the deterministic fake-OpenClaw \
             backend (CI / live-matrix only)."
        )
    }
    fn name(&self) -> &'static str {
        "fake-openclaw-stub"
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
    /// Plan 08 (FW-07) provides `roz_core::embodiment::test_fixtures::manipulator_runtime`
    /// when the `test-fixtures` feature is enabled — under that feature we
    /// route through the canonical helper. Without the feature, the body of
    /// the manipulator factory bails before reading the runtime, so a
    /// hand-built minimal model is sufficient.
    #[cfg(feature = "test-fixtures")]
    fn minimal_runtime() -> EmbodimentRuntime {
        roz_core::embodiment::test_fixtures::manipulator_runtime(2, 1.0, 3.14)
    }

    #[cfg(not(feature = "test-fixtures"))]
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
        let f2 = factory_for(Some("openclaw")).expect("openclaw alias factory exists");
        #[cfg(feature = "test-fixtures")]
        {
            assert_eq!(f.name(), "fake-openclaw");
            assert_eq!(f2.name(), "fake-openclaw");
        }
        #[cfg(not(feature = "test-fixtures"))]
        {
            assert_eq!(f.name(), "fake-openclaw-stub");
            assert_eq!(f2.name(), "fake-openclaw-stub");
        }
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

    /// Without the `test-fixtures` feature, the manipulator factory MUST return
    /// an Err so production binaries cannot spawn a controller through the fake
    /// (T-26.10-08-04 mitigation).
    #[cfg(not(feature = "test-fixtures"))]
    #[test]
    fn factory_for_manipulator_without_feature_errors() {
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
            msg.contains("test-fixtures feature not enabled"),
            "expected feature-disabled message, got: {msg}"
        );
    }

    /// With the `test-fixtures` feature, the manipulator factory MUST return a
    /// real paired actuator+sensor backed by `fake_openclaw_pair`. Sending a
    /// command through the actuator must be visible on the next sensor read.
    #[cfg(feature = "test-fixtures")]
    #[test]
    fn fake_openclaw_factory_build_returns_paired_io() {
        let runtime = minimal_runtime();
        let manifest = minimal_manifest();
        let factory = factory_for(Some("manipulator")).unwrap();
        let (actuator, mut sensor) = factory.build(&runtime, &manifest).unwrap();
        actuator
            .send(&roz_core::command::CommandFrame {
                values: vec![0.5, -0.25],
            })
            .unwrap();
        let frame = sensor.try_recv().expect("sensor frame");
        // Both joints have max_velocity = 1.0, so values are within saturation;
        // sensor frame echoes the (clamped) commanded velocities through the
        // shared-state path.
        assert!(
            (frame.joint_velocities[0] - 0.5).abs() < f64::EPSILON,
            "joint 0 velocity should reflect command: got {}",
            frame.joint_velocities[0]
        );
        assert!(
            (frame.joint_velocities[1] - -0.25).abs() < f64::EPSILON,
            "joint 1 velocity should reflect command: got {}",
            frame.joint_velocities[1]
        );
    }
}
