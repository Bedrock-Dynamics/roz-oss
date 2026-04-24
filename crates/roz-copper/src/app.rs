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

/// Build a session-scoped mmap-backed unified logger (Phase 26.7 D-13/D-14).
///
/// Writes segments under `session_dir` as `session_N.copper`
/// (`session_0.copper`, `session_1.copper`, …) — cu29-unifiedlog 0.14
/// format `{stem}_{N}.{ext}`, NOT the dotted `session.copper.N` that
/// older CONTEXT.md drafts referenced; see Phase 26.7 RESEARCH.md
/// Discrepancy 1.
///
/// # Preallocation + rotation
/// `preallocated_size(preallocated_mb * MiB)` sizes the initial mmap region.
/// When it fills, cu29 rotates to the next numbered segment automatically.
///
/// # CRITICAL: drop-ordering invariant (Q1)
/// `UnifiedLoggerWrite` has NO public `flush`/`close`/`sync` method. Disk
/// sync happens EXCLUSIVELY on `Drop` (clears tmp markers, writes EoL
/// marker, garbage-collects back slabs). Callers that need to read or
/// upload the segment files (e.g. `finalize_copper_archive` in Plan 06)
/// MUST drop every `Arc<Mutex<UnifiedLoggerWrite>>` held for this
/// `session_dir` BEFORE reading from disk. Violating this invariant
/// produces truncated uploads with an otherwise-valid digest.
///
/// # Errors
/// Returns `anyhow::Error` if directory creation fails, cu29 init fails,
/// or the builder returns a reader variant instead of a writer.
pub fn build_session_logger(
    session_dir: &std::path::Path,
    preallocated_mb: usize,
) -> anyhow::Result<Arc<Mutex<UnifiedLoggerWrite>>> {
    std::fs::create_dir_all(session_dir)?;
    let log_path = session_dir.join("session.copper");

    let UnifiedLogger::Write(writer) = UnifiedLoggerBuilder::new()
        .write(true)
        .create(true)
        .preallocated_size(preallocated_mb * 1024 * 1024)
        .file_base_name(&log_path)
        .build()
        .map_err(|e| anyhow::anyhow!("unified logger init failed: {e}"))?
    else {
        anyhow::bail!("unified logger builder did not return a writer");
    };

    Ok(Arc::new(Mutex::new(writer)))
}

/// Build a temporary mmap-backed unified logger for CI/test use.
///
/// Delegates to [`build_session_logger`] with a unique tmp subdirectory per
/// process+counter tick and a 16 MiB region. Preserves the pre-26.7
/// concurrent-test behaviour — `run_sim_ticks(N)` callers get an isolated
/// `session_dir`.
fn build_temp_logger() -> anyhow::Result<Arc<Mutex<UnifiedLoggerWrite>>> {
    let seq = LOG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let session_dir = std::env::temp_dir()
        .join("roz-copper-ci")
        .join(format!("ci_sim_{}_{seq}", std::process::id()));
    // Defensive cleanup: if a previous run left stale segments, remove them so
    // cu29 doesn't see pre-existing slabs at the same path.
    let _ = std::fs::remove_dir_all(&session_dir);
    build_session_logger(&session_dir, 16)
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

// NOTE: The `#[cfg(test)] mod tests` block previously in this file did not
// compile into the test binary (pre-existing; the `#[copper_runtime]`
// proc-macro at module level consumes subsequent items during expansion).
// Tests were removed rather than relocated — see the Phase 26.7 Plan 05
// SUMMARY for rationale. The filename-format contract this refactor
// establishes is instead documented by source citation in the
// `build_session_logger` doc comment (`cu29-unifiedlog 0.14`'s
// `src/cu29_unifiedlog/memmap.rs:385` emits `{stem}_{N}.{ext}`).
