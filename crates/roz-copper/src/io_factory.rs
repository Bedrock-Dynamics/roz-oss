//! FW-02 — IO factory trait surface for embodiment-keyed actuator/sensor backends.
//!
//! This module defines the *contract* between Copper and any IO backend.
//! Concrete backend registration + dispatch lives in `roz-worker`
//! (`crates/roz-worker/src/io_backends.rs`) — placing it here would require
//! `roz-copper → roz-mavlink`, which cycles since `roz-mavlink` already
//! depends on `roz-copper` for `ActuatorSink`/`SensorSource` traits.
//!
//! CRITICAL: returned actuator and sensor MUST share state — see
//! `roz_mavlink::backend::MavlinkBackend` for the canonical pattern.

use std::sync::Arc;

use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::ControlInterfaceManifest;

use crate::io::{ActuatorSink, SensorSource};

/// Factory for embodiment-keyed IO bindings. Adding a new robot family is
/// `impl IoFactory for X` plus one match arm in `roz_worker::io_backends::factory_for`.
pub trait IoFactory: Send + Sync {
    fn build(
        &self,
        runtime: &EmbodimentRuntime,
        control_manifest: &ControlInterfaceManifest,
    ) -> anyhow::Result<(Arc<dyn ActuatorSink>, Box<dyn SensorSource>)>;

    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopFactory;
    impl IoFactory for NoopFactory {
        fn build(
            &self,
            _runtime: &EmbodimentRuntime,
            _control_manifest: &ControlInterfaceManifest,
        ) -> anyhow::Result<(Arc<dyn ActuatorSink>, Box<dyn SensorSource>)> {
            anyhow::bail!("noop factory")
        }
        fn name(&self) -> &'static str {
            "noop"
        }
    }

    #[test]
    fn io_factory_trait_is_object_safe() {
        // Constructing Box<dyn IoFactory> compiles iff the trait is object-safe.
        let _factory: Box<dyn IoFactory> = Box::new(NoopFactory);
    }

    #[test]
    fn roz_copper_does_not_depend_on_roz_mavlink() {
        // Codex H2 guard — prevents reintroduction of the cargo cycle.
        let cargo_toml = include_str!("../Cargo.toml");
        // Look for dep declaration shapes: `roz-mavlink = ` or `name = "roz-mavlink"` in [dependencies.X]
        assert!(
            !cargo_toml.contains("roz-mavlink"),
            "roz-copper/Cargo.toml MUST NOT depend on roz-mavlink — cycle prevention. \
             Move concrete factory selection to roz-worker per Plan 03 H2 fix."
        );
    }
}
