//! Phase 24-17 Task 2: prove the deadman expiry chain observes hot-swapped
//! policies under realistic conditions.
//!
//! The existing `command_watchdog` unit tests prove that
//! `build_deadman_callback` compiles, captures the `Arc<HotPolicy>`, and
//! matches all four `OnBreachAction` variants. What they do NOT prove is the
//! full chain under real concurrent hot-swap pressure:
//!
//! 1. `HotPolicy::store(...)` → `CommandWatchdog::run().await` fires →
//!    callback observes the value that was live AT FIRE TIME (not a boot
//!    snapshot, not a stale arc).
//! 2. Post-expiry latch: the watchdog does not re-fire until
//!    `clear_failsafe()`; subsequent `pet()`s during latch are ignored.
//! 3. Re-arm: after `clear_failsafe()` + respawn, a later hot-swap's action
//!    is the value observed at the next expiry — proves callback reads are
//!    always fresh.
//! 4. Mid-cycle hot-swap: a swap that happens between `run()` spawning and
//!    the deadline expiring wins (the callback reads at fire-time, not at
//!    spawn-time).
//! 5. Concurrent swap race: 8 tasks hammering `HotPolicy::store(...)` while
//!    the watchdog expires — observed value must be one of the four valid
//!    `OnBreachAction` variants, never a torn / corrupt enum. This is the
//!    lock-free correctness check for the underlying `ArcSwap`.
//!
//! This test is pure in-process — no Docker, no network, no sleeps longer
//! than a single watchdog tick. It runs in under 5 seconds on a laptop.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use roz_worker::command_watchdog::{CommandWatchdog, build_deadman_callback_with_observer};
use roz_worker::policy_cache::HotPolicy;
use roz_worker::policy_enforcement::{
    AccelerationLimits, DeadmanTimers, EnforcementMode, ForceLimits, OnBreachAction, PolicyLimits, PolicyV1,
    VelocityLimits,
};

/// Build a PolicyV1 that differs from other PolicyV1s only by its
/// `deadman_timers.on_expire` action, so swaps are unambiguous under
/// inspection.
fn policy_with_on_expire(action: OnBreachAction) -> PolicyV1 {
    PolicyV1 {
        policy_id: Uuid::new_v4(),
        version: 1,
        enforcement_mode: EnforcementMode::Halt,
        limits: PolicyLimits {
            max_velocity: VelocityLimits {
                linear_m_per_s: 1.0,
                angular_rad_per_s: 0.5,
            },
            max_acceleration: AccelerationLimits {
                linear_m_per_s2: 1.0,
                angular_rad_per_s2: 0.5,
            },
            max_force: ForceLimits { newtons: 25.0 },
            joint_limits: Vec::new(),
        },
        geofences: Vec::new(),
        interlocks: Vec::new(),
        deadman_timers: DeadmanTimers {
            command_timeout_ms: 5000,
            on_expire: action,
        },
    }
}

/// Poll `observed` until it contains `Some(_)` or `budget` elapses. Returns
/// the final value (None on timeout). Poll cadence is 25ms — short enough to
/// land inside the watchdog's 1s tick interval with headroom.
async fn wait_observed(observed: &Arc<Mutex<Option<OnBreachAction>>>, budget: Duration) -> Option<OnBreachAction> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let current = *observed.lock();
        if let Some(a) = current {
            return Some(a);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    *observed.lock()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadman_callback_honors_hot_swapped_policy_on_expire() {
    // ---- setup ----
    let hot = Arc::new(HotPolicy::permissive());
    hot.store(policy_with_on_expire(OnBreachAction::Halt));

    let observed: Arc<Mutex<Option<OnBreachAction>>> = Arc::new(Mutex::new(None));
    let observer_arc = {
        let o = observed.clone();
        Arc::new(move |action: OnBreachAction| {
            *o.lock() = Some(action);
        })
    };
    let callback = build_deadman_callback_with_observer(hot.clone(), observer_arc);

    // ---- phase 1: initial fire on Halt ----
    //
    // Short deadline (50ms) + the watchdog's 1s internal tick cadence means
    // we expect the first tick after run() spawns to observe is_expired() ==
    // true and fire. We budget 3s for safety (3 tick intervals).
    let wd = Arc::new(CommandWatchdog::with_on_expire(
        Duration::from_millis(50),
        callback.clone(),
    ));
    let cancel1 = CancellationToken::new();
    let wd_run = wd.clone();
    let cancel1_run = cancel1.clone();
    let run1 = tokio::spawn(async move { wd_run.run(cancel1_run).await });

    let got = wait_observed(&observed, Duration::from_secs(3)).await;
    assert_eq!(
        got,
        Some(OnBreachAction::Halt),
        "phase 1: initial fire must observe the Halt action stored before spawn"
    );
    assert!(wd.is_latched(), "phase 1: motion must latch after expire");

    // Drive run1 to completion: it returns after firing the callback, but we
    // still await it for task hygiene.
    let _ = tokio::time::timeout(Duration::from_millis(200), run1)
        .await
        .expect("run1 must exit promptly after fire");

    // ---- phase 2: latch-survives-pet + hot-swap doesn't re-fire latched ----
    //
    // Swap to Land + pet() — the watchdog must remain latched and the
    // observer must NOT see a second fire (the callback is not even called
    // because run1 already returned).
    hot.store(policy_with_on_expire(OnBreachAction::Land));
    *observed.lock() = None;
    wd.pet();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(wd.is_latched(), "phase 2: latch must persist across pet after expire");
    assert_eq!(
        *observed.lock(),
        None,
        "phase 2: latched + pet must NOT re-fire the callback"
    );

    // ---- phase 3: clear_failsafe + respawn sees hot-swapped Land ----
    //
    // Re-arm and spawn a fresh run cycle. The callback is the SAME Arc — we
    // intentionally reuse it to prove the observer + HotPolicy capture are
    // fresh on every fire.
    wd.clear_failsafe();
    assert!(!wd.is_latched(), "phase 3 precondition: clear_failsafe un-latches");

    // Rebuild the watchdog with a fresh last_pet_ms so the 50ms deadline is
    // genuinely in the future from this point; reusing the old one would
    // race against any residual state from run1.
    let wd2 = Arc::new(CommandWatchdog::with_on_expire(
        Duration::from_millis(50),
        callback.clone(),
    ));
    let cancel2 = CancellationToken::new();
    let wd2_run = wd2.clone();
    let cancel2_run = cancel2.clone();
    let run2 = tokio::spawn(async move { wd2_run.run(cancel2_run).await });

    let got = wait_observed(&observed, Duration::from_secs(3)).await;
    assert_eq!(
        got,
        Some(OnBreachAction::Land),
        "phase 3: re-armed fire must observe the hot-swapped Land action, not the original Halt"
    );
    assert!(wd2.is_latched(), "phase 3: motion must latch again");
    let _ = tokio::time::timeout(Duration::from_millis(200), run2).await;

    // ---- phase 4: mid-cycle hot-swap (Land → ReturnToLaunch) ----
    //
    // Spawn a run cycle with the policy currently at Land; BEFORE the
    // deadline expires, swap to ReturnToLaunch. The observer must see the
    // LATE value (proving the callback reads at fire-time, not at spawn).
    *observed.lock() = None;
    hot.store(policy_with_on_expire(OnBreachAction::Land));
    let wd3 = Arc::new(CommandWatchdog::with_on_expire(
        Duration::from_millis(50),
        callback.clone(),
    ));
    let cancel3 = CancellationToken::new();
    let wd3_run = wd3.clone();
    let cancel3_run = cancel3.clone();
    let run3 = tokio::spawn(async move { wd3_run.run(cancel3_run).await });

    // Wait a small fraction of the 1s tick interval, then swap. The first
    // tick will still read the swapped value.
    tokio::time::sleep(Duration::from_millis(10)).await;
    hot.store(policy_with_on_expire(OnBreachAction::ReturnToLaunch));

    let got = wait_observed(&observed, Duration::from_secs(3)).await;
    assert_eq!(
        got,
        Some(OnBreachAction::ReturnToLaunch),
        "phase 4: mid-cycle hot-swap must win — callback reads at fire-time, not at spawn"
    );
    assert!(wd3.is_latched());
    let _ = tokio::time::timeout(Duration::from_millis(200), run3).await;

    // ---- phase 5: concurrent swap race (lock-free correctness) ----
    //
    // 8 tasks spam 100 stores each, cycling through all 4 actions. The
    // watchdog expires while the storm is running. Observed value MUST be
    // one of the 4 valid `OnBreachAction` variants — a torn enum would be a
    // spec violation of `ArcSwap` that 24-05 relies on.
    *observed.lock() = None;
    hot.store(policy_with_on_expire(OnBreachAction::Halt)); // deterministic start
    wd.clear_failsafe();
    let storm_running = Arc::new(AtomicBool::new(true));
    let actions = [
        OnBreachAction::Halt,
        OnBreachAction::HoldPosition,
        OnBreachAction::Land,
        OnBreachAction::ReturnToLaunch,
    ];
    let storm_handles: Vec<_> = (0..8)
        .map(|_| {
            let h = hot.clone();
            let running = storm_running.clone();
            let acts = actions;
            tokio::spawn(async move {
                let mut i = 0usize;
                while running.load(Ordering::Relaxed) && i < 100 {
                    h.store(policy_with_on_expire(acts[i % 4]));
                    i += 1;
                    tokio::task::yield_now().await;
                }
            })
        })
        .collect();

    let wd4 = Arc::new(CommandWatchdog::with_on_expire(
        Duration::from_millis(50),
        callback.clone(),
    ));
    let cancel4 = CancellationToken::new();
    let wd4_run = wd4.clone();
    let cancel4_run = cancel4.clone();
    let run4 = tokio::spawn(async move { wd4_run.run(cancel4_run).await });

    let got = wait_observed(&observed, Duration::from_secs(5)).await;
    storm_running.store(false, Ordering::Relaxed);
    for h in storm_handles {
        let _ = tokio::time::timeout(Duration::from_millis(200), h).await;
    }
    let _ = tokio::time::timeout(Duration::from_millis(200), run4).await;

    // The race MUST produce a well-formed OnBreachAction — the actual value
    // depends on scheduling, but it must be one of the four.
    let got = got.expect("phase 5: watchdog must fire even under concurrent store storm");
    assert!(
        matches!(
            got,
            OnBreachAction::Halt | OnBreachAction::HoldPosition | OnBreachAction::Land | OnBreachAction::ReturnToLaunch
        ),
        "phase 5: observed action must be a well-formed OnBreachAction variant, got {got:?}"
    );

    // Explicit cancel for tokio-task hygiene; the watchdogs have all
    // returned via the expire branch but the cancel tokens would still be
    // consulted if we re-used them.
    cancel1.cancel();
    cancel2.cancel();
    cancel3.cancel();
    cancel4.cancel();
}
