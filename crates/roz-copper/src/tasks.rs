//! Task trait re-exports and roz-specific helpers.

// Re-export core Copper traits for downstream use.
pub use cu29::prelude::*;

/// Minimal source task used by the `copper-runtime` feature's sim app.
#[derive(Debug, Default, Reflect)]
pub struct HeartbeatSource {
    tick: u64,
}

impl Freezable for HeartbeatSource {}

impl CuSrcTask for HeartbeatSource {
    type Output<'m> = CuMsg<u64>;
    type Resources<'r> = ();

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self>
    where
        Self: Sized,
    {
        Ok(Self::default())
    }

    fn process<'o>(&mut self, _ctx: &CuContext, output: &mut Self::Output<'o>) -> CuResult<()> {
        self.tick = self.tick.saturating_add(1);
        output.set_payload(self.tick);
        output.metadata.set_status("heartbeat");
        Ok(())
    }
}

/// Minimal sink task used by the `copper-runtime` feature's sim app.
#[derive(Debug, Default, Reflect)]
pub struct LogSink {
    received: u64,
    last_tick: u64,
}

impl Freezable for LogSink {}

impl CuSinkTask for LogSink {
    type Input<'m> = CuMsg<u64>;
    type Resources<'r> = ();

    fn new(_config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self>
    where
        Self: Sized,
    {
        Ok(Self::default())
    }

    fn process<'i>(&mut self, _ctx: &CuContext, input: &Self::Input<'i>) -> CuResult<()> {
        if let Some(tick) = input.payload() {
            self.received = self.received.saturating_add(1);
            self.last_tick = *tick;
        }
        Ok(())
    }
}
