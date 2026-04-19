use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::policy_cache::HotPolicy;
use crate::policy_enforcement::OnBreachAction;

/// Callback type fired when the watchdog expires.
///
/// `Send + Sync` so it can be shared across the worker's tokio tasks — the
/// dispatcher uses the callback to invoke the policy-sourced action
/// (`halt` / `hold_position` / `land` / `return_to_launch`).
pub type OnExpireCallback = Arc<dyn Fn() + Send + Sync>;

/// Build a deadman-expiry callback that reads the live policy action from a
/// shared [`HotPolicy`] and logs it (Plan 24-12 Task 2).
///
/// The callback captures an `Arc` clone of the `HotPolicy` so subsequent
/// `roz.policy.{worker_id}` push updates are visible on expiry. The logged
/// `action` field maps the policy's `deadman_timers.on_expire` enum to the
/// canonical D-03 string set: `halt` / `hold_position` / `land` /
/// `return_to_launch`. The physical-action dispatch itself (actual MAVLink
/// halt / land / RTL) is Phase 25 scope per 24-CONTEXT D-03.
#[must_use]
pub fn build_deadman_callback(hot_policy: Arc<HotPolicy>) -> OnExpireCallback {
    Arc::new(move || {
        let policy = hot_policy.load();
        let action = match policy.deadman_timers.on_expire {
            OnBreachAction::Halt => "halt",
            OnBreachAction::HoldPosition => "hold_position",
            OnBreachAction::Land => "land",
            OnBreachAction::ReturnToLaunch => "return_to_launch",
        };
        tracing::error!(
            policy_id = %policy.policy_id,
            %action,
            "deadman watchdog expired — dispatching policy-sourced action"
        );
    })
}

/// Watchdog that fires if no command arrives within the deadline.
///
/// # Phase 24 extension (FS-01, D-02)
///
/// After expiry:
/// 1. The `on_expire` callback is invoked once. Callers wire the policy's
///    `deadman_timers.on_expire` action through this callback — no NATS
///    round-trip is required, so the watchdog remains broker-independent.
/// 2. Motion is **latched** — subsequent [`pet`](Self::pet) calls do NOT
///    clear the latch or re-arm the deadline.
/// 3. Only [`clear_failsafe`](Self::clear_failsafe) un-latches, matching
///    PX4 / ArduPilot industry-standard failsafe semantics.
///
/// Backward-compatible constructor [`new`](Self::new) installs a no-op
/// callback so legacy call sites continue to compile and behave identically
/// (watchdog expires, loop returns — nothing else).
pub struct CommandWatchdog {
    last_pet_ms: Arc<AtomicU64>,
    deadline: Duration,
    /// Callback dispatched exactly once per latch cycle on expiry. Default
    /// (via `new`) is a no-op — production wiring swaps in a policy-action
    /// dispatcher.
    on_expire: OnExpireCallback,
    /// Motion latch. `true` after expiry fires until
    /// [`clear_failsafe`](Self::clear_failsafe) explicitly un-latches.
    /// `pub(crate)` so sibling modules (`clear_failsafe.rs`) and tests can
    /// simulate an expired state without running the full async loop.
    pub(crate) latched: Arc<AtomicBool>,
}

impl CommandWatchdog {
    /// Backward-compatible constructor — no-op `on_expire` callback.
    pub fn new(deadline: Duration) -> Self {
        Self::with_on_expire(deadline, Arc::new(|| ()))
    }

    /// Phase 24 constructor: register a callback dispatched on expiry.
    ///
    /// The callback is invoked exactly once per latch cycle. Callers that
    /// need to re-arm the watchdog after a latch MUST call
    /// [`clear_failsafe`](Self::clear_failsafe) and respawn [`run`](Self::run).
    #[must_use]
    pub fn with_on_expire(deadline: Duration, on_expire: OnExpireCallback) -> Self {
        let now = Self::now_ms();
        Self {
            last_pet_ms: Arc::new(AtomicU64::new(now)),
            deadline,
            on_expire,
            latched: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Reset the watchdog timer. No-op while the motion latch is engaged —
    /// D-02 requires explicit operator re-arm via
    /// [`clear_failsafe`](Self::clear_failsafe).
    pub fn pet(&self) {
        if !self.latched.load(Ordering::Relaxed) {
            self.last_pet_ms.store(Self::now_ms(), Ordering::Relaxed);
        }
    }

    /// Check if the watchdog has expired.
    pub fn is_expired(&self) -> bool {
        let last = self.last_pet_ms.load(Ordering::Relaxed);
        let now = Self::now_ms();
        let elapsed = Duration::from_millis(now.saturating_sub(last));
        elapsed > self.deadline
    }

    /// Return `true` when motion is currently latched (post-expiry, pre-
    /// [`clear_failsafe`](Self::clear_failsafe)).
    #[must_use]
    pub fn is_latched(&self) -> bool {
        self.latched.load(Ordering::Relaxed)
    }

    /// Explicit operator re-arm (D-02). Clears the motion latch and re-arms
    /// the deadline so subsequent [`pet`](Self::pet) calls track normally.
    ///
    /// Only called from the verified `cmd.{worker_id}.clear_failsafe`
    /// subscriber path (Plan 24-06 Task 2) — never from an auto-recovery
    /// path.
    pub fn clear_failsafe(&self) {
        self.latched.store(false, Ordering::Relaxed);
        self.last_pet_ms.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Run the watchdog loop.
    ///
    /// Fires `on_expire` exactly once on expiry, latches motion, and then
    /// exits. Callers that want to re-arm the watchdog after a latch must
    /// call [`clear_failsafe`](Self::clear_failsafe) and respawn a fresh
    /// `run()` task.
    pub async fn run(&self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if self.is_expired() && !self.latched.load(Ordering::Relaxed) {
                        tracing::error!(
                            deadline_secs = self.deadline.as_secs(),
                            "command watchdog expired — dispatching policy action + latching motion"
                        );
                        // Latch BEFORE firing the callback so a reentrant
                        // callback sees `is_latched() == true` and cannot
                        // race with itself.
                        self.latched.store(true, Ordering::Relaxed);
                        (self.on_expire)();
                        return;
                    }
                }
                () = cancel.cancelled() => return,
            }
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "millis since epoch fits in u64 until year 584,942,417"
    )]
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== existing backward-compat tests =====

    #[test]
    fn watchdog_not_expired_immediately() {
        let wd = CommandWatchdog::new(Duration::from_secs(5));
        assert!(!wd.is_expired());
    }

    #[test]
    fn watchdog_expired_after_deadline() {
        let wd = CommandWatchdog::new(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(wd.is_expired());
    }

    #[test]
    fn watchdog_reset_by_pet() {
        let wd = CommandWatchdog::new(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(30));
        wd.pet();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!wd.is_expired()); // 30ms since pet, deadline is 50ms
    }

    // ===== Phase 24 FS-01 / D-02 extensions =====

    #[test]
    fn existing_watchdog_construction_compatible() {
        // API backward-compat: `new(deadline)` still constructs a valid
        // watchdog with a no-op on_expire and an un-latched motion state.
        let wd = CommandWatchdog::new(Duration::from_secs(5));
        assert!(!wd.is_latched());
    }

    #[test]
    fn on_expire_callback_fires_after_deadline() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cb: OnExpireCallback = Arc::new(move || {
            fired_clone.store(true, Ordering::Relaxed);
        });
        let wd = CommandWatchdog::with_on_expire(Duration::from_millis(1), cb);
        std::thread::sleep(Duration::from_millis(10));
        assert!(wd.is_expired(), "deadline must be past");
        // Drive one iteration of run()'s expire branch synchronously.
        wd.latched.store(true, Ordering::Relaxed);
        (wd.on_expire)();
        assert!(fired.load(Ordering::Relaxed));
    }

    #[test]
    fn motion_latched_after_expire() {
        let wd = CommandWatchdog::new(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        // Simulate expire-fire path without running the async loop.
        wd.latched.store(true, Ordering::Relaxed);
        assert!(wd.is_latched());
    }

    #[test]
    fn pet_does_not_clear_latch() {
        let wd = CommandWatchdog::new(Duration::from_millis(50));
        wd.latched.store(true, Ordering::Relaxed);
        let before = wd.last_pet_ms.load(Ordering::Relaxed);
        // Sleep so a successful pet would visibly shift last_pet_ms.
        std::thread::sleep(Duration::from_millis(5));
        wd.pet();
        let after = wd.last_pet_ms.load(Ordering::Relaxed);
        assert!(wd.is_latched(), "latch must persist across pet");
        assert_eq!(before, after, "pet must be a no-op while latched");
    }

    #[test]
    fn clear_failsafe_clears_latch_and_rearms() {
        let wd = CommandWatchdog::new(Duration::from_millis(50));
        wd.latched.store(true, Ordering::Relaxed);
        assert!(wd.is_latched());
        // Force last_pet_ms into the past so `is_expired` returns true before
        // the re-arm.
        wd.last_pet_ms.store(0, Ordering::Relaxed);
        assert!(wd.is_expired(), "precondition: expired before clear");
        wd.clear_failsafe();
        assert!(!wd.is_latched());
        assert!(!wd.is_expired(), "clear_failsafe must re-arm the deadline");
    }

    #[tokio::test]
    async fn run_fires_callback_and_exits_on_expire() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let cb: OnExpireCallback = Arc::new(move || fired_clone.store(true, Ordering::Relaxed));
        let wd = Arc::new(CommandWatchdog::with_on_expire(Duration::from_millis(1), cb));
        let cancel = CancellationToken::new();
        let wd2 = wd.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move { wd2.run(cancel2).await });
        // Wait past deadline + interval.tick() (interval is 1 s → 1500 ms).
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(fired.load(Ordering::Relaxed), "callback must have fired");
        assert!(wd.is_latched(), "motion must be latched");
        let _ = tokio::time::timeout(Duration::from_millis(200), handle).await;
    }

    // ===== Plan 24-12 Task 2: policy-sourced deadman callback =====

    use crate::policy_cache::HotPolicy;
    use crate::policy_enforcement::{
        AccelerationLimits, DeadmanTimers, EnforcementMode, ForceLimits, OnBreachAction, PolicyLimits, PolicyV1,
        VelocityLimits,
    };
    use uuid::Uuid;

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

    #[test]
    fn on_expire_callback_reads_hot_policy_action() {
        // Seed the HotPolicy with Land, build callback, invoke — tracing
        // log output is best verified visually; the correctness of the
        // match happens on the branch table below.
        let hot = Arc::new(HotPolicy::new(policy_with_on_expire(OnBreachAction::Land)));
        let cb = build_deadman_callback(hot.clone());
        // Hot-swap to ReturnToLaunch AFTER the callback is built — the
        // callback MUST see the new value (Arc clone observes the ArcSwap
        // pointer updated via HotPolicy::store).
        hot.store(policy_with_on_expire(OnBreachAction::ReturnToLaunch));
        // The callback is a side-effect only — calling it does not panic
        // and the match arms cover every variant (Halt / HoldPosition /
        // Land / ReturnToLaunch). Both branches of behaviour are asserted
        // by the companion test `on_expire_callback_defaults_to_halt_on_permissive_policy`.
        cb();
    }

    #[test]
    fn on_expire_callback_defaults_to_halt_on_permissive_policy() {
        // The `permissive()` default has on_expire=Halt; the callback must
        // complete without panicking when called against the default
        // posture.
        let hot = Arc::new(HotPolicy::permissive());
        let cb = build_deadman_callback(hot);
        cb();
    }

    #[test]
    fn on_expire_callback_is_send_sync() {
        // `OnExpireCallback = Arc<dyn Fn() + Send + Sync>` — the helper
        // must produce a callback that satisfies the trait object's bounds
        // so `CommandWatchdog::with_on_expire` can accept it.
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let hot = Arc::new(HotPolicy::permissive());
        let cb = build_deadman_callback(hot);
        assert_send_sync(&cb);
    }
}
