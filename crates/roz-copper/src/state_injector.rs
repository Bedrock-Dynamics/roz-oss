//! Trait for injecting sensor data into the WASM controller's HostContext.
//!
//! Implementations bridge hardware-specific sensor APIs to the generic
//! channel-based state interface. The controller loop calls `inject()`
//! before each WASM tick.

use crate::wit_host::HostContext;

/// Source of sensor data for the controller loop.
///
/// Implementations read from hardware (or simulation) and write into
/// the `HostContext`'s `state_values` array. Called once per tick
/// before the WASM controller runs.
pub trait StateInjector: Send {
    /// Inject the latest sensor readings into the host context.
    ///
    /// Implementors should:
    /// 1. Read the latest sensor data (non-blocking).
    /// 2. Write values into `ctx.state_values` by channel index.
    /// 3. Optionally update `ctx.sim_time_ns` for timing.
    fn inject(&mut self, ctx: &mut HostContext);
}

/// No-op injector for controllers that don't need sensor input.
pub struct NullStateInjector;

impl StateInjector for NullStateInjector {
    fn inject(&mut self, _ctx: &mut HostContext) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::channels::ChannelManifest;

    struct FakeInjector {
        values: Vec<f64>,
    }

    impl StateInjector for FakeInjector {
        fn inject(&mut self, ctx: &mut HostContext) {
            for (i, &v) in self.values.iter().enumerate() {
                if i < ctx.state_values.len() {
                    ctx.state_values[i] = v;
                }
            }
        }
    }

    #[test]
    fn fake_injector_writes_state_values() {
        let manifest = ChannelManifest::reachy_mini();
        let mut ctx = HostContext::with_manifest(manifest);
        assert!(ctx.state_values.iter().all(|&v| v == 0.0));

        let mut injector = FakeInjector {
            values: vec![0.01, 0.02, 0.0, 0.1, 0.2, 0.5, 0.3, 0.8, 0.8],
        };
        injector.inject(&mut ctx);

        assert!((ctx.state_values[0] - 0.01).abs() < f64::EPSILON);
        assert!((ctx.state_values[5] - 0.5).abs() < f64::EPSILON);
        assert!((ctx.state_values[8] - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn null_injector_is_noop() {
        let manifest = ChannelManifest::reachy_mini();
        let mut ctx = HostContext::with_manifest(manifest);
        let mut injector = NullStateInjector;
        injector.inject(&mut ctx);
        assert!(ctx.state_values.iter().all(|&v| v == 0.0));
    }
}
