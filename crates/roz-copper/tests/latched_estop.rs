//! Phase 26.10 Plan 07 — FW-05c integration tests for the latched
//! e-stop state machine (IEC 60204-1 + EN ISO 13849-1).
//!
//! These integration tests exercise the publicly observable surface of
//! the latched e-stop:
//!
//! 1. The `LatchState` enum + transitions (covered exhaustively at
//!    crate/src/latch.rs unit tests; smoke-tested here).
//! 2. The new `ControllerCommand::AckEstop` and
//!    `ResumeAfterZeroVerified` variants thread through to the runtime
//!    side via the agent->Copper bridge (covered at
//!    crate/src/channels.rs unit tests; smoke-tested here).
//! 3. The published `ControllerState.latch_state` is observable from the
//!    agent side after Copper boot.
//!
//! Deeper loop-level behaviour (`build_per_channel_zero_frame`,
//! `assert_latch_estop`, `bump_zero_motion_tick`, the latched-tick gate
//! in `run_controller_loop_with_policy`, the rollback guard, and the
//! per-channel-kind zero policy with a `LoadedController` in scope) is
//! covered by the inline `#[cfg(test)] mod tests` block in
//! `crate/src/controller.rs`, which can reach `pub(crate)` helpers and
//! private fixtures (`prepared_test_controller`, `test_control_manifest`,
//! etc.) without crossing the integration-test boundary.

#![allow(
    clippy::pedantic,
    clippy::nursery,
    reason = "test-only style/complexity lints"
)]

use std::time::Duration;

use roz_copper::channels::ControllerCommand;
use roz_copper::handle::CopperHandle;
use roz_copper::latch::LatchState;

/// Boot Copper with no controller loaded; assert the agent-visible
/// `ControllerState.latch_state` defaults to `Run`.
#[tokio::test]
async fn boot_default_latch_state_is_run() {
    let handle = CopperHandle::spawn(1.5);
    // Allow the controller thread to publish at least one tick.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let state = handle.state().load_full();
    assert_eq!(
        state.latch_state,
        LatchState::Run,
        "Copper must boot in LatchState::Run when WAL is empty (no prior latched state)"
    );
    assert_eq!(state.zero_motion_tick_count, 0);
}

/// FW-05 H3: from a fresh-boot Run state, sending the existing
/// `ControllerCommand::Resume` MUST NOT advance the latch. The new
/// `ResumeAfterZeroVerified` variant from a non-`ZeroVerified` state
/// is also a no-op (IEC 60204-1: no auto-rearm). This test verifies
/// the agent->Copper bridge accepts the new variant without panicking
/// AND that the latch state stays at `Run`.
#[tokio::test]
async fn resume_after_zero_verified_from_run_is_noop() {
    let handle = CopperHandle::spawn(1.5);
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle
        .send(ControllerCommand::ResumeAfterZeroVerified)
        .await
        .expect("send ResumeAfterZeroVerified");
    handle.send(ControllerCommand::Resume).await.expect("send Resume");
    handle.send(ControllerCommand::AckEstop).await.expect("send AckEstop");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let state = handle.state().load_full();
    assert_eq!(
        state.latch_state,
        LatchState::Run,
        "Resume / ResumeAfterZeroVerified / AckEstop are all no-ops from Run; latch must remain Run"
    );
}

/// Smoke-test: the existing `ControllerCommand::Resume` variant
/// continues to be accepted by the bridge after the FW-05 changes
/// without panicking. (Full Resume semantic preservation is covered by
/// the inline test `controller_command_existing_resume_unchanged` in
/// `crate/src/channels.rs`.)
#[tokio::test]
async fn existing_resume_command_still_accepted() {
    let handle = CopperHandle::spawn(1.5);
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.send(ControllerCommand::Resume).await.expect("send Resume");
    tokio::time::sleep(Duration::from_millis(50)).await;
    // No assertion on Resume's runtime effect (it has no controller to
    // resume) — just that the channel survives the post-FW-05 wiring.
    let state = handle.state().load_full();
    assert_eq!(state.latch_state, LatchState::Run);
}
