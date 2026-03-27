//! Copper application wiring and sim-mode execution.
//!
//! Uses the `#[copper_runtime]` proc-macro to generate a statically
//! scheduled task graph from [`copperconfig.ron`](../../copperconfig.ron).
//! The graph connects [`HeartbeatSource`] to [`LogSink`] via a `u64`
//! message channel -- the simplest possible Copper pipeline.
//!
//! The [`run_sim_ticks`] function drives the runtime through N iterations
//! in simulation mode, used by the CI harness to verify the pipeline is
//! wired correctly.

// The `copper_runtime` macro generates code that trips various clippy lints.
// Suppress them here since we cannot control the macro output.
#![allow(
    clippy::useless_let_if_seq,
    unused_imports,
    clippy::needless_pass_by_value,
    clippy::missing_const_for_fn
)]

use std::sync::{Arc, Mutex};

use crate::tasks::{HeartbeatSource, LogSink};
use cu29::prelude::*;
use cu29_derive::copper_runtime;

#[copper_runtime(config = "copperconfig.ron", sim_mode = true, ignore_resources = true)]
struct RozApp {}

/// Monotonic counter to give each logger invocation a unique path within the
/// same process (tests run in-process with the same PID).
static LOG_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Build a temporary mmap-backed unified logger for CI/test use.
///
/// Uses `std::env::temp_dir()` with PID + atomic counter to guarantee
/// unique paths across concurrent invocations within the same process.
fn build_temp_logger() -> anyhow::Result<Arc<Mutex<UnifiedLoggerWrite>>> {
    let tmp_dir = std::env::temp_dir().join("roz-copper-ci");
    std::fs::create_dir_all(&tmp_dir)?;

    let seq = LOG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let log_path = tmp_dir.join(format!("ci_sim_{}_{seq}.copper", std::process::id()));

    // Remove any stale alias/segment files from a previous run.
    let _ = std::fs::remove_file(&log_path);
    let segment = tmp_dir.join(format!("ci_sim_{}_{seq}_0.copper", std::process::id()));
    let _ = std::fs::remove_file(&segment);

    let UnifiedLogger::Write(writer) = UnifiedLoggerBuilder::new()
        .write(true)
        .create(true)
        .preallocated_size(16 * 1024 * 1024)
        .file_base_name(&log_path)
        .build()
        .map_err(|e| anyhow::anyhow!("unified logger init failed: {e}"))?
    else {
        anyhow::bail!("unified logger builder did not return a writer");
    };

    Ok(Arc::new(Mutex::new(writer)))
}

/// Run the Copper task graph for `ticks` iterations in sim mode.
///
/// Creates a temporary unified log, instantiates `RozApp` via the generated
/// builder, then calls `start_all_tasks` / `run_one_iteration` (N times) /
/// `stop_all_tasks`.
///
/// All tasks in `copperconfig.ron` have `run_in_sim: true`, so the real
/// [`HeartbeatSource`] and [`LogSink`] implementations execute on each tick.
///
/// # Errors
///
/// Returns an error if the runtime fails to initialize, any tick returns an
/// error, or the unified logger cannot be created.
///
/// # Future work
///
/// More sophisticated sim mode (virtual clock advancement, input injection,
/// output assertion) will build on this foundation.
pub fn run_sim_ticks(ticks: u32) -> anyhow::Result<()> {
    let logger = build_temp_logger()?;
    let (clock, _mock) = RobotClock::mock();

    // The sim callback lets every step execute via the real runtime.
    // Both tasks have `run_in_sim: true` so no simulation stubs are needed.
    let mut sim_cb = |_step: default::SimStep<'_>| -> SimOverride { SimOverride::ExecuteByRuntime };

    let mut app = RozAppBuilder::new()
        .with_clock(clock)
        .with_unified_logger(logger)
        .with_sim_callback(&mut sim_cb)
        .build()
        .map_err(|e| anyhow::anyhow!("RozApp build failed: {e}"))?;

    app.start_all_tasks(&mut sim_cb)
        .map_err(|e| anyhow::anyhow!("start_all_tasks failed: {e}"))?;

    for tick in 0..ticks {
        app.run_one_iteration(&mut sim_cb)
            .map_err(|e| anyhow::anyhow!("tick {tick} failed: {e}"))?;
    }

    app.stop_all_tasks(&mut sim_cb)
        .map_err(|e| anyhow::anyhow!("stop_all_tasks failed: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_sim_ticks_completes() {
        let result = run_sim_ticks(10);
        assert!(result.is_ok(), "10 sim ticks should complete: {result:?}");
    }

    #[test]
    fn run_sim_zero_ticks_completes() {
        let result = run_sim_ticks(0);
        assert!(
            result.is_ok(),
            "0 sim ticks (start/stop only) should complete: {result:?}"
        );
    }
}
