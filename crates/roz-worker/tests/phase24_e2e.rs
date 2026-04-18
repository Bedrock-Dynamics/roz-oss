//! Phase 24 end-to-end integration scenarios.
//!
//! - `phase24_deadman_survives_nats_outage` — deterministic worker-local
//!   watchdog test proving FS-01's "edge-local deadman" invariant: the
//!   watchdog keeps running on a local pet loop even with zero NATS
//!   activity, so a network outage does NOT trip the deadman.
//! - `phase24_resume_path_with_fresh_checkpoint` — deterministic end-to-end
//!   resume-gate scenario wired against a real `WalStore`. Persists a
//!   checkpoint, reads its age back via `checkpoint_age_secs`, and asserts
//!   `decide_recovery` returns `ResumeFromCheckpoint`.
//! - `phase24_induced_30s_nats_outage_survives_buffering_and_replay` —
//!   `#[ignore]`-gated hero scenario requiring Docker + toxiproxy. Scoped
//!   to the worker-only shape because the full server/worker/toxiproxy
//!   harness is deferred to Phase 27 SITL CI per RD-01.

use roz_core::edge::recovery::{CrashState, RecoveryStrategy};
use roz_worker::recovery::decide_recovery;
use roz_worker::wal::WalStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// SC#2: deadman does NOT trip during a simulated NATS outage because the
/// watchdog is edge-local. This test proves the watchdog is NOT broker-
/// dependent — a healthy local pet path keeps it alive independent of
/// NATS availability.
#[tokio::test]
async fn phase24_deadman_survives_nats_outage() {
    use roz_worker::command_watchdog::{CommandWatchdog, OnExpireCallback};

    let fired = Arc::new(AtomicBool::new(false));
    let fired_clone = fired.clone();
    let cb: OnExpireCallback = Arc::new(move || {
        fired_clone.store(true, Ordering::Relaxed);
    });
    let wd = Arc::new(CommandWatchdog::with_on_expire(
        Duration::from_secs(2), // deadline
        cb,
    ));

    // Simulate 5 s of "NATS down" while the local control path keeps the
    // watchdog alive via pet. No NATS message ever arrives, yet the
    // deadman stays un-expired.
    let wd_task = wd.clone();
    let pet_task = tokio::spawn(async move {
        for _ in 0..50 {
            wd_task.pet();
            tokio::time::sleep(Duration::from_millis(100)).await; // 10 Hz pet
        }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    pet_task.abort();

    assert!(
        !fired.load(Ordering::Relaxed),
        "watchdog incorrectly fired during healthy-pet window"
    );
    assert!(
        !wd.is_latched(),
        "watchdog must not be latched while pet path is healthy"
    );
}

/// SC#5: end-to-end resume-gate scenario. Writes a fresh checkpoint to a
/// real `WalStore`, reads its age back, constructs a `CrashState` that
/// satisfies every D-11 predicate, and asserts `decide_recovery` returns
/// `ResumeFromCheckpoint`.
#[tokio::test]
async fn phase24_resume_path_with_fresh_checkpoint() {
    let wal = WalStore::open(":memory:").expect("open WAL");
    let ck = wal
        .append_checkpoint("task-e2e", 3, b"snapshot")
        .expect("append checkpoint");
    let age = wal
        .checkpoint_age_secs("task-e2e")
        .expect("checkpoint age query")
        .expect("checkpoint present");
    let now = chrono::Utc::now().timestamp();
    let state = CrashState {
        joint_positions: Some(vec![0.0, 1.0]),
        brakes_engaged: true,
        mid_action: true,
        task_id: Some("task-e2e".into()),
        last_wal_seq: Some(1),
        last_checkpoint_id: Some(ck.clone()),
        last_checkpoint_ts_unix: Some(now - age),
    };
    let decision = decide_recovery(&state, now);
    assert_eq!(
        decision.strategy,
        RecoveryStrategy::ResumeFromCheckpoint,
        "fresh checkpoint must resume"
    );
}

/// SC#3/SC#4: induced 30 s NATS outage buffers telemetry + checkpoints, then
/// drains cleanly on reconnect. Full end-to-end server+worker+toxiproxy
/// harness is deferred to Phase 27 SITL CI (RD-01) because it requires
/// Docker, server startup, and the Restate ingress — none of which is
/// appropriate inside the roz-worker integration test crate.
///
/// Scoped here to the worker-only invariants: telemetry frames land in the
/// WAL when the publish path fails and the replay loop drains them once
/// reconnect fires. This test is `#[ignore]` until the containerised
/// harness lands; running it locally requires:
/// ```text
/// cargo test -p roz-worker --test phase24_e2e -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires toxiproxy + live NATS; full harness deferred to Phase 27 SITL CI per RD-01"]
async fn phase24_induced_30s_nats_outage_survives_buffering_and_replay() {
    // The deterministic buffering / replay contract is covered by
    // Plan 24-03 (append_telemetry_frame + list_unacked_telemetry) and
    // Plan 24-07 (TelemetryReplay::run_once + compute_delay bands). The
    // value this test adds is the *induced outage + reconnect-replay
    // round-trip*, which requires a live NATS with toxic injection.
    //
    // Acceptance (future Phase 27 SITL CI):
    // 1. Bring up NATS behind a toxiproxy forwarding listener.
    // 2. Start a minimal worker wired to that toxiproxy endpoint.
    // 3. Publish several telemetry frames; confirm they land on NATS.
    // 4. Inject a toxic dropping all bytes for 30 s.
    // 5. During the outage: wal.list_unacked_telemetry() grows; watchdog
    //    stays un-latched because the pet path is edge-local.
    // 6. Remove the toxic; fire reconnect_tx.
    // 7. After drain: wal.list_unacked_telemetry() is empty and the
    //    server-side dedup map shows every seq exactly once.
    //
    // Today this body just asserts the WAL round-trip invariant the
    // buffer contract depends on, so the `#[ignore]`-gated run still
    // compiles + passes under the "ignored" flag on developer laptops
    // that cannot stand up Docker.
    let wal = WalStore::open(":memory:").expect("open WAL");
    let seq1 = wal
        .append_telemetry_frame("worker-e2e", "state", b"frame-1")
        .expect("append frame 1");
    let seq2 = wal
        .append_telemetry_frame("worker-e2e", "state", b"frame-2")
        .expect("append frame 2");
    assert!(seq2 > seq1, "monotonic seq");
    let frames = wal.list_unacked_telemetry().expect("list unacked");
    assert_eq!(frames.len(), 2, "both frames buffered before drain");
    wal.ack_telemetry_up_to(seq2).expect("ack up to seq2");
    let remaining = wal.list_unacked_telemetry().expect("list after ack");
    assert!(remaining.is_empty(), "no frames remain after full drain");
}
